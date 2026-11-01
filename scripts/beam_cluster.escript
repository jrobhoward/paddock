%% -*- erlang -*-
%%! -noshell
%%
%% BEAM reference figures between nodes for docs/BASELINE.md: the runs
%% bulkhead-host's cluster_throughput makes, between two Erlang nodes over
%% distributed Erlang. Node 2 holds 16 gen_servers; node 1 runs the
%% figures, against 16 gen_servers of its own on one node and node 2's on
%% two nodes. Two OS processes on one machine, or on two machines:
%%
%%   escript scripts/beam_cluster.escript serve bench2@ADDR2 COOKIE
%%   escript scripts/beam_cluster.escript connect bench1@ADDR1 COOKIE bench2@ADDR2
%%
%% Schedulers come from ERL_FLAGS ('+S 4:4' beside 4 tokio workers). Over
%% TLS distribution, as Bulkhead's links are mutual TLS, with the options
%% file scripts/beam_cluster_tls.sh writes, on both nodes:
%%
%%   ERL_FLAGS='+S 4:4 -proto_dist inet_tls -ssl_dist_optfile DIR/ssl_dist.conf' escript ...
%%
%% Distribution listens on ports 9100 to 9109, beside epmd's 4369, which
%% the script starts if it is not running.
%%
%% From a plain process on node 1, as cluster_throughput's host calls:
%% - Sequential calls: {echo, <<"x">>}, one at a time: latency.
%% - Concurrent calls: 16 and 64 callers over 16 servers for 5 s: calls/s.
%% - Casts: 400,000 `count` casts over 16 servers, then a call to each to
%%   read its count: casts handled per second. Erlang drops none: a sender
%%   to a busy distribution port waits.
%% - Payloads: `mirror` calls with 1 KiB and 64 KiB binaries, 16 callers.
%% From gen_servers on node 1, as cluster_throughput's instances call:
%% - Sequential calls: one server calling another one at a time.
%% - Concurrent calls: 16 servers each calling its own target.
%% - Casts: 16 servers each casting 25,000 `count` to its own target, then
%%   calling it behind them; then the targets' counts read.
-mode(compile).
-behaviour(gen_server).
-export([init/1, handle_call/3, handle_cast/2, handle_info/2]).

-define(SERVERS, 16).
-define(SEQUENTIAL, 20000).
-define(TIMED_MS, 5000).
-define(CASTS, 400000).
-define(APP_CALLS, 5000).
-define(APP_CASTS, 25000).
-define(TIMEOUT, 30000).

main(["serve", Name, Cookie]) ->
    start_node(Name, Cookie),
    Servers = servers(),
    [register(list_to_atom("msgbench" ++ integer_to_list(I)), S)
     || {I, S} <- lists:zip(lists:seq(1, ?SERVERS), Servers)],
    io:format("beam_cluster: ~s, ~b gen_servers registered msgbench1 to msgbench~b; ~s~n",
              [node(), ?SERVERS, ?SERVERS, versions()]),
    receive stop -> ok end;
main(["connect", Name, Cookie, Peer]) ->
    start_node(Name, Cookie),
    PeerNode = list_to_atom(Peer),
    connected(PeerNode, 100),
    Remote = [rpc:call(PeerNode, erlang, whereis,
                       [list_to_atom("msgbench" ++ integer_to_list(I))])
              || I <- lists:seq(1, ?SERVERS)],
    true = lists:all(fun is_pid/1, Remote),
    io:format("beam_cluster: ~s to ~s (~s distribution), ~b gen_servers per node; ~s~n",
              [node(), PeerNode, proto(), ?SERVERS, versions()]),
    run_in(fun() -> bench(servers(), servers(), Remote) end),
    halt(0);
main(_) ->
    io:format("usage: beam_cluster.escript serve NAME@HOST COOKIE | "
              "connect NAME@HOST COOKIE PEER@HOST~n"),
    halt(2).

versions() ->
    io_lib:format("erts ~s, OTP ~s, ~b schedulers",
                  [erlang:system_info(version), erlang:system_info(otp_release),
                   erlang:system_info(schedulers_online)]).

proto() ->
    case init:get_argument(proto_dist) of
        {ok, [[P]]} -> P;
        _ -> "inet_tcp"
    end.

start_node(Name, Cookie) ->
    Epmd = filename:join([code:root_dir(), "erts-" ++ erlang:system_info(version), "bin", "epmd"]),
    os:cmd(case filelib:is_file(Epmd) of true -> Epmd; false -> "epmd" end ++ " -daemon"),
    timer:sleep(200),
    application:set_env(kernel, inet_dist_listen_min, 9100),
    application:set_env(kernel, inet_dist_listen_max, 9109),
    {ok, _} = net_kernel:start([list_to_atom(Name), longnames]),
    erlang:set_cookie(node(), list_to_atom(Cookie)).

connected(Peer, 0) ->
    io:format("cannot reach ~s~n", [Peer]),
    halt(1);
connected(Peer, Tries) ->
    case net_adm:ping(Peer) of
        pong -> ok;
        pang -> timer:sleep(100), connected(Peer, Tries - 1)
    end.

run_in(F) ->
    {Pid, Ref} = spawn_monitor(F),
    receive {'DOWN', Ref, process, Pid, normal} -> ok;
            {'DOWN', Ref, process, Pid, Why} -> io:format("failed: ~p~n", [Why]), halt(1)
    end.

servers() ->
    [element(2, gen_server:start(?MODULE, [], [])) || _ <- lists:seq(1, ?SERVERS)].

init([]) -> {ok, 0}.
handle_call({echo, B}, _From, N) -> {reply, B, N};
handle_call({mirror, B}, _From, N) -> {reply, B, N};
handle_call(count, _From, N) -> {reply, N + 1, N + 1};
handle_call({calls, To, Calls}, _From, N) -> {reply, calls(To, Calls), N};
handle_call({casts, To, Casts}, _From, N) -> {reply, casts(To, Casts), N}.
handle_cast(count, N) -> {noreply, N + 1}.
handle_info(_, N) -> {noreply, N}.

us(Native) -> erlang:convert_time_unit(Native, native, nanosecond) / 1000.

quantile(Sorted, Q) -> lists:nth(round((length(Sorted) - 1) * Q) + 1, Sorted).

%% `Calls` calls of {echo, <<"x">>} to `To`, one at a time: total, p50 and
%% p99 in native time units.
calls(To, Calls) ->
    T0 = erlang:monotonic_time(),
    Samples = [begin
                   T = erlang:monotonic_time(),
                   <<"x">> = gen_server:call(To, {echo, <<"x">>}, ?TIMEOUT),
                   erlang:monotonic_time() - T
               end || _ <- lists:seq(1, Calls)],
    Total = erlang:monotonic_time() - T0,
    Sorted = lists:sort(Samples),
    {Total, quantile(Sorted, 0.5), quantile(Sorted, 0.99)}.

%% `Casts` casts of `count` to `To`, then a call behind them.
casts(To, Casts) ->
    T0 = erlang:monotonic_time(),
    [gen_server:cast(To, count) || _ <- lists:seq(1, Casts)],
    <<"x">> = gen_server:call(To, {echo, <<"x">>}, ?TIMEOUT),
    erlang:monotonic_time() - T0.

bench(Local, Targets, Remote) ->
    sequential(hd(Local), "one node"),
    sequential(hd(Remote), "two nodes"),
    lists:foreach(fun(Callers) ->
        One = concurrent(Local, Callers, {echo, <<"x">>}),
        Two = concurrent(Remote, Callers, {echo, <<"x">>}),
        io:format("~b callers: one node ~b calls/s, two nodes ~b calls/s~n",
                  [Callers, round(One), round(Two)])
    end, [16, 64]),
    host_casts(Local, "one node"),
    host_casts(Remote, "two nodes"),
    lists:foreach(fun(Size) ->
        Msg = {mirror, binary:copy(<<"x">>, Size)},
        One = concurrent(Local, 16, Msg),
        Two = concurrent(Remote, 16, Msg),
        MiB = fun(C) -> C * Size / (1 bsl 20) end,
        io:format("~b KiB bodies, 16 callers: one node ~b calls/s (~b MiB/s each way), "
                  "two nodes ~b calls/s (~b MiB/s each way)~n",
                  [Size div 1024, round(One), round(MiB(One)), round(Two), round(MiB(Two))])
    end, [1024, 65536]),
    app_sequential(hd(Local), hd(Targets), "one node"),
    app_sequential(hd(Local), hd(Remote), "two nodes"),
    One = app_concurrent(Local, Targets),
    Two = app_concurrent(Local, Remote),
    io:format("from servers, ~b callers: one node ~b calls/s, two nodes ~b calls/s~n",
              [length(Local), round(One), round(Two)]),
    app_casts(Local, Targets, "one node"),
    app_casts(Local, Remote, "two nodes").

sequential(To, Label) ->
    calls(To, 1000),
    {Total, P50, P99} = calls(To, ?SEQUENTIAL),
    io:format("~s: sequential calls ~b/s, p50 ~.1f us, p99 ~.1f us~n",
              [Label, round(?SEQUENTIAL / us(Total) * 1.0e6), us(P50), us(P99)]).

%% `Callers` processes calling `Msg` on `Targets` in turn for ?TIMED_MS:
%% calls per second.
concurrent(Targets, Callers, Msg) ->
    Me = self(),
    Deadline = erlang:monotonic_time(millisecond) + ?TIMED_MS,
    N = length(Targets),
    Pids = [spawn_link(fun() -> Me ! {done, self(), caller(Targets, N, C, Msg, Deadline, 0)} end)
            || C <- lists:seq(0, Callers - 1)],
    Done = lists:sum([receive {done, P, D} -> D end || P <- Pids]),
    Done / (?TIMED_MS / 1000).

caller(Targets, N, I, Msg, Deadline, Done) ->
    case erlang:monotonic_time(millisecond) < Deadline of
        false -> Done;
        true ->
            _ = gen_server:call(lists:nth(I rem N + 1, Targets), Msg, ?TIMEOUT),
            caller(Targets, N, I + 1, Msg, Deadline, Done + 1)
    end.

count(To) -> gen_server:call(To, count, ?TIMEOUT).

host_casts(Targets, Label) ->
    Before = lists:sum([count(T) || T <- Targets]),
    N = length(Targets),
    T0 = erlang:monotonic_time(),
    cast_loop(list_to_tuple(Targets), N, 0),
    After = lists:sum([count(T) || T <- Targets]),
    Handled = After - Before - N,
    Secs = us(erlang:monotonic_time() - T0) / 1.0e6,
    io:format("~s: casts ~b handled/s (~b of ~b handled)~n",
              [Label, round(Handled / Secs), Handled, ?CASTS]).

cast_loop(_, _, ?CASTS) -> ok;
cast_loop(Targets, N, I) ->
    gen_server:cast(element(I rem N + 1, Targets), count),
    cast_loop(Targets, N, I + 1).

app_sequential(From, To, Label) ->
    gen_server:call(From, {calls, To, 1000}, infinity),
    {Total, P50, P99} = gen_server:call(From, {calls, To, ?SEQUENTIAL}, infinity),
    io:format("~s, from a server: sequential calls ~b/s, p50 ~.1f us, p99 ~.1f us~n",
              [Label, round(?SEQUENTIAL / us(Total) * 1.0e6), us(P50), us(P99)]).

%% Each of `From` asking its server to run `Msg(To)` at once: the wall time.
all_at_once(From, To, Msg) ->
    Me = self(),
    T0 = erlang:monotonic_time(),
    Pids = [spawn_link(fun() -> Me ! {done, self(), gen_server:call(F, Msg(T), infinity)} end)
            || {F, T} <- lists:zip(From, To)],
    [receive {done, P, _} -> ok end || P <- Pids],
    us(erlang:monotonic_time() - T0) / 1.0e6.

app_concurrent(From, To) ->
    Secs = all_at_once(From, To, fun(T) -> {calls, T, ?APP_CALLS} end),
    length(From) * ?APP_CALLS / Secs.

app_casts(From, To, Label) ->
    Before = lists:sum([count(T) || T <- To]),
    Secs = all_at_once(From, To, fun(T) -> {casts, T, ?APP_CASTS} end),
    After = lists:sum([count(T) || T <- To]),
    Handled = After - Before - length(To),
    io:format("~s, from servers: casts ~b handled/s (~b of ~b handled)~n",
              [Label, round(Handled / Secs), Handled, length(From) * ?APP_CASTS]).

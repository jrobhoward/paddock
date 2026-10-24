#!/usr/bin/env escript
%% -*- erlang -*-
%%! -noshell
%%
%% BEAM reference figures for docs/BASELINE.md: the shapes `baseline` and
%% `timer_cost` measure in Paddock, as Erlang/OTP does them. A gen_server
%% call and cast, spawning a gen_server to its first reply, idle memory per
%% process, and how late erlang:send_after messages are handled, idle and
%% under load. The load is timer_cost's: 64 clients calling 16 gen_servers
%% back to back, each call about 31 us of integer work (calibrated at the
%% start). Schedulers come from ERL_FLAGS:
%%
%%   ERL_FLAGS='+S 1:1' escript scripts/beam_baseline.escript
%%   ERL_FLAGS='+S 4:4' escript scripts/beam_baseline.escript
-mode(compile).
-behaviour(gen_server).
-export([init/1, handle_call/3, handle_cast/2, handle_info/2]).

-define(CALLS, 100000).
-define(CASTS, 200000).
-define(SPAWNS, 1000).
-define(TIMERS, 100).
-define(ROUNDS, 10).
-define(LOAD_SERVERS, 16).
-define(LOAD_CLIENTS, 64).
-define(BUSY_US, 31).

main(_) ->
    io:format("erts ~s, OTP ~s, schedulers: ~p~n",
              [erlang:system_info(version), erlang:system_info(otp_release),
               erlang:system_info(schedulers_online)]),
    %% From a process of its own, as the Paddock client runs as a task.
    run_in(fun() ->
        {ok, S} = gen_server:start(?MODULE, [], []),
        call_latency(S, echo, "empty call"),
        call_latency(S, {echo, binary:copy(<<"x">>, 1024)}, "1 KiB echo"),
        cast_throughput(S),
        spawn_cost(),
        timers("idle"),
        Stop = load(),
        timer:sleep(200),
        timers("loaded"),
        Stop()
    end).

run_in(F) ->
    {Pid, Ref} = spawn_monitor(F),
    receive {'DOWN', Ref, process, Pid, normal} -> ok;
            {'DOWN', Ref, process, Pid, Why} -> io:format("failed: ~p~n", [Why]), halt(1)
    end.

init([]) -> {ok, 0}.
handle_call(echo, _From, N) -> {reply, ok, N};
handle_call({echo, B}, _From, N) -> {reply, B, N};
handle_call(count, _From, N) -> {reply, N + 1, N + 1};
handle_call({busy, Rounds}, _From, N) -> {reply, busy(Rounds, 1), N};
handle_call({timed, Ms}, _From, N) ->
    Due = erlang:monotonic_time(microsecond) + Ms * 1000,
    erlang:send_after(Ms, self(), {timed, Due}),
    {reply, ok, N};
handle_call(lateness, _From, N) ->
    Late = case get(late) of undefined -> []; L -> L end,
    put(late, []),
    {reply, Late, N}.
handle_cast(count, N) -> {noreply, N + 1}.
handle_info({timed, Due}, N) ->
    Late = erlang:monotonic_time(microsecond) - Due,
    put(late, [max(Late, 0) | case get(late) of undefined -> []; L -> L end]),
    {noreply, N}.

us(Native) -> erlang:convert_time_unit(Native, native, nanosecond) / 1000.

quantile(Sorted, Q) -> lists:nth(round((length(Sorted) - 1) * Q) + 1, Sorted).

call_latency(S, Msg, Label) ->
    T0 = erlang:monotonic_time(),
    Samples = [begin
                   T = erlang:monotonic_time(),
                   _ = gen_server:call(S, Msg),
                   erlang:monotonic_time() - T
               end || _ <- lists:seq(1, ?CALLS)],
    Total = us(erlang:monotonic_time() - T0),
    Sorted = lists:sort(Samples),
    io:format("call, ~s: p50 ~.2f us, p99 ~.2f us, p99.9 ~.2f us, max ~.2f us; ~b calls/s sequential~n",
              [Label, us(quantile(Sorted, 0.5)), us(quantile(Sorted, 0.99)),
               us(quantile(Sorted, 0.999)), us(lists:last(Sorted)), round(?CALLS / Total * 1.0e6)]).

cast_throughput(S) ->
    Before = gen_server:call(S, count),
    T0 = erlang:monotonic_time(),
    [gen_server:cast(S, count) || _ <- lists:seq(1, ?CASTS)],
    After = gen_server:call(S, count),
    Elapsed = us(erlang:monotonic_time() - T0),
    After = ?CASTS + Before + 1,
    io:format("cast: ~b casts/s handled (~p casts, unbounded mailbox)~n",
              [round(?CASTS / Elapsed * 1.0e6), ?CASTS]).

spawn_cost() ->
    erlang:garbage_collect(),
    Before = erlang:memory(processes),
    Timed = [begin
                 T = erlang:monotonic_time(),
                 {ok, P} = gen_server:start(?MODULE, [], []),
                 ok = gen_server:call(P, echo),
                 {erlang:monotonic_time() - T, P}
             end || _ <- lists:seq(1, ?SPAWNS)],
    timer:sleep(200),
    After = erlang:memory(processes),
    Sorted = lists:sort([D || {D, _} <- Timed]),
    {memory, One} = process_info(element(2, hd(Timed)), memory),
    io:format("start to first reply (~p gen_servers): p50 ~.2f us, p99 ~.2f us~n",
              [?SPAWNS, us(quantile(Sorted, 0.5)), us(quantile(Sorted, 0.99))]),
    io:format("idle memory per process: ~.1f KiB (erlang:memory(processes) delta), ~.1f KiB (process_info)~n",
              [(After - Before) / ?SPAWNS / 1024, One / 1024]),
    [exit(P, kill) || {_, P} <- Timed],
    ok.

timers(Condition) ->
    Servers = [element(2, gen_server:start(?MODULE, [], [])) || _ <- lists:seq(1, ?TIMERS)],
    lists:foreach(fun(Ms) ->
        Late = lists:append([round(S0, Servers, Ms) || S0 <- lists:seq(1, ?ROUNDS)]),
        Sorted = lists:sort(Late),
        io:format("~s, send_after ~p ms: lateness p50 ~p us, p99 ~p us, max ~p us~n",
                  [Condition, Ms, quantile(Sorted, 0.5), quantile(Sorted, 0.99), lists:last(Sorted)])
    end, [1, 10, 50]),
    [exit(P, kill) || P <- Servers],
    ok.

round(_, Servers, Ms) ->
    [ok = gen_server:call(S, {timed, Ms}) || S <- Servers],
    timer:sleep(Ms + 50),
    Late = lists:append([gen_server:call(S, lateness) || S <- Servers]),
    ?TIMERS = length(Late),
    Late.

%% The load; stopped by the fun it returns.
load() ->
    Rounds = calibrate(),
    Servers = [element(2, gen_server:start(?MODULE, [], [])) || _ <- lists:seq(1, ?LOAD_SERVERS)],
    Clients = [spawn(fun() -> client(lists:nth(I rem ?LOAD_SERVERS + 1, Servers), Rounds) end)
               || I <- lists:seq(1, ?LOAD_CLIENTS)],
    io:format("load: ~p clients, ~p gen_servers, busy ~p rounds per call (about ~p us)~n",
              [?LOAD_CLIENTS, ?LOAD_SERVERS, Rounds, ?BUSY_US]),
    fun() -> [exit(P, kill) || P <- Clients ++ Servers], ok end.

client(S, Rounds) ->
    _ = gen_server:call(S, {busy, Rounds}, infinity),
    client(S, Rounds).

%% Rounds of busy/2 that take about BUSY_US.
calibrate() ->
    N = 100000,
    T = erlang:monotonic_time(),
    busy(N, 1),
    max(1, round(N * ?BUSY_US / us(erlang:monotonic_time() - T))).

busy(0, X) -> X;
busy(N, X) -> busy(N - 1, (X * 6364136223846793005 + 1442695040888963407) band 16#FFFFFFFFFFFFFFFF).

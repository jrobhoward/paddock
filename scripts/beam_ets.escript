#!/usr/bin/env escript
%% -*- erlang -*-
%%! -noshell
%%
%% ETS reference figures for docs/BASELINE.md: the loops paddock-host's
%% kv_cost example runs in a guest against memory buckets, run in an Erlang
%% process against ETS tables. The same keys (<<"k000000">> ...), binary
%% values of the same sizes, and the same pseudo-random key sequence
%% (paddock-fixture-kvbench's generator), made before each loop is timed.
%% Each figure is the median of 7 runs, in ns per operation.
%%
%% - Memory: bytes per entry with 100,000 entries, the table's own words
%%   and, for values over 64 bytes, the off-heap binaries.
%% - Single operations on `set` and `ordered_set` tables of 1,000 and
%%   100,000 keys: lookup, a missing key, overwrite, insert of new keys,
%%   delete, update_counter.
%% - Ordered reads on `ordered_set`: 100 entries from a random key
%%   (lookup, then next_lookup), the whole table (foldl), next_lookup one
%%   key at a time.
%% - Concurrency: 1, 4 and 16 processes on one public table of 100,000
%%   keys, 95 or 50 reads in 100, or every write to one key; with the
%%   concurrency options off and on.
%% - Dependent reads: along a chain whose values name the next key, in ETS
%%   and in a persistent_term map; and 1,000 lookups per gen_server call,
%%   timed by the caller, as kv_cost's reads per call.
%% - Readers beside a writer: 1 to 16 processes reading while one more
%%   inserts as fast as it can, ETS with the concurrency options on.
%% - persistent_term, the read-only reference: one map of 100,000 keys,
%%   read by key from 1, 4 and 16 processes; no copy, no lock.
%%
%% Schedulers come from ERL_FLAGS, as for kv_cost's workers:
%%
%%   ERL_FLAGS='+S 4:4' escript scripts/beam_ets.escript
-mode(compile).

-define(RUNS, 7).

main(_) ->
    io:format("erts ~s, OTP ~s, schedulers: ~p; median of ~p runs; ns per operation~n~n",
              [erlang:system_info(version), erlang:system_info(otp_release),
               erlang:system_info(schedulers_online), ?RUNS]),
    run_in(fun() ->
        memory(),
        single(),
        ordered(),
        dependent(),
        concurrency(),
        readers_and_writer(),
        persistent()
    end).

run_in(F) ->
    {Pid, Ref} = spawn_monitor(F),
    receive {'DOWN', Ref, process, Pid, normal} -> ok;
            {'DOWN', Ref, process, Pid, Why} -> io:format("failed: ~p~n", [Why]), halt(1)
    end.

%% --- keys and the generator ---------------------------------------------------

-define(MASK, 16#FFFFFFFFFFFFFFFF).

%% paddock-fixture-kvbench's Lcg::next.
lcg(X, Below) ->
    X1 = (X * 6364136223846793005 + 1442695040888963407) band ?MASK,
    {(X1 bsr 33) rem max(Below, 1), X1}.

key(Prefix, I) -> iolist_to_binary(io_lib:format("~s~6..0B", [Prefix, I])).

value(Size) -> binary:copy(<<7>>, Size).

%% The keys and read choices a loop uses, as the guest picks them.
picked(Op, N, Keys, Seed) ->
    Reads = case Op of mix95 -> 95; mix50 -> 50; _ -> 100 end,
    picked(Op, 0, N, Keys, Reads, Seed, []).

picked(_, N, N, _, _, _, Acc) -> lists:reverse(Acc);
picked(Op, I, N, Keys, Reads, X, Acc) ->
    {Item, X2} =
        case Op of
            miss -> {K, X1} = lcg(X, Keys), {{key("m", K), true}, X1};
            insert -> {{key("n", I), false}, X};
            delete -> {{key("n", I), false}, X};
            hot -> {{key("k", 0), false}, X};
            _ ->
                {K, X1} = lcg(X, Keys),
                {R, Xr} = lcg(X1, 100),
                {{key("k", K), R < Reads}, Xr}
        end,
    picked(Op, I + 1, N, Keys, Reads, X2, [Item | Acc]).

%% Each entry its own value, as each bucket entry is: one binary shared by
%% every key would count its bytes once.
fill(T, Prefix, N, Size) ->
    ets:insert(T, [{key(Prefix, I), value(Size)} || I <- lists:seq(0, N - 1)]).

%% Run F on each item, keeping nothing: a list comprehension would collect
%% every result, garbage the Paddock loop does not make.
each(F, P) -> lists:foreach(F, P).

%% --- memory -----------------------------------------------------------------

memory() ->
    io:format("~-14s ~8s ~14s~n", ["memory", "value", "bytes/entry"]),
    Word = erlang:system_info(wordsize),
    lists:foreach(
      fun({Type, Size}) ->
              garbage_collect(),
              Bin0 = erlang:memory(binary),
              T = ets:new(mem, [Type, public]),
              fill(T, "k", 100000, Size),
              garbage_collect(),
              Bytes = ets:info(T, memory) * Word + (erlang:memory(binary) - Bin0),
              io:format("~-14s ~8B ~14.1f~n", [Type, Size, Bytes / 100000]),
              ets:delete(T)
      end,
      [{set, 16}, {set, 1024}, {ordered_set, 16}, {ordered_set, 1024}]).

%% --- single operations ----------------------------------------------------------

median(Xs) -> lists:nth(length(Xs) div 2 + 1, lists:sort(Xs)).

%% Median ns per operation of ?RUNS runs of Fun(Picked).
per_op(Op, N, Keys, Fun) ->
    median([begin
                P = picked(Op, N, Keys, Seed),
                T0 = erlang:monotonic_time(nanosecond),
                Fun(P),
                (erlang:monotonic_time(nanosecond) - T0) / N
            end || Seed <- lists:seq(0, ?RUNS - 1)]).

single() ->
    io:format("~n~-12s ~-11s ~8s ~8s ~10s~n", ["op", "table", "keys", "value", "ns/op"]),
    [single(Type, Keys, Size) || Type <- [set, ordered_set],
                                 Keys <- [1000, 100000],
                                 Size <- [16, 1024, 65536],
                                 not (Size =:= 65536 andalso Keys > 1000)],
    ok.

single(Type, Keys, Size) ->
    T = ets:new(bench, [Type, public]),
    fill(T, "k", Keys, Size),
    ets:insert(T, {<<"c">>, 0}),
    V = value(Size),
    N = case Size of 65536 -> 5000; _ -> 100000 end,
    Ops = [{get, fun(P) -> each(fun({K, _}) -> ets:lookup(T, K) end, P) end},
           {miss, fun(P) -> each(fun({K, _}) -> ets:lookup(T, K) end, P) end},
           {set, fun(P) -> each(fun({K, _}) -> ets:insert(T, {K, V}) end, P) end},
           {insert, fun(P) -> each(fun({K, _}) -> ets:insert(T, {K, V}) end, P) end},
           {delete, fun(P) -> each(fun({K, _}) -> ets:delete(T, K) end, P) end},
           {incr, fun(P) -> each(fun(_) -> ets:update_counter(T, <<"c">>, 1) end, P) end}],
    lists:foreach(
      fun({Op, F}) ->
              %% insert and delete alternate: each delete run removes what
              %% the matching insert run added.
              Ns = case Op of
                       insert -> per_op_paired(N, Keys, F, T);
                       delete -> per_op_deletes(N, Keys, T);
                       _ -> per_op(Op, N, Keys, F)
                   end,
              io:format("~-12s ~-11s ~8B ~8B ~10B~n", [Op, Type, Keys, Size, round(Ns)])
      end, Ops),
    ets:delete(T).

%% Inserts of new keys, each run followed by deleting them (untimed).
per_op_paired(N, Keys, F, T) ->
    median([begin
                P = picked(insert, N, Keys, 0),
                T0 = erlang:monotonic_time(nanosecond),
                F(P),
                Ns = (erlang:monotonic_time(nanosecond) - T0) / N,
                each(fun({K, _}) -> ets:delete(T, K) end, P),
                Ns
            end || _ <- lists:seq(1, ?RUNS)]).

%% Deletes of keys inserted just before (untimed).
per_op_deletes(N, Keys, T) ->
    V = value(16),
    median([begin
                P = picked(delete, N, Keys, 0),
                each(fun({K, _}) -> ets:insert(T, {K, V}) end, P),
                T0 = erlang:monotonic_time(nanosecond),
                each(fun({K, _}) -> ets:delete(T, K) end, P),
                (erlang:monotonic_time(nanosecond) - T0) / N
            end || _ <- lists:seq(1, ?RUNS)]).

%% --- ordered reads ----------------------------------------------------------------

next_lookup(T, K) ->
    case erlang:function_exported(ets, next_lookup, 2) of
        true -> ets:next_lookup(T, K);
        false ->
            case ets:next(T, K) of
                '$end_of_table' -> '$end_of_table';
                K2 -> {K2, ets:lookup(T, K2)}
            end
    end.

first_lookup(T) ->
    case erlang:function_exported(ets, first_lookup, 1) of
        true -> ets:first_lookup(T);
        false ->
            case ets:first(T) of
                '$end_of_table' -> '$end_of_table';
                K -> {K, ets:lookup(T, K)}
            end
    end.

%% 100 entries from K: K itself, then the next 99.
range(T, K) -> range(T, K, ets:lookup(T, K), 99, []).

range(_, _, _, 0, Acc) -> Acc;
range(T, K, Found, Left, Acc) ->
    case next_lookup(T, K) of
        '$end_of_table' -> [Found | Acc];
        {K2, Objs} -> range(T, K2, Objs, Left - 1, [Found | Acc])
    end.

walk(_, _, 0) -> ok;
walk(T, '$end_of_table', N) -> walk(T, first_lookup(T), N - 1);
walk(T, {K, _}, N) -> walk(T, next_lookup(T, K), N - 1).

ordered() ->
    code:ensure_loaded(ets),
    io:format("~n~-12s ~8s ~12s ~12s~n", ["ordered", "keys", "ns/call", "ns/entry"]),
    lists:foreach(
      fun(Keys) ->
              T = ets:new(bench, [ordered_set, public]),
              fill(T, "k", Keys, 16),
              Range = per_op(range, 2000, Keys,
                             fun(P) -> each(fun({K, _}) -> range(T, K) end, P) end),
              io:format("~-12s ~8B ~12B ~12.1f~n", [range, Keys, round(Range), Range / 100]),
              Scans = case Keys > 1000 of true -> 5; false -> 200 end,
              All = per_op(all, Scans, Keys,
                           fun(P) -> each(fun(_) -> ets:foldl(fun(_, A) -> A + 1 end, 0, T) end, P) end),
              io:format("~-12s ~8B ~12B ~12.1f~n", [all, Keys, round(All), All / Keys]),
              Walk = median([begin
                                 T0 = erlang:monotonic_time(nanosecond),
                                 walk(T, first_lookup(T), 20000),
                                 (erlang:monotonic_time(nanosecond) - T0) / 20000
                             end || _ <- lists:seq(1, ?RUNS)]),
              io:format("~-12s ~8B ~12B ~12.1f~n", [next, Keys, round(Walk), Walk]),
              ets:delete(T)
      end, [1000, 100000]).

%% --- concurrency --------------------------------------------------------------------

concurrency() ->
    io:format("~n~-8s ~-11s ~-6s ~9s ~14s~n",
              ["mix", "table", "conc", "processes", "ops/s"]),
    Keys = 100000,
    N = 50000,
    V = value(16),
    [begin
         Opts = case Conc of
                    off -> [];
                    on -> [{read_concurrency, true}, {write_concurrency, auto}]
                end,
         T = ets:new(shared, [Type, public | Opts]),
         fill(T, "k", Keys, 16),
         [begin
              Lists = [picked(Mix, N, Keys, I) || I <- lists:seq(0, Procs - 1)],
              Self = self(),
              Pids = [spawn_link(fun() ->
                                         receive go -> ok end,
                                         each(fun({K, true}) -> ets:lookup(T, K);
                                                 ({K, false}) -> ets:insert(T, {K, V})
                                              end, P),
                                         Self ! {done, self()}
                                 end) || P <- Lists],
              T0 = erlang:monotonic_time(nanosecond),
              [P ! go || P <- Pids],
              [receive {done, P} -> ok end || P <- Pids],
              Secs = (erlang:monotonic_time(nanosecond) - T0) / 1.0e9,
              io:format("~-8s ~-11s ~-6s ~9B ~14B~n",
                        [Mix, Type, Conc, Procs, round(N * Procs / Secs)])
          end || Mix <- [mix95, mix50, hot], Procs <- [1, 4, 16]],
         ets:delete(T)
     end || Type <- [set, ordered_set], Conc <- [off, on]],
    ok.

%% --- dependent reads -----------------------------------------------------------

%% The chain i -> (7919 i + 1) mod N, as paddock-fixture-kvbench's fillchain.
chain_pairs(N) -> [{key("c", I), key("c", (7919 * I + 1) rem N)} || I <- lists:seq(0, N - 1)].

walk_ets(_, K, 0) -> K;
walk_ets(T, K, N) -> [{_, Next}] = ets:lookup(T, K), walk_ets(T, Next, N - 1).

walk_map(_, K, 0) -> K;
walk_map(M, K, N) -> walk_map(M, maps:get(K, M), N - 1).

walk_pt(_, K, 0) -> K;
walk_pt(Name, K, N) -> walk_pt(Name, maps:get(K, persistent_term:get(Name)), N - 1).

timed(N, F) ->
    median([begin
                T0 = erlang:monotonic_time(nanosecond),
                F(),
                (erlang:monotonic_time(nanosecond) - T0) / N
            end || _ <- lists:seq(1, ?RUNS)]).

%% Its key lists made up front, as the guest makes them before its clock,
%% so a call carries an index, not 1,000 keys to copy.
reader_loop(T, Lists) ->
    receive
        {From, I} ->
            each(fun({K, _}) -> ets:lookup(T, K) end, element(I, Lists)),
            From ! {self(), done},
            reader_loop(T, Lists)
    end.

dependent() ->
    io:format("~n~-16s ~8s ~10s~n", ["reads", "keys", "ns/read"]),
    lists:foreach(
      fun(Keys) ->
              Pairs = chain_pairs(Keys),
              T = ets:new(chain, [ordered_set, public]),
              ets:insert(T, Pairs),
              io:format("~-16s ~8B ~10B~n",
                        ["chain ets", Keys, round(timed(100000, fun() -> walk_ets(T, key("c", 0), 100000) end))]),
              persistent_term:put({?MODULE, chain}, maps:from_list(Pairs)),
              io:format("~-16s ~8B ~10B~n",
                        ["chain pt", Keys, round(timed(100000, fun() -> walk_pt({?MODULE, chain}, key("c", 0), 100000) end))]),
              persistent_term:erase({?MODULE, chain}),
              ets:delete(T)
      end, [1000, 100000]),
    %% 1,000 lookups per call to a process that owns nothing but reads a
    %% public table, timed by the caller: the call counts, as in kv_cost.
    T = ets:new(bench, [ordered_set, public]),
    fill(T, "k", 100000, 16),
    Lists = list_to_tuple([picked(get, 1000, 100000, Seed) || Seed <- lists:seq(0, 199)]),
    Reader = spawn_link(fun() -> reader_loop(T, Lists) end),
    PerRead = median([begin
                          T0 = erlang:monotonic_time(nanosecond),
                          Reader ! {self(), Seed + 1},
                          receive {Reader, done} -> ok end,
                          (erlang:monotonic_time(nanosecond) - T0) / 1000
                      end || Seed <- lists:seq(0, 199)]),
    io:format("~-16s ~8B ~10B~n", ["1000 per call", 100000, round(PerRead)]),
    unlink(Reader),
    exit(Reader, kill),
    ets:delete(T).

%% --- readers beside a writer -------------------------------------------------------

readers_and_writer() ->
    io:format("~n~-8s ~-11s ~9s ~14s ~14s~n",
              ["readers", "table", "processes", "reads/s", "writes/s"]),
    Keys = 100000,
    N = 50000,
    V = value(16),
    [begin
         T = ets:new(shared, [Type, public, {read_concurrency, true}, {write_concurrency, auto}]),
         fill(T, "k", Keys, 16),
         [begin
              Self = self(),
              Writes = picked(set, N, Keys, 99),
              Writer = spawn_link(fun() ->
                                          receive go -> ok end,
                                          W0 = erlang:monotonic_time(nanosecond),
                                          each(fun({K, _}) -> ets:insert(T, {K, V}) end, Writes),
                                          Self ! {writer, N / ((erlang:monotonic_time(nanosecond) - W0) / 1.0e9)}
                                  end),
              Lists = [picked(get, N, Keys, I) || I <- lists:seq(0, Procs - 1)],
              Pids = [spawn_link(fun() ->
                                         receive go -> ok end,
                                         each(fun({K, _}) -> ets:lookup(T, K) end, P),
                                         Self ! {done, self()}
                                 end) || P <- Lists],
              T0 = erlang:monotonic_time(nanosecond),
              [P ! go || P <- [Writer | Pids]],
              [receive {done, P} -> ok end || P <- Pids],
              Secs = (erlang:monotonic_time(nanosecond) - T0) / 1.0e9,
              WritesPerSec = receive {writer, W} -> W end,
              io:format("~-8s ~-11s ~9B ~14B ~14B~n",
                        ["get+set", Type, Procs, round(N * Procs / Secs), round(WritesPerSec)])
          end || Procs <- [1, 4, 16]],
         ets:delete(T)
     end || Type <- [set, ordered_set]],
    ok.

%% --- persistent_term ---------------------------------------------------------------

persistent() ->
    io:format("~n~-16s ~9s ~10s ~14s~n", ["persistent_term", "processes", "ns/read", "reads/s"]),
    Keys = 100000,
    N = 100000,
    Name = {?MODULE, table},
    persistent_term:put(Name, maps:from_list([{key("k", I), value(16)} || I <- lists:seq(0, Keys - 1)])),
    Single = per_op(get, N, Keys,
                    fun(P) -> each(fun({K, _}) -> maps:get(K, persistent_term:get(Name)) end, P) end),
    io:format("~-16s ~9B ~10B~n", ["get", 1, round(Single)]),
    lists:foreach(
      fun(Procs) ->
              Self = self(),
              Lists = [picked(get, N, Keys, I) || I <- lists:seq(0, Procs - 1)],
              Pids = [spawn_link(fun() ->
                                         receive go -> ok end,
                                         each(fun({K, _}) -> maps:get(K, persistent_term:get(Name)) end, P),
                                         Self ! {done, self()}
                                 end) || P <- Lists],
              T0 = erlang:monotonic_time(nanosecond),
              [P ! go || P <- Pids],
              [receive {done, P} -> ok end || P <- Pids],
              Secs = (erlang:monotonic_time(nanosecond) - T0) / 1.0e9,
              io:format("~-16s ~9B ~10s ~14B~n", ["get", Procs, "", round(N * Procs / Secs)])
      end, [1, 4, 16]),
    persistent_term:erase(Name),
    ok.

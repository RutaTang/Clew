%% clew's debugger bridge, run inside the debugged node: the adapter's
%% commands arrive as Erlang terms, one per line, on a socket to it; what
%% happens — a process stopped, the program's end, each command's answer — is
%% sent back as JSON lines. It drives OTP's own interpreter (`int`): the
%% project's modules run interpreted, a process that reaches a breakpoint is
%% attached, and its stack, bindings and expressions are read through it.
-module(clew_dap).
-export([start/1, attach/1]).

start([PortAtom]) ->
    Port = list_to_integer(atom_to_list(PortAtom)),
    {ok, Sock} = gen_tcp:connect({127, 0, 0, 1}, Port, [binary, {packet, line}, {active, false}]),
    Self = self(),
    register(clew_dap, Self),
    int:auto_attach([break], {clew_dap, attach}),
    spawn_link(fun() -> reader(Sock, Self) end),
    loop(#{sock => Sock, threads => #{}, next_id => 1, refs => #{}, next_ref => 1, waiting => #{},
           pausing => [], paused => false}).

reader(Sock, Main) ->
    case gen_tcp:recv(Sock, 0) of
        {ok, Line} ->
            case parse(binary_to_list(Line)) of
                {ok, Term} -> Main ! {command, Term};
                {error, Why} -> Main ! {bad_command, Why}
            end,
            reader(Sock, Main);
        {error, _} ->
            erlang:halt(0)
    end.

parse(Text) ->
    case erl_scan:string(Text) of
        {ok, Tokens, _} ->
            case erl_parse:parse_term(Tokens) of
                {ok, Term} -> {ok, Term};
                {error, E} -> {error, E}
            end;
        {error, E, _} -> {error, E}
    end.

%% A process that reached a breakpoint: attach, and relay its meta process.
attach(Pid) ->
    {ok, Meta} = int:attached(Pid),
    clew_dap ! {attached, Pid, Meta, self()},
    relay(Meta).

relay(Meta) ->
    receive
        {Meta, Event} -> clew_dap ! {meta, Meta, Event}, relay(Meta);
        {do, Fun} -> Fun(), relay(Meta);
        _ -> relay(Meta)
    end.

loop(S) ->
    receive
        {command, Term} -> loop(command(Term, S));
        {bad_command, Why} -> send(S, #{event => <<"error">>, message => fmt(Why)}), loop(S);
        {attached, Pid, Meta, Relay} ->
            Id = maps:get(next_id, S),
            Threads = maps:put(Id, #{pid => Pid, meta => Meta, relay => Relay}, maps:get(threads, S)),
            loop(S#{threads => Threads, next_id => Id + 1});
        {meta, Meta, Event} -> loop(meta(Meta, Event, S));
        {program_done, Code, Why} ->
            send(S, #{event => <<"exited">>, code => Code, reason => Why}),
            loop(S);
        _ -> loop(S)
    end.

thread_of(Meta, S) ->
    case [Id || {Id, #{meta := M}} <- maps:to_list(maps:get(threads, S)), M =:= Meta] of
        [Id | _] -> Id;
        [] -> undefined
    end.

meta(Meta, {break_at, Mod, Line, _Cm}, S) ->
    case {thread_of(Meta, S), maps:is_key(Meta, maps:get(waiting, S))} of
        {undefined, _} -> S;
        %% An evaluation re-reports where the process is stopped: not news.
        {_, true} -> S;
        {Id, false} ->
            %% Stopped by a pause: its breakpoints everywhere go again.
            Paused = maps:get(paused, S),
            [int:delete_break(M, L) || {M, L} <- maps:get(pausing, S)],
            Reason = case Paused of true -> <<"pause">>; false -> <<"breakpoint">> end,
            send(S, #{event => <<"stopped">>, thread => Id, module => Mod, line => Line, reason => Reason}),
            Threads = maps:get(threads, S),
            Thread = maps:get(Id, Threads),
            S#{refs => #{}, threads => maps:put(Id, Thread#{line => Line}, Threads),
               pausing => [], paused => false}
    end;
meta(Meta, {eval_rsp, Value}, S) ->
    Waiting = maps:get(waiting, S),
    case maps:take(Meta, Waiting) of
        {Seq, Rest} ->
            {Shown, S2} = value(Value, S),
            reply(S2, Seq, Shown),
            S2#{waiting => Rest};
        error -> S
    end;
meta(Meta, {exit_at, _Where, _Reason, _Cm}, S) ->
    %% Stopped in the stack of a process that exits: let it go.
    int:meta(Meta, continue),
    S;
meta(_Meta, _Event, S) ->
    S.

command({interpret, Mods}, S) ->
    Results = [#{module => M, ok => interpreted(int:i(M))} || M <- Mods],
    send(S, #{answer => <<"interpret">>, modules => Results}),
    S;
command({break, Seq, Mod, Lines}, S) ->
    [int:delete_break(Mod, L) || {{M, L}, _} <- int:all_breaks(), M =:= Mod],
    Set = [#{line => L, ok => int:break(Mod, L) =:= ok} || L <- Lines],
    reply(S, Seq, Set),
    S;
command({run, Mod, Fun, Args}, S) ->
    Main = self(),
    spawn(fun() ->
        {Code, Why} =
            try
                case Args of
                    none -> Mod:Fun();
                    _ -> Mod:Fun(Args)
                end,
                {0, <<"normal">>}
            catch
                Class:Reason:Stack ->
                    io:format(standard_error, "~p:~tp~n~tp~n", [Class, Reason, Stack]),
                    {1, fmt({Class, Reason})}
            end,
        Main ! {program_done, Code, Why}
    end),
    S;
command({resume, Thread, How}, S) ->
    case thread(Thread, S) of
        #{meta := Meta} -> int:meta(Meta, How);
        _ -> ok
    end,
    S;
%% Pause: nothing stops a running interpreted process from outside, so it is
%% stopped by a breakpoint on every line of the interpreted modules that has
%% one, taken off again at the first stop.
command({pause, Seq}, S) ->
    Existing = [{M, L} || {{M, L}, _} <- int:all_breaks()],
    Temporary = [{M, L} || M <- int:interpreted(), L <- source_lines(M),
                           not lists:member({M, L}, Existing),
                           int:break(M, L) =:= ok],
    reply(S, Seq, length(Temporary)),
    S#{pausing => Temporary, paused => true};
command({threads, Seq}, S) ->
    Threads = [#{id => Id, name => fmt(Pid)} || {Id, #{pid := Pid}} <- maps:to_list(maps:get(threads, S)),
                                                is_process_alive(Pid)],
    reply(S, Seq, Threads),
    S;
command({stack, Seq, Thread}, S) ->
    Frames = case thread(Thread, S) of
        #{meta := Meta} = T -> frames(Meta, maps:get(line, T, 0));
        _ -> []
    end,
    reply(S, Seq, Frames),
    S;
command({bindings, Seq, Thread, Level}, S) ->
    case thread(Thread, S) of
        #{meta := Meta} ->
            Bindings = case call(fun() -> int:meta(Meta, bindings, Level) end) of
                {ok, B} when is_list(B) -> B;
                _ -> []
            end,
            {Shown, S2} = lists:foldl(
                fun({Name, Value}, {Acc, St}) ->
                    {V, St2} = value(Value, St),
                    {[V#{name => atom_to_binary(Name, utf8)} | Acc], St2}
                end, {[], S}, Bindings),
            reply(S2, Seq, lists:reverse(Shown)),
            S2;
        _ ->
            reply(S, Seq, []),
            S
    end;
command({children, Seq, Ref}, S) ->
    case maps:find(Ref, maps:get(refs, S)) of
        {ok, Term} ->
            {Shown, S2} = lists:foldl(
                fun({Name, Value}, {Acc, St}) ->
                    {V, St2} = value(Value, St),
                    {[V#{name => Name} | Acc], St2}
                end, {[], S}, children(Term)),
            reply(S2, Seq, lists:reverse(Shown)),
            S2;
        error ->
            reply(S, Seq, []),
            S
    end;
command({eval, Seq, Thread, Level, Expr}, S) ->
    case thread(Thread, S) of
        #{meta := Meta, relay := Relay} = T ->
            Mod = case frames(Meta, maps:get(line, T, 0)) of
                [#{module := M} | _] -> M;
                _ -> erlang
            end,
            Text = case lists:reverse(string:trim(Expr)) of
                "." ++ _ -> Expr;
                _ -> Expr ++ "."
            end,
            Relay ! {do, fun() -> int:meta(Meta, eval, {Mod, Text, Level}) end},
            S#{waiting => maps:put(Meta, Seq, maps:get(waiting, S))};
        _ ->
            reply(S, Seq, #{value => <<"no such process">>, ref => 0}),
            S
    end;
command(_, S) ->
    S.

%% The lines of an interpreted module's source.
source_lines(Mod) ->
    case int:file(Mod) of
        File when is_list(File) ->
            case file:read_file(File) of
                {ok, Text} -> lists:seq(1, length(binary:split(Text, <<"\n">>, [global])));
                _ -> []
            end;
        _ -> []
    end.

interpreted({module, _}) -> true;
interpreted(_) -> false.

thread(Id, S) -> maps:get(Id, maps:get(threads, S), undefined).

%% The stack, top first: each frame's module, function, arity and line, its
%% level being what `bindings` and `eval` take. The top frame is at `Top`,
%% the line the process stopped at; each other frame is where its callee was
%% called from — `stack_frame` read up from the level above it (`down`, or
%% `up` past the top, takes the meta process down).
frames(Meta, Top) ->
    Backtrace = case call(fun() -> int:meta(Meta, backtrace, all) end) of
        {ok, B} when is_list(B) -> B;
        _ -> []
    end,
    {Frames, _} = lists:mapfoldl(
        fun({Level, {M, F, A}}, Above) ->
            Line = case Above of
                none -> Top;
                _ ->
                    case call(fun() -> int:meta(Meta, stack_frame, {up, Above}) end) of
                        {ok, {_, {_, L}, _}} when is_integer(L) -> L;
                        _ -> 0
                    end
            end,
            {#{level => Level, module => M, function => F, arity => length(A), line => Line}, Level};
           ({Level, _}, _) ->
            {#{level => Level, module => <<"?">>, function => <<"?">>, arity => 0, line => 0}, Level}
        end,
        none, Backtrace),
    Frames.

%% `Fun`'s result, or `timeout` when the meta process does not answer: one
%% that died leaves `int:meta` waiting for ever.
call(Fun) ->
    Caller = self(),
    Ref = make_ref(),
    Pid = spawn(fun() -> Caller ! {Ref, catch Fun()} end),
    receive
        {Ref, Result} -> {ok, Result}
    after 5000 ->
        exit(Pid, kill),
        timeout
    end.

%% A value as shown: its text, and a reference to list its parts by when it
%% has any.
value(Term, S) ->
    Text = fmt(Term),
    case children(Term) of
        [] -> {#{value => Text, ref => 0}, S};
        _ ->
            Ref = maps:get(next_ref, S),
            {#{value => Text, ref => Ref},
             S#{refs => maps:put(Ref, Term, maps:get(refs, S)), next_ref => Ref + 1}}
    end.

children(Term) when is_list(Term) ->
    case io_lib:printable_unicode_list(Term) of
        true -> [];
        false -> indexed(Term)
    end;
children(Term) when is_tuple(Term) -> indexed(tuple_to_list(Term));
children(Term) when is_map(Term) -> [{fmt(K), V} || {K, V} <- maps:to_list(Term)];
children(_) -> [].

indexed(List) ->
    {Named, _} = lists:mapfoldl(fun(V, I) -> {{integer_to_binary(I), V}, I + 1} end, 1, List),
    lists:sublist(Named, 500).

fmt(Term) -> unicode:characters_to_binary(io_lib:format("~0tp", [Term])).

reply(S, Seq, Body) -> send(S, #{answer => Seq, body => Body}).

send(#{sock := Sock}, Message) ->
    gen_tcp:send(Sock, [json(Message), $\n]).

%% JSON, as much of it as these messages need.
json(Map) when is_map(Map) ->
    ["{", lists:join(",", [[json(key(K)), ":", json(V)] || {K, V} <- maps:to_list(Map)]), "}"];
json(List) when is_list(List) -> ["[", lists:join(",", [json(V) || V <- List]), "]"];
json(true) -> "true";
json(false) -> "false";
json(undefined) -> "null";
json(N) when is_integer(N) -> integer_to_list(N);
json(A) when is_atom(A) -> json(atom_to_binary(A, utf8));
json(B) when is_binary(B) -> [$", escape(unicode:characters_to_list(B)), $"].

key(K) when is_atom(K) -> atom_to_binary(K, utf8);
key(K) -> K.

escape([]) -> [];
escape([$" | R]) -> [$\\, $" | escape(R)];
escape([$\\ | R]) -> [$\\, $\\ | escape(R)];
escape([$\n | R]) -> [$\\, $n | escape(R)];
escape([$\r | R]) -> [$\\, $r | escape(R)];
escape([$\t | R]) -> [$\\, $t | escape(R)];
escape([C | R]) when C < 32 -> [io_lib:format("\\u~4.16.0b", [C]) | escape(R)];
escape([C | R]) -> [unicode:characters_to_binary([C]) | escape(R)].

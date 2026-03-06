-module(tier2_stdlib).
-export([test/0]).

test() ->
    ok = test_maps_native(),
    ok = test_lists_native(),
    ok = test_tuple_ops(),
    ok = test_type_conversions(),
    ok = test_comparisons(),
    passed.

%% --- maps BIFs ---
test_maps_native() ->
    M0 = maps:new(),
    0 = maps:size(M0),
    M1 = maps:put(a, 1, M0),
    M2 = maps:put(b, 2, M1),
    1 = maps:get(a, M2),
    2 = maps:get(b, M2),
    default = maps:get(c, M2, default),
    true = maps:is_key(a, M2),
    false = maps:is_key(c, M2),
    {ok, 1} = maps:find(a, M2),
    error = maps:find(c, M2),
    M3 = maps:remove(a, M2),
    false = maps:is_key(a, M3),
    M4 = maps:merge(M2, #{c => 3}),
    3 = maps:get(c, M4),
    2 = maps:size(M2),
    %% keys and values — sort since order may vary
    KList = lists:sort(maps:keys(M2)),
    [a, b] = KList,
    VList = lists:sort(maps:values(M2)),
    [1, 2] = VList,
    %% from_list / to_list round-trip
    Pairs = [{x, 10}, {y, 20}],
    M5 = maps:from_list(Pairs),
    10 = maps:get(x, M5),
    20 = maps:get(y, M5),
    ok.

%% --- lists operations ---
test_lists_native() ->
    [1, 2, 3] = lists:sort([3, 1, 2]),
    [a, b, c, d] = lists:append([a, b], [c, d]),
    [1, 2, 3, 4] = lists:flatten([[1, 2], [3, 4]]),
    [1, 2, 3] = lists:flatten([1, [2], [[3]]]),
    b = lists:nth(2, [a, b, c]),
    c = lists:last([a, b, c]),
    %% Higher-order list functions (stdlib Erlang, not pure BIFs)
    [2, 4, 6] = lists:map(fun(X) -> X * 2 end, [1, 2, 3]),
    6 = lists:foldl(fun(X, Acc) -> X + Acc end, 0, [1, 2, 3]),
    [2, 4] = lists:filter(fun(X) -> X rem 2 == 0 end, [1, 2, 3, 4]),
    ok.

%% --- tuple operations (erlang BIFs) ---
test_tuple_ops() ->
    T = {a, b, c},
    a = element(1, T),
    c = element(3, T),
    3 = tuple_size(T),
    [a, b, c] = tuple_to_list(T),
    {x, y, z} = list_to_tuple([x, y, z]),
    {a, x, c} = setelement(2, T, x),
    ok.

%% --- type conversion BIFs ---
test_type_conversions() ->
    <<"hello">> = atom_to_binary(hello),
    world = binary_to_atom(<<"world">>),
    ok.

%% --- comparison BIFs ---
test_comparisons() ->
    5 = max(3, 5),
    3 = min(3, 5),
    b = max(a, b),
    a = min(a, b),
    ok.

-module(parity_maps).
-export([test/0]).

test() ->
    ok = test_empty_map(),
    ok = test_put_assoc(),
    ok = test_put_exact(),
    ok = test_get_elements(),
    ok = test_has_fields(),
    ok = test_is_map(),
    ok = test_nested(),
    passed.

test_empty_map() ->
    M = #{},
    true = is_map(M),
    0 = map_size(M),
    ok.

test_put_assoc() ->
    M = #{a => 1, b => 2, c => 3},
    3 = map_size(M),
    1 = maps:get(a, M),
    ok.

test_put_exact() ->
    M0 = #{x => 10},
    M1 = M0#{x := 20},
    20 = maps:get(x, M1),
    ok.

test_get_elements() ->
    M = #{k1 => v1, k2 => v2},
    v1 = maps:get(k1, M),
    v2 = maps:get(k2, M),
    ok.

test_has_fields() ->
    M = #{a => 1},
    true = maps:is_key(a, M),
    false = maps:is_key(b, M),
    ok.

test_is_map() ->
    true = is_map(#{}),
    true = is_map(#{a => 1}),
    false = is_map(not_a_map),
    false = is_map([]),
    ok.

test_nested() ->
    M = #{outer => #{inner => deep}},
    #{inner := deep} = maps:get(outer, M),
    ok.

-module(tier1_kitchen_sink).
-export([test/0]).

%% Exercises: maps, floats, exceptions, binary, select_tuple_arity, type tests
test() ->
    ok = test_maps(),
    ok = test_floats(),
    ok = test_exceptions(),
    ok = test_binary(),
    ok = test_type_checks(),
    ok = test_select_tuple(),
    passed.

test_maps() ->
    M0 = #{},
    M1 = M0#{a => 1, b => 2},
    1 = maps:get(a, M1),
    2 = maps:get(b, M1),
    M2 = M1#{a := 10},
    10 = maps:get(a, M2),
    true = is_map(M2),
    [a, b] = lists:sort(maps:keys(M2)),
    ok.

test_floats() ->
    3.0 = 1.0 + 2.0,
    -1.0 = 1.0 - 2.0,
    6.0 = 2.0 * 3.0,
    2.0 = 6.0 / 3.0,
    -1.5 = -1.5,
    ok.

test_exceptions() ->
    ok = try
        throw(test_throw),
        error
    catch
        throw:test_throw -> ok
    end,
    {error, badarg} = try
        erlang:error(badarg),
        error
    catch
        error:badarg -> {error, badarg}
    end,
    ok.

test_binary() ->
    <<"hello">> = <<"hello">>,
    5 = byte_size(<<"hello">>),
    <<1, 2, 3>> = <<1, 2, 3>>,
    <<H, _Rest/binary>> = <<1, 2, 3>>,
    1 = H,
    ok.

test_type_checks() ->
    true = is_atom(hello),
    true = is_integer(42),
    true = is_float(3.14),
    true = is_list([1, 2]),
    true = is_binary(<<"bin">>),
    true = is_boolean(true),
    true = is_map(#{}),
    true = is_tuple({a, b}),
    ok.

test_select_tuple() ->
    a = classify({point, 1, 2}),
    b = classify({line, 1, 2, 3, 4}),
    unknown = classify({other}),
    ok.

classify({point, _, _}) -> a;
classify({line, _, _, _, _}) -> b;
classify(_) -> unknown.

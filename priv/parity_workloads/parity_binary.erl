-module(parity_binary).
-export([test/0]).

test() ->
    ok = test_construction(),
    ok = test_matching(),
    ok = test_append(),
    ok = test_match_string(),
    ok = test_bit_operations(),
    passed.

test_construction() ->
    <<1, 2, 3>> = <<1, 2, 3>>,
    <<"hello">> = <<"hello">>,
    <<1:16>> = <<0, 1>>,
    ok.

test_matching() ->
    <<A, B, C>> = <<10, 20, 30>>,
    10 = A, 20 = B, 30 = C,
    <<_:8, Rest/binary>> = <<"hello">>,
    <<"ello">> = Rest,
    ok.

test_append() ->
    A = <<1, 2>>,
    B = <<3, 4>>,
    <<1, 2, 3, 4>> = <<A/binary, B/binary>>,
    ok.

test_match_string() ->
    <<"ello">> = match_hello(<<"hello">>),
    no_match = match_hello(<<"world">>),
    ok.

match_hello(<<"hello", Rest/binary>>) -> Rest;
match_hello(_) -> no_match.

test_bit_operations() ->
    <<5:4>> = <<5:4>>,
    ok.

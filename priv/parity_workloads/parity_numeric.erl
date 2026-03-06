-module(parity_numeric).
-export([test/0]).

test() ->
    ok = test_integer_arith(),
    ok = test_float_arith(),
    ok = test_mixed_arith(),
    ok = test_division(),
    ok = test_abs(),
    ok = test_guard_bifs(),
    passed.

test_integer_arith() ->
    3 = 1 + 2,
    -1 = 1 - 2,
    6 = 2 * 3,
    2 = 7 div 3,
    1 = 7 rem 3,
    ok.

test_float_arith() ->
    3.0 = 1.0 + 2.0,
    0.5 = 1.0 / 2.0,
    ok.

test_mixed_arith() ->
    3.0 = 1 + 2.0,
    3.0 = 1.0 + 2,
    ok.

test_division() ->
    0.5 = 1 / 2,
    caught = try 1 div 0 catch error:badarith -> caught end,
    caught = try 1 rem 0 catch error:badarith -> caught end,
    ok.

test_abs() ->
    5 = abs(-5),
    5 = abs(5),
    3.14 = abs(-3.14),
    ok.

test_guard_bifs() ->
    true = is_number(42),
    true = is_number(3.14),
    true = is_integer(42),
    false = is_integer(3.14),
    true = is_float(3.14),
    false = is_float(42),
    ok.

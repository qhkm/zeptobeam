-module(parity_floats).
-export([test/0]).

test() ->
    ok = test_basic_arith(),
    ok = test_conversions(),
    ok = test_negation(),
    ok = test_mixed(),
    passed.

test_basic_arith() ->
    3.0 = 1.0 + 2.0,
    -1.0 = 1.0 - 2.0,
    6.0 = 2.0 * 3.0,
    0.5 = 1.0 / 2.0,
    ok.

test_conversions() ->
    2.0 = float(2),
    ok.

test_negation() ->
    -3.14 = -(3.14),
    ok.

test_mixed() ->
    3.0 = 1 + 2.0,
    3.0 = 1.0 + 2,
    ok.

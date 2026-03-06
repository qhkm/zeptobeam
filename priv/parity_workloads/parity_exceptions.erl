-module(parity_exceptions).
-export([test/0]).

test() ->
    ok = test_try_catch(),
    ok = test_case_clause(),
    ok = test_if_clause(),
    ok = test_catch_expr(),
    ok = test_stacktrace(),
    passed.

test_try_catch() ->
    ok = try ok catch _:_ -> error end,
    {caught, badarg} = try
        erlang:error(badarg)
    catch
        error:Reason -> {caught, Reason}
    end,
    ok.

test_case_clause() ->
    Result = try
        case not_matched of
            something_else -> wrong
        end
    catch
        error:{case_clause, not_matched} -> caught_case
    end,
    caught_case = Result,
    ok.

test_if_clause() ->
    Result = try
        X = 0,
        if X > 0 -> positive;
           X < 0 -> negative
        end
    catch
        error:if_clause -> caught_if
    end,
    caught_if = Result,
    ok.

test_catch_expr() ->
    ok = catch ok,
    {'EXIT', {badarg, _}} = catch erlang:error(badarg),
    ok.

test_stacktrace() ->
    try
        erlang:error(test_error)
    catch
        error:test_error:Stacktrace ->
            true = is_list(Stacktrace)
    end,
    ok.

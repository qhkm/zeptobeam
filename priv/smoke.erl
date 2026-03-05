-module(smoke).
-export([test/0]).

%% Minimal smoke test — no stdlib dependencies.
test() ->
    3 = add(1, 2),
    ok = identity(ok),
    hello = identity(hello),
    0 = countdown(5),
    [3, 2, 1] = rev([1, 2, 3], []),
    passed.

add(A, B) -> A + B.

identity(X) -> X.

countdown(0) -> 0;
countdown(N) when N > 0 -> countdown(N - 1).

rev([], Acc) -> Acc;
rev([H | T], Acc) -> rev(T, [H | Acc]).

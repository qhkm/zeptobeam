-module(test_bs_match).
-export([test/0]).

%% Stress test for bs_match opcode and binary matching in the ErlangRT emulator.
%% Exercises: ensure_at_least, ensure_exactly, =:= (eq), skip, get_tail,
%%            byte-aligned and bit-level binary matching, offset restore on fail.

test() ->
    ok = test_exact_byte_match(),
    ok = test_split_binary(),
    ok = test_integer_extraction(),
    ok = test_skip_and_tail(),
    ok = test_nested_match(),
    ok = test_match_failure_restores_offset(),
    ok = test_empty_binary(),
    ok = test_multi_field(),
    ok = test_binary_construction_then_match(),
    passed.

%% 1. Exact byte matching with literals
test_exact_byte_match() ->
    <<1, 2, 3>> = <<1, 2, 3>>,
    <<>> = <<>>,
    ok.

%% 2. Splitting a binary at various positions
test_split_binary() ->
    Bin = <<10, 20, 30, 40, 50>>,
    <<A:2/binary, B:3/binary>> = Bin,
    <<10, 20>> = A,
    <<30, 40, 50>> = B,
    <<_:1/binary, C:4/binary>> = Bin,
    <<20, 30, 40, 50>> = C,
    ok.

%% 3. Extracting integers of various sizes
test_integer_extraction() ->
    Bin = <<16#DEADBEEF:32>>,
    <<V:32>> = Bin,
    16#DEADBEEF = V,
    <<H:16, L:16>> = Bin,
    16#DEAD = H,
    16#BEEF = L,
    <<B1:8, B2:8, B3:8, B4:8>> = Bin,
    16#DE = B1,
    16#AD = B2,
    16#BE = B3,
    16#EF = B4,
    ok.

%% 4. Skip bytes and extract tail
test_skip_and_tail() ->
    Bin = <<1, 2, 3, 4, 5, 6, 7, 8>>,
    <<_:3/binary, Tail/binary>> = Bin,
    <<4, 5, 6, 7, 8>> = Tail,
    <<_:7/binary, Last/binary>> = Bin,
    <<8>> = Last,
    <<_:8/binary, Empty/binary>> = Bin,
    <<>> = Empty,
    ok.

%% 5. Nested binary matching (match inside match)
test_nested_match() ->
    Outer = <<0, 3, 10, 20, 30, 99>>,
    <<_Tag:8, Len:8, Payload:Len/binary, Trailer:8>> = Outer,
    <<10, 20, 30>> = Payload,
    99 = Trailer,
    3 = Len,
    ok.

%% 6. Verify that a failed match restores the offset (no side-effect)
test_match_failure_restores_offset() ->
    Bin = <<1, 2, 3>>,
    %% First clause fails, second succeeds; offset must be reset
    Result = case Bin of
        <<99, _/binary>> -> wrong;
        <<1, Rest/binary>> -> {ok, Rest}
    end,
    {ok, <<2, 3>>} = Result,
    ok.

%% 7. Empty binary edge cases
test_empty_binary() ->
    <<>> = <<>>,
    Bin = <<42>>,
    <<42, Rest/binary>> = Bin,
    <<>> = Rest,
    0 = byte_size(Rest),
    ok.

%% 8. Multiple fields with different types
test_multi_field() ->
    Bin = <<1, 0, 2, 0, 0, 0, 3, "hello">>,
    <<A:8, B:16, C:32, Str:5/binary>> = Bin,
    1 = A,
    2 = B,
    3 = C,
    <<"hello">> = Str,
    ok.

%% 9. Construct a binary, then immediately pattern-match it
test_binary_construction_then_match() ->
    Values = [10, 20, 30, 40, 50],
    Bin = list_to_binary(Values),
    <<F, S, T, Fo, Fi>> = Bin,
    10 = F, 20 = S, 30 = T, 40 = Fo, 50 = Fi,
    ok.

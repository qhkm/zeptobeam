-module(test_bs_nostdlib).
-export([test/0]).

%% Binary matching stress test — NO stdlib dependencies.
%% Can run on the bare emulator without loading lists.beam etc.
%% Tests the bs_match opcode subcommands: ensure_at_least, =:=, skip, get_tail.

test() ->
    ok = test_byte_literal_match(),
    ok = test_integer_sizes(),
    ok = test_binary_split(),
    ok = test_skip_tail(),
    ok = test_case_match(),
    ok = test_nested_binary(),
    ok = test_guard_byte_size(),
    ok = test_construction_roundtrip(),
    ok = test_single_byte(),
    ok = test_integer_boundaries(),
    ok = test_repeated_match(),
    ok = test_clause_isolation(),
    ok = test_all_zeros(),
    ok = test_all_ones(),
    ok = test_tail_single_byte(),
    ok = test_multi_clause_fallthrough(),
    passed.

%% Match byte literals
test_byte_literal_match() ->
    <<1, 2, 3>> = <<1, 2, 3>>,
    <<0>> = <<0>>,
    <<255>> = <<255>>,
    ok.

%% Extract integers of various bit widths
test_integer_sizes() ->
    <<V8:8>> = <<42>>,
    42 = V8,
    <<V16:16>> = <<1, 0>>,
    256 = V16,
    <<V32:32>> = <<0, 0, 1, 0>>,
    256 = V32,
    ok.

%% Split binary into parts
test_binary_split() ->
    Bin = <<10, 20, 30, 40, 50>>,
    <<A:8, B:8, _Rest/binary>> = Bin,
    10 = A,
    20 = B,
    <<_:16, C:8, _/binary>> = Bin,
    30 = C,
    ok.

%% Skip bytes and extract tail
test_skip_tail() ->
    <<_:24, Tail/binary>> = <<1, 2, 3, 4, 5>>,
    <<4, 5>> = Tail,
    <<_:40, Empty/binary>> = <<1, 2, 3, 4, 5>>,
    <<>> = Empty,
    ok.

%% Case expression with binary patterns — tests offset restore on mismatch
test_case_match() ->
    Bin = <<1, 100>>,
    R1 = case Bin of
        <<2, _/binary>> -> wrong;
        <<1, V/binary>> -> V
    end,
    <<100>> = R1,

    Bin2 = <<0, 0, 0, 5>>,
    R2 = case Bin2 of
        <<0, 0, 0, 0>> -> zero;
        <<0, 0, 0, N:8>> -> N
    end,
    5 = R2,
    ok.

%% Nested: length-prefixed payload
test_nested_binary() ->
    Pkt = <<3, 10, 20, 30>>,
    <<Len:8, Payload:Len/binary>> = Pkt,
    3 = Len,
    <<10, 20, 30>> = Payload,
    ok.

%% Guard using byte_size
test_guard_byte_size() ->
    Bin = <<1, 2, 3>>,
    true = (byte_size(Bin) =:= 3),
    ok.

%% Construct then immediately decompose
test_construction_roundtrip() ->
    A = 16#CA,
    B = 16#FE,
    Bin = <<A:8, B:8>>,
    <<X:8, Y:8>> = Bin,
    16#CA = X,
    16#FE = Y,
    ok.

%% --- Additional edge cases ---

%% 9. Single-byte binary edge case
test_single_byte() ->
    <<V:8>> = <<0>>,
    0 = V,
    <<V2:8>> = <<255>>,
    255 = V2,
    ok.

%% 10. Max value integers at various widths
test_integer_boundaries() ->
    <<127:8/signed>> = <<127>>,
    <<255:8/unsigned>> = <<255>>,
    <<0:8>> = <<0>>,
    <<V:16>> = <<255, 255>>,
    65535 = V,
    ok.

%% 11. Repeated matching on the same binary (offset independence)
test_repeated_match() ->
    Bin = <<1, 2, 3>>,
    <<A:8, _/binary>> = Bin,
    <<B:8, _/binary>> = Bin,
    A = B,
    1 = A,
    ok.

%% 12. Match then rebind (no variable leaking between clauses)
test_clause_isolation() ->
    R = match_first(<<10, 20>>, <<30, 40>>),
    {10, 30} = R,
    ok.

match_first(<<A:8, _/binary>>, <<B:8, _/binary>>) -> {A, B}.

%% 13. Binary of all zeros
test_all_zeros() ->
    Bin = <<0, 0, 0, 0>>,
    <<0, 0, 0, 0>> = Bin,
    <<V:32>> = Bin,
    0 = V,
    ok.

%% 14. Binary of all 0xFF
test_all_ones() ->
    Bin = <<255, 255, 255, 255>>,
    <<V:32/unsigned>> = Bin,
    16#FFFFFFFF = V,
    ok.

%% 15. Match tail of length 1
test_tail_single_byte() ->
    <<_, T/binary>> = <<99, 42>>,
    <<42>> = T,
    1 = byte_size(T),
    ok.

%% 16. Deeply nested case (multiple fallthrough)
test_multi_clause_fallthrough() ->
    R = classify(<<0>>),
    zero = R,
    one = classify(<<1>>),
    two = classify(<<2>>),
    other = classify(<<99>>),
    ok.

classify(<<0>>) -> zero;
classify(<<1>>) -> one;
classify(<<2>>) -> two;
classify(<<_>>) -> other.

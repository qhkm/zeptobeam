# OTP 28 Parity Targets

## Tier 1: Kitchen-Sink Conformance (our test modules)
- smoke:test/0 ✅
- test2:test/0 ✅
- test_bs_nostdlib:test/0 ✅
- parity_maps:test/0 (new)
- parity_floats:test/0 (new)
- parity_exceptions:test/0 (new)
- parity_binary:test/0 (new)

## Tier 2: Selected stdlib calls
- string:tokens/2, string:join/2, string:trim/1
- proplists:get_value/3, proplists:get_all_values/2
- lists:sort/1, lists:map/2, lists:foldl/3, lists:filter/2, lists:flatten/1
- maps:get/2, maps:put/3, maps:merge/2, maps:keys/1, maps:values/1
- unicode:characters_to_binary/1
- io_lib:format/2

## Tier 3: Full stdlib module loading
- Load and call exported functions from each target module without panic.

## Pass Criteria
- Tier 1: 100% pass (all test modules return normally)
- Tier 2: 95% pass (diff test matches Erlang VM output)
- Tier 3: 80% pass (no panics, correct output where exercised)

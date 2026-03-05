# OTP28 Smoke-Test Fixes: Why Each Change Works

## Context
The runtime previously loaded OTP28 BEAM files but failed during end-to-end execution (`test2:test/0` and `bs_match_bin_SUITE`) due loader compatibility gaps, missing opcodes, and missing BIFs.

This note links each update to the concrete failure mode it resolved.

## Fix-by-Fix Rationale

1. **BEAM atom-table support for OTP28 (`AtU8`, compact atom lengths)**
- Failure before: loader mis-read atom chunks and produced invalid atom indexes.
- Why this works: OTP28 encodes atoms differently than older OTP targets; decoding the new format restores correct atom table indexing.

2. **Literal chunk support for uncompressed literals (`uncomp_sz == 0`)**
- Failure before: literal loading failed when OTP emitted non-compressed literal data.
- Why this works: OTP25+ can store literals uncompressed; accepting this path reads literal payload correctly.

3. **Ignore newer metadata chunks (`Docs`, `Meta`, `Type`)**
- Failure before: parser treated unknown chunks as fatal/unsupported.
- Why this works: these chunks are non-execution metadata; skipping them preserves executable sections (`Code`, `LitT`, etc.) without semantic loss.

4. **ETF decoder support for UTF-8 atom tags (`SMALL_ATOM_UTF8_EXT`, `ATOM_UTF8_EXT`)**
- Failure before: external-term decoding failed on modern atom tags.
- Why this works: OTP28 emits UTF-8 atom forms; decoding these tags restores term decode compatibility.

5. **Compact-term reader support for `TypedRegister` and `AllocList`**
- Failure before: code stream decode desynchronized on OTP28 compact operands.
- Why this works: these operand forms are now valid in generated code; parsing them preserves opcode argument alignment.

6. **Correct extended-list interpretation (not all ext-lists are jump tables)**
- Failure before: opcode stream misalignment when register/segment lists were parsed as jump-table pairs.
- Why this works: opcodes like `init_yregs`, `make_fun3`, `bs_create_bin`, `update_record`, `bs_match` carry plain lists; parsing them as plain lists prevents decoder drift.

7. **Recursive load-time term resolution for nested literals/atoms**
- Failure before: nested tuple/list terms retained unresolved load-time placeholders.
- Why this works: resolving recursively ensures runtime terms contain final `Term` values at all nesting levels.

8. **BitReader implementation and binary backend `get_bit_reader()` support**
- Failure before: unaligned binary operations could not read data correctly.
- Why this works: bit-accurate reads are required when binary slices start at non-byte offsets.

9. **`bs_match` opcode implementation**
- Failure before: hard stop on `Opcode ... bs_match not implemented`.
- Why this works: matcher command execution (`ensure_at_least`, `ensure_exactly`, etc.) now advances/fails match state as OTP expects.

10. **`bs_create_bin` opcode implementation**
- Failure before: hard stop on `Opcode ... bs_create_bin not implemented`.
- Why this works: OTP28 emits `bs_create_bin` for binary construction; implementing segment handling (`integer`, `binary`) unblocks modern binary builds.

11. **`bs_skip_bits2` opcode implementation**
- Failure before: panic on `Opcode 120 'bs_skip_bits2' not implemented`.
- Why this works: this opcode is required for matcher offset movement in bit-level closures.

12. **Missing BIFs added (`binary_to_list/1`, bitwise ops, `div/2`, `rem/2`)**
- Failure before: `BifNotFound(...)` during `bs_match_bin_SUITE` helper flows.
- Why this works: OTP-compiled helper functions use these BIFs heavily; wiring them restores expected arithmetic/bit-manip behavior.

13. **`binary_to_list/1` and `list_to_binary/1` made robust for empty/unaligned binaries**
- Failure before: badarg/panics on values like `<<>>` or byte-sized but unaligned sub-binaries.
- Why this works: byte-sized data can be reconstructed via bit-reader even when byte-reader is unavailable.

14. **`md5/1` updated to handle byte-sized unaligned binaries**
- Failure before: md5 path rejected binaries lacking direct byte-reader access.
- Why this works: reading via bit-reader preserves byte semantics whenever total bit size is divisible by 8.

15. **Match-state `reset()` now resets offset (instead of TODO/no-op logging)**
- Failure before: repeated noisy logs and risk of stale offsets when reusing match contexts.
- Why this works: reset semantics now match expected behavior for fresh match passes.

16. **Scheduler empty-run-queue behavior fixed**
- Failure before: busy-loop with no runnable processes; `erlexec` could hang after normal completion.
- Why this works: returning `None` when queues are empty lets VM dispatch terminate cleanly.

## Result
With these fixes, smoke execution reaches normal process completion and exits cleanly:
- `Process #Pid<0> end of life (return on empty stack) x0=ok`
- `scheduler: Terminating pid #Pid<0> reason=<exit>:normal`
- `erlexec: Finished.`

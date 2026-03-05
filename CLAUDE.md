# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

ErlangRT is an alternative Erlang BEAM runtime (emulator) written in Rust. It loads and executes compiled `.beam` files. The project targets OTP 22+ compatibility and is in a proof-of-concept stage (~45% opcode coverage, ~74 of 168 opcodes).

## Build Commands

Requires Rust **nightly** toolchain (uses `#![feature(ptr_metadata)]`).

```bash
# First-time setup: initialize OTP submodule and build OTP stdlib/preloaded BEAM files
make otp

# Build (runs codegen then cargo build with nightly)
make build

# Build test .erl files in priv/
make build_tests

# Run the emulator with test args
make run

# Run tests (ct_run binary with RUST_BACKTRACE=1)
make test

# Generate Rust code from .tab files (opcodes, atoms, BIFs, VM dispatch)
make codegen

# Generate docs
make doc
```

All `cargo` commands must use `+nightly` (the Makefile handles this).

## Workspace Structure

Cargo workspace with 3 crates:

- **`lib-erlangrt`** — Core library: VM, BEAM loader, term representation, opcodes, native functions (BIFs), heap/GC, scheduler. This is where nearly all logic lives.
- **`erlexec`** — Binary entry point (`erl` equivalent). Calls `start_emulator()` with hardcoded `test2:test/0`.
- **`ct_run`** — Common Test runner entry point. Sets up search paths including `otp/erts/preloaded/ebin/` and `otp/lib/stdlib/ebin/`.

## Code Generation

Python scripts in `lib-erlangrt/codegen/` generate Rust source files from `.tab` definition files:

| Script | Input | Output | Purpose |
|--------|-------|--------|---------|
| `create_gen_op.py` | OTP genop.tab + `implemented_ops.tab` | `beam/gen_op.rs` | Opcode constants, arity map, names |
| `create_gen_atoms.py` | `atoms.tab` | `emulator/gen_atoms.rs` | Pre-registered atom constants |
| `create_gen_native_fun.py` | `implemented_native_funs.tab` | `native_fun/gen_native_fun.rs` | BIF registry mappings |
| `create_vm_dispatch.py` | `implemented_ops.tab` | `beam/vm_dispatch.rs` | Opcode dispatch match arms |

Run `make codegen` from root after modifying any `.tab` file or codegen script.

## Architecture (lib-erlangrt)

### Term Representation (`term/`)
`Term` is a tagged machine-word-sized value using primary tag bits. Stores either immediate values (small ints, atoms, pids, nil) or pointers to heap-allocated boxed terms (tuples, binaries, bignums, closures, maps). The `PrimaryTag` enum defines tag bits. `SpecialTag` and `SpecialReg` handle registers and load-time values.

### BEAM Loader (`beam/loader/`)
3-stage pipeline:
1. **Stage 1** (`beam_file.rs`): Parse raw BEAM file chunks (atoms, code, literals, etc.)
2. **Stage 2** (`impl_stage2.rs`): Register atoms with VM atom table, fill lambdas
3. **Finalize** (`impl_parse_code.rs` → `impl_fix_labels.rs` → `impl_setup_imports.rs`): Parse raw opcodes into internal format, resolve labels to code pointers, set up import table. Returns a `Module` object.

### VM Core (`emulator/`)
- **`vm/mod.rs`** — `VM` struct: owns `CodeServer`, `Scheduler`, `ProcessRegistry`, `BinaryHeapOwner`. Entry point is `tick()` → `dispatch()`.
- **`process.rs`** — `Process` struct: owns heap, mailbox, runtime context, pid.
- **`runtime_ctx/mod.rs`** — `RuntimeContext`: IP, CP, X registers (256), FP registers (8), reductions counter. Swapped in/out per process scheduling.
- **`scheduler.rs`** — Round-robin scheduler with priority queues (Low/Normal/High). Processes get 200 reductions per time slice.
- **`code_srv/mod.rs`** — `CodeServer`: module registry with versioned current/old generations. Handles MFA lookup (BIF → native fn, BEAM → code pointer) with auto-loading.
- **`heap/`** — Process heap with copying GC (`gc_copying.rs`), stack (Y registers stored on stack).

### VM Dispatch Loop (`beam/vm_loop.rs`)
`VM::dispatch()` picks next process from scheduler, fetches opcodes in a loop, calls `dispatch_op_inline()` (generated), handles `DispatchResult` (Normal/Yield/Finished), and tracks reductions.

### Opcode Implementations (`beam/opcodes/`)
Grouped by category: `op_data` (move/load), `op_execution` (call/return/jump), `op_list`, `op_tuple`, `op_memory` (alloc/dealloc/GC), `op_message` (send/receive), `op_predicates` (is_eq/comparisons), `op_try_catch`, `op_fun` (closures), `op_type_checks`, `binary/` (binary matching/construction).

### Native Functions / BIFs (`native_fun/`)
Rust implementations of Erlang BIFs organized by module: `erlang/` (arithmetic, compare, process, tuple, list, etc.), `erts_internal/`, `lists/`. Signature: `fn(vm: &mut VM, proc: &mut Process, args: &[Term]) -> RtResult<Term>`.

### Error Handling (`fail/`)
`RtErr` enum covers all runtime errors. `RtResult<T>` is the standard Result type. Exceptions use `RtErr::Exception(ExceptionType, Term)`.

## Feature Flags (Cargo features in lib-erlangrt)

Defined in `lib-erlangrt/Cargo.toml`. Toggle tracing output:
- `trace_opcode_execution` — prints every opcode as it executes
- `trace_register_changes` — prints X register writes
- `trace_stack_changes` — prints stack modifications
- `trace_calls` — logs native and BEAM function calls
- `trace_beam_loader` — prints BEAM loading debug info
- `trace_comparisons` — logs failed comparisons with args
- `fancy_string_quotes` — uses unicode quotes for string display

## Adding New BIFs

1. Find the BIF in `codegen/otp22/bif.tab` (or similar), copy the line
2. Add it to `lib-erlangrt/codegen/implemented_native_funs.tab`
3. Run `make codegen` — compiler errors will show the missing function
4. Implement in the appropriate `native_fun/` submodule

## Adding New Opcodes

1. Find the opcode in `codegen/otp22/genop.tab`
2. Add it to `lib-erlangrt/codegen/implemented_ops.tab`
3. Run `make codegen` — compiler errors will show the missing handler
4. Implement in the appropriate `beam/opcodes/` submodule

## Code Style

- Rust edition 2018
- `rustfmt.toml`: 90 char max width, 2-space indentation, no hard tabs
- Modules use `fn module() -> &'static str` for log prefixes
- Heavy use of `unsafe` for performance-critical pointer operations (register access, code pointer manipulation, heap operations)

## OTP Dependency

The `otp/` git submodule points to Erlang/OTP source. Required for preloaded BEAM modules (`otp/erts/preloaded/ebin/`) and stdlib (`otp/lib/stdlib/ebin/`). Test `.erl` files live in `priv/` and are compiled with the OTP `erlc`.

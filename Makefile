.PHONY: build codegen ct submodule otp test test-rust test-beam test-all test-parity

build: codegen
	cargo +nightly build

build_tests:
	cd priv && $(MAKE)

codegen:
	cd lib-erlangrt && $(MAKE) codegen

ct: build
	mkdir -p tmp; cd tmp && ../target/debug/ct_run 1 2 3 -erl_args 4 5 6

# Run default module (test2:test)
run: build build_tests
	set -a && . ./.env && set +a && cargo +nightly run --bin erlexec

# Run a specific module:  make run-mod M=smoke F=test
run-mod: build build_tests
	set -a && . ./.env && set +a && cargo +nightly run --bin erlexec -- -m $(M) -f $(or $(F),test)

# Rust unit/integration tests (423 tests)
test-rust:
	cargo +nightly test -p erlangrt --lib

# BEAM integration tests — run each test module through the emulator
test-beam: build build_tests
	@set -a && . ./.env && set +a && \
	failed=0; passed=0; \
	for mod in smoke test2 test_bs_nostdlib; do \
		printf "  %-25s" "$$mod:test/0..."; \
		output=$$(cargo +nightly run --bin erlexec -- -m $$mod -f test 2>&1); \
		if echo "$$output" | grep -q "reason=<exit>:normal"; then \
			echo "OK"; \
			passed=$$((passed + 1)); \
		else \
			echo "FAIL"; \
			echo "$$output" | grep -E "reason=|error|panic" | head -3; \
			failed=$$((failed + 1)); \
		fi; \
	done; \
	echo ""; \
	echo "BEAM tests: $$passed passed, $$failed failed"; \
	test $$failed -eq 0

# Run everything: Rust tests + BEAM integration tests
test-all: test-rust test-beam

# Alias for backward compat
test: build build_tests
	set -a && . ./.env && set +a && RUST_BACKTRACE=1 cargo +nightly run --bin ct_run

# Graphical user interface for GDB - Gede
.PHONY: test-gede
test-gede: build
	RUST_BACKTRACE=1 gede --args target/debug/ct_run

# Differential test: compare Zeptobeam vs Erlang VM
test-diff: build build_tests
	@set -a && . ./.env 2>/dev/null && set +a; \
	failed=0; passed=0; \
	for mod in smoke test2 test_bs_nostdlib; do \
		bash scripts/diff_test.sh $$mod test 2>&1 | tail -5; \
		if echo "$$output" | grep -q "PASS"; then \
			passed=$$((passed + 1)); \
		else \
			failed=$$((failed + 1)); \
		fi; \
	done; \
	echo ""; \
	echo "Diff tests: $$passed passed, $$failed failed"

# Full OTP 28 parity suite
.PHONY: test-parity
test-parity: build build_tests
	@set -a && . ./.env 2>/dev/null && set +a; \
	passed=0; failed=0; \
	echo "=== Tier 1: Kitchen Sink ==="; \
	for mod in parity_maps parity_floats parity_exceptions parity_binary parity_numeric; do \
		printf "  %-25s" "$$mod..."; \
		output=$$(bash scripts/diff_test.sh $$mod test 2>&1); \
		if echo "$$output" | grep -q "PASS"; then \
			echo "OK"; passed=$$((passed + 1)); \
		else \
			echo "FAIL"; failed=$$((failed + 1)); \
		fi; \
	done; \
	echo ""; \
	echo "=== Tier 2: stdlib ==="; \
	for mod in tier2_stdlib; do \
		printf "  %-25s" "$$mod..."; \
		output=$$(bash scripts/diff_test.sh $$mod test 2>&1); \
		if echo "$$output" | grep -q "PASS"; then \
			echo "OK"; passed=$$((passed + 1)); \
		else \
			echo "FAIL"; failed=$$((failed + 1)); \
		fi; \
	done; \
	echo ""; \
	echo "Parity tests: $$passed passed, $$failed failed"; \
	test $$failed -eq 0

otp:
	git submodule init && git submodule update && cd otp && MAKE_FLAGS=-j8 ./otp_build setup


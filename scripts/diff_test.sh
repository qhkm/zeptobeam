#!/usr/bin/env bash
# Differential test: run same .beam on Erlang VM and Zeptobeam, compare results.
# Usage: ./scripts/diff_test.sh <module> <function>
# Requires: erlc on PATH, .env with ZEPTOBEAM paths

set -euo pipefail

MOD="${1:?Usage: diff_test.sh <module> [function]}"
FUN="${2:-test}"
PRIV_DIR="priv"
WORKLOAD_DIR="priv/parity_workloads"
DIFF_OUT="tmp/diff_results"
mkdir -p "$DIFF_OUT"

# Find the .erl file
ERL_FILE=""
for dir in "$PRIV_DIR" "$WORKLOAD_DIR"; do
  if [ -f "$dir/$MOD.erl" ]; then
    ERL_FILE="$dir/$MOD.erl"
    break
  fi
done

if [ -z "$ERL_FILE" ]; then
  echo "ERROR: $MOD.erl not found in $PRIV_DIR or $WORKLOAD_DIR"
  exit 1
fi

# Compile with OTP erlc
echo "Compiling $ERL_FILE..."
erlc -o "$PRIV_DIR" "$ERL_FILE" 2>"$DIFF_OUT/${MOD}_compile.log" || {
  echo "COMPILE FAIL: see $DIFF_OUT/${MOD}_compile.log"
  exit 1
}

# Run on reference Erlang VM
echo "Running on Erlang VM: ${MOD}:${FUN}/0..."
ERL_RESULT=$(erl -noshell -pa "$PRIV_DIR" -eval "
  try
    Result = ${MOD}:${FUN}(),
    io:format(\"RESULT:~p~n\", [Result]),
    halt(0)
  catch
    Class:Reason ->
      io:format(\"EXCEPTION:~p:~p~n\", [Class, Reason]),
      halt(1)
  end." 2>&1) || true
echo "$ERL_RESULT" > "$DIFF_OUT/${MOD}_erlang.out"

# Run on Zeptobeam
echo "Running on Zeptobeam: ${MOD}:${FUN}/0..."
set -a && source .env 2>/dev/null && set +a || true
ZEPTO_RESULT=$(cargo +nightly run --bin erlexec -- -m "$MOD" -f "$FUN" 2>&1) || true
echo "$ZEPTO_RESULT" > "$DIFF_OUT/${MOD}_zeptobeam.out"

# Extract result lines for comparison
ERL_LINE=$(echo "$ERL_RESULT" | grep -E "^RESULT:|^EXCEPTION:" | head -1)
# Zeptobeam: check for normal exit or error
ZEPTO_EXIT="UNKNOWN"
if echo "$ZEPTO_RESULT" | grep -q "reason=<exit>:normal"; then
  ZEPTO_EXIT="NORMAL"
elif echo "$ZEPTO_RESULT" | grep -q "panic\|not implemented"; then
  ZEPTO_EXIT="PANIC"
  # Extract the missing opcode or BIF if possible
  MISSING=$(echo "$ZEPTO_RESULT" | grep -oE "Opcode [^ ]+ '[^']+' not implemented|not yet implemented" | head -1)
  if [ -n "$MISSING" ]; then
    ZEPTO_EXIT="MISSING: $MISSING"
  fi
elif echo "$ZEPTO_RESULT" | grep -q "reason=<exit>"; then
  ZEPTO_EXIT="EXCEPTION"
fi

# Report
echo ""
echo "=== DIFFERENTIAL RESULT: $MOD:$FUN/0 ==="
echo "  Erlang VM : $ERL_LINE"
echo "  Zeptobeam : $ZEPTO_EXIT"
if [ "$ZEPTO_EXIT" = "NORMAL" ] && echo "$ERL_LINE" | grep -q "^RESULT:"; then
  echo "  Status    : PASS (both completed normally)"
elif [ "$ZEPTO_EXIT" = "NORMAL" ] && echo "$ERL_LINE" | grep -q "^EXCEPTION:"; then
  echo "  Status    : MISMATCH (Erlang threw, Zeptobeam succeeded)"
else
  echo "  Status    : FAIL"
fi
echo ""
echo "Full outputs in: $DIFF_OUT/"

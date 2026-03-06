#!/usr/bin/env bash
# Reports opcode and BIF implementation coverage.

set -euo pipefail

# Count implemented opcodes (non-comment, non-empty lines in implemented_ops.tab)
IMPL=$(grep -cv "^#\|^$" lib-erlangrt/codegen/implemented_ops.tab || true)

# Count total opcodes defined in OTP genop.tab
# Format: "NUMBER: opcode_name/ARITY"
GENOP_FILE=""
for f in lib-erlangrt/codegen/otp28/genop.tab lib-erlangrt/codegen/otp22/genop.tab; do
  if [ -f "$f" ]; then
    GENOP_FILE="$f"
    break
  fi
done

if [ -n "$GENOP_FILE" ]; then
  TOTAL=$(grep -cE "^[0-9]+:" "$GENOP_FILE" || true)
else
  TOTAL="?"
fi

# Count registered BIFs across all native_fun modules
BIFS=$(grep -r "NativeFnEntry::with_str" lib-erlangrt/src/native_fun/*/mod.rs 2>/dev/null | wc -l | tr -d ' ')

echo "=== Opcode Coverage ==="
if [ "$TOTAL" != "?" ] && [ "$TOTAL" -gt 0 ]; then
  PCT=$(( IMPL * 100 / TOTAL ))
  echo "  Implemented: $IMPL / $TOTAL ($PCT%)"
else
  echo "  Implemented: $IMPL (total unknown)"
fi
echo "  BIFs registered: $BIFS"
echo ""

#!/usr/bin/env bash
# Proves that a declared rename moves state instead of destroying resources.
#
# Unlike check.sh, this script APPLIES. It applies only resources of the
# `random` provider, which exist nowhere but in a local state file under
# .kiln-check/move/. No cloud API is called and no credentials are used.
#
#   1. build v1 and apply it                -> state holds the old addresses
#   2. build v2 (renames, declared)         -> the plan must be moves only
#   3. build v3 (a rename, not declared)    -> the build must stop
#   4. apply v2, then plan again            -> nothing left to do

set -uo pipefail
cd "$(dirname "$0")/.."

TOFU="${TOFU:-tofu}"
KILN=./target/release/kiln
OUT=.kiln-check
W="$OUT/move"
FIX=tests/fixtures/move
mkdir -p "$OUT/plugin-cache"
export TF_PLUGIN_CACHE_DIR="$PWD/$OUT/plugin-cache"
export TF_IN_AUTOMATION=1

{
  fails=0
  run() {
    local label="$1"; shift
    if "$@" >"$OUT/last.log" 2>&1; then
      echo "PASS  $label"
    else
      echo "FAIL  $label"
      sed 's/^/      /' "$OUT/last.log"
      fails=$((fails + 1))
    fi
  }
  must_fail_with() {
    local label="$1" pattern="$2"; shift 2
    if "$@" >"$OUT/last.log" 2>&1; then
      echo "FAIL  $label (the command succeeded)"; fails=$((fails + 1))
    elif grep -q "$pattern" "$OUT/last.log"; then
      echo "PASS  $label"
    else
      echo "FAIL  $label (failed, but not with $pattern)"; sed 's/^/      /' "$OUT/last.log"; fails=$((fails + 1))
    fi
  }

  echo "kiln move test  $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  "$TOFU" version 2>&1 | head -1
  run "cargo build --release" cargo build --release
  rm -rf "$W"; mkdir -p "$W"

  echo; echo "=== 1. create state at the old addresses ==="
  cp "$FIX/v1.kiln" "$W/main.kiln"
  run "kiln build v1" "$KILN" build "$W/main.kiln"
  run "init" "$TOFU" -chdir="$W/out" init -input=false -no-color
  run "apply v1 (random provider, local state)" "$TOFU" -chdir="$W/out" apply -auto-approve -input=false -no-color

  echo; echo "=== 2. declared renames must plan as moves only ==="
  cp "$FIX/v2.kiln" "$W/main.kiln"
  run "kiln build v2" "$KILN" build "$W/main.kiln"
  run "plan v2" "$TOFU" -chdir="$W/out" plan -input=false -no-color -out=plan.bin
  "$TOFU" -chdir="$W/out" show -json plan.bin >"$W/plan.json" 2>/dev/null
  run "3 resources moved, none created, changed or destroyed" python3 tools/movecheck.py "$W/plan.json" 3

  echo; echo "=== 3. an undeclared rename must stop the build ==="
  cp "$FIX/v3.kiln" "$W/main.kiln"
  must_fail_with "kiln build v3 is refused with K0500" "K0500" "$KILN" build "$W/main.kiln"

  echo; echo "=== 4. apply the moves; nothing remains ==="
  run "apply v2" "$TOFU" -chdir="$W/out" apply -auto-approve -input=false -no-color
  run "plan is empty afterwards" "$TOFU" -chdir="$W/out" plan -input=false -no-color -detailed-exitcode

  echo
  echo "RESULT: $fails failure(s)"
  exit "$fails"
} 2>&1 | tee "$OUT/move-report.txt"
exit "${PIPESTATUS[0]}"

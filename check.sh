#!/usr/bin/env bash
# Engine-side checks. Needs cargo and OpenTofu (or Terraform: TOFU=terraform ./check.sh).
#
#   ./check.sh           build, test, and `init` + `validate` every example
#   PLAN=1 ./check.sh    additionally plan the generated and the hand-written
#                        configuration and compare the two plans (needs credentials)
#
# Results go to .kiln-check/report.txt; provider schemas to .kiln-check/schema-*.json.
# This script never applies anything. The state-move test, which applies
# `random` resources to a local state file only, is separate: tools/move-test.sh

set -uo pipefail
cd "$(dirname "$0")"

TOFU="${TOFU:-tofu}"
OUT=".kiln-check"
mkdir -p "$OUT/plugin-cache"
export TF_PLUGIN_CACHE_DIR="$PWD/$OUT/plugin-cache"
export TF_IN_AUTOMATION=1

{
  fails=0
  step() { echo; echo "=== $* ==="; }
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

  echo "kiln check  $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  "$TOFU" version 2>&1 | head -2
  cargo --version

  step "build and test"
  run "cargo build --release" cargo build --release
  run "cargo test" cargo test
  KILN=./target/release/kiln

  plan() { # <dir> <label>: plan a configuration and save the plan as JSON
    run "$1: plan" "$TOFU" -chdir="$1" plan -input=false -no-color -lock=false -out=plan.bin
    "$TOFU" -chdir="$1" show -json plan.bin >"$OUT/plan-$2.json" 2>/dev/null
  }

  # Hand-written Kiln stacks: committed output is current and valid.
  for src in examples/*/main.kiln; do
    ex="$(dirname "$src")"
    name="$(basename "$ex")"
    step "$ex"
    run "committed output and lock are current" "$KILN" build "$src" --check
    for d in "$ex/out" "$ex/original"; do
      [ -d "$d" ] || continue
      run "$d: init" "$TOFU" -chdir="$d" init -backend=false -input=false -no-color
      run "$d: validate" "$TOFU" -chdir="$d" validate -no-color
    done
    if "$TOFU" -chdir="$ex/out" providers schema -json >"$OUT/schema-$name.json" 2>"$OUT/last.log"; then
      echo "PASS  provider schema written to $OUT/schema-$name.json"
    else
      echo "FAIL  provider schema"; sed 's/^/      /' "$OUT/last.log"; fails=$((fails + 1))
    fi
    if [ "${PLAN:-0}" = "1" ] && [ -d "$ex/original" ]; then
      plan "$ex/out" "$name-out"
      plan "$ex/original" "$name-original"
      run "plans are identical" python3 tools/plandiff.py \
        "$OUT/plan-$name-out.json" "$OUT/plan-$name-original.json"
    fi
  done

  # HCL originals: import each one afresh; the result must be valid and,
  # with PLAN=1, plan exactly as the HCL does.
  for orig in examples/*/original; do
    [ -d "$orig" ] || continue
    name="$(basename "$(dirname "$orig")")"
    imp="$OUT/import-$name"
    step "import $orig"
    rm -rf "$imp"
    run "kiln import" "$KILN" import "$orig" -o "$imp"
    [ -d "$imp/out" ] || continue
    run "$orig: init" "$TOFU" -chdir="$orig" init -backend=false -input=false -no-color
    run "imported: init" "$TOFU" -chdir="$imp/out" init -backend=false -input=false -no-color
    run "imported: validate" "$TOFU" -chdir="$imp/out" validate -no-color
    if [ "${PLAN:-0}" = "1" ]; then
      plan "$imp/out" "$name-imported"
      plan "$orig" "$name-original"
      run "imported plan is identical to the HCL plan" python3 tools/plandiff.py \
        "$OUT/plan-$name-imported.json" "$OUT/plan-$name-original.json"
    fi
  done

  echo
  echo "RESULT: $fails failure(s)"
  exit "$fails"
} 2>&1 | tee "$OUT/report.txt"
exit "${PIPESTATUS[0]}"

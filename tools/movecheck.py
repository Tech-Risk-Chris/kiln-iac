#!/usr/bin/env python3
"""Check a `tofu show -json` plan: every resource is moved, none is created, changed or destroyed."""
import json
import sys


def main():
    if len(sys.argv) != 3:
        sys.exit("usage: movecheck.py <plan.json> <expected-number-of-moves>")
    with open(sys.argv[1]) as f:
        plan = json.load(f)
    expected = int(sys.argv[2])
    bad = 0
    moves = 0
    for rc in plan.get("resource_changes", []):
        actions = rc["change"]["actions"]
        prev = rc.get("previous_address")
        if actions != ["no-op"]:
            print(f"NOT A NO-OP: {rc['address']} {actions}")
            bad += 1
        if prev:
            print(f"moved {prev} -> {rc['address']}")
            moves += 1
    if moves != expected:
        print(f"expected {expected} moves, found {moves}")
        bad += 1
    print(f"{moves} moved, {bad} problem(s)")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()

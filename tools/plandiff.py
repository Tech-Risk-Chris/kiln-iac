#!/usr/bin/env python3
"""Compare two `tofu show -json` plans: same addresses, same actions, same planned values."""
import json
import sys


def load(path):
    with open(path) as f:
        plan = json.load(f)
    changes = {}
    for rc in plan.get("resource_changes", []):
        ch = rc["change"]
        changes[rc["address"]] = (ch.get("actions"), ch.get("after"), ch.get("after_unknown"))
    outputs = {
        k: (v.get("actions"), v.get("after"), v.get("after_unknown"), v.get("after_sensitive"))
        for k, v in plan.get("output_changes", {}).items()
    }
    return changes, outputs


def diff(kind, a, b):
    bad = 0
    for key in sorted(set(a) | set(b)):
        if key not in a:
            print(f"{kind} only in second plan: {key}")
        elif key not in b:
            print(f"{kind} only in first plan: {key}")
        elif a[key] != b[key]:
            print(f"{kind} differs: {key}")
            print("  first:  " + json.dumps(a[key], sort_keys=True))
            print("  second: " + json.dumps(b[key], sort_keys=True))
        else:
            continue
        bad += 1
    return bad


def main():
    if len(sys.argv) != 3:
        sys.exit("usage: plandiff.py <plan-a.json> <plan-b.json>")
    (ra, oa), (rb, ob) = load(sys.argv[1]), load(sys.argv[2])
    bad = diff("resource", ra, rb) + diff("output", oa, ob)
    print(f"{len(ra)} resources, {len(oa)} outputs compared; {bad} difference(s)")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()

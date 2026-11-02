#!/usr/bin/env python3
"""Validate an updater feed latest.json (task U1).

Fails (exit 1) if the feed is malformed: missing version/platforms, a platform
entry without a non-empty signature or url, or no windows-x86_64 target (the
owner's primary platform must always be updatable). Run in the release workflow
after native/scripts/updater-latest-json.sh.

Usage: updater-check-latest-json.py <latest.json>
Schema: https://v2.tauri.app/plugin/updater/
"""
import json
import sys


def main(path: str) -> int:
    with open(path, encoding="utf-8") as f:
        doc = json.load(f)
    for field in ("version", "platforms"):
        if not doc.get(field):
            print(f"error: latest.json missing '{field}'", file=sys.stderr)
            return 1
    platforms = doc["platforms"]
    if not isinstance(platforms, dict) or not platforms:
        print("error: latest.json has no platforms", file=sys.stderr)
        return 1
    for target, entry in platforms.items():
        for key in ("signature", "url"):
            if not entry.get(key):
                print(f"error: {target} missing '{key}'", file=sys.stderr)
                return 1
    if "windows-x86_64" not in platforms:
        print("error: windows-x86_64 target is required", file=sys.stderr)
        return 1
    print("latest.json OK:", ", ".join(sorted(platforms)))
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("usage: updater-check-latest-json.py <latest.json>", file=sys.stderr)
        sys.exit(2)
    sys.exit(main(sys.argv[1]))

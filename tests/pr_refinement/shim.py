#!/usr/bin/env python3
"""A stand-in for `pail` (the plugin's PAIL_BIN) that holds every write until the harness
releases it: `put` and `land` write what they were asked to (argv, and the files put would
store) to $PR_CTL/<client>.blocked.json, then wait for $PR_CTL/<client>.go. Everything else
(reads) goes straight through to the real pail ($PR_REAL_PAIL)."""
import json
import os
import sys
import time

argv = sys.argv[1:]
real = os.environ["PR_REAL_PAIL"]
rest = argv[2:] if argv[:1] == ["--store"] else argv
if rest[:1] in (["put"], ["land"]):
    ctl, client = os.environ["PR_CTL"], os.environ["PR_CLIENT"]
    files = {}
    for i, a in enumerate(rest):
        if a == "--file":
            path, src = rest[i + 1].split("=", 1)
            with open(src, "rb") as f:
                files[path] = f.read().decode()
    tmp = os.path.join(ctl, client + ".blocked.tmp")
    with open(tmp, "w") as f:
        json.dump({"argv": rest, "files": files}, f)
    os.rename(tmp, os.path.join(ctl, client + ".blocked.json"))
    go = os.path.join(ctl, client + ".go")
    parent = os.getppid()
    while not os.path.exists(go):
        if os.getppid() != parent:  # the plugin was killed: do not wait for ever
            sys.exit(1)
        time.sleep(0.003)
    os.remove(go)
os.execv(real, [real] + argv)

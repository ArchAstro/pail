#!/usr/bin/env python3
"""Run extensions/pail-pr as `pail pr` would, with the clock fixed at $PR_NOW_MS milliseconds
(the plugin names a comment by the millisecond it was made in; a test picks it). Nothing else
differs from running the plugin itself."""
import os
import runpy
import sys
import time

t = (int(os.environ["PR_NOW_MS"]) + 0.5) / 1000
time.time = lambda: t
plugin = sys.argv[1]
sys.argv = [plugin] + sys.argv[2:]
runpy.run_path(plugin, run_name="__main__")

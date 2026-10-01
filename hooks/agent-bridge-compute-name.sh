#!/usr/bin/env bash
# Shared helper. It derives the unique agent-bridge name of this session from
# the hook JSON on stdin. Shape: claude-<cwd basename>-<4 hex of session_id>.
# The bridge accepts [a-z0-9_-]{1,64}.
# The hook JSON goes to python as argv, NOT as stdin. With `python3 -` the
# script source itself already consumes stdin.
set -euo pipefail

input=$(cat)

python3 - "$input" <<'PY'
import json, re, sys, os

data = json.loads(sys.argv[1])
sid = data.get("session_id", "") or "nosid"
cwd = data.get("cwd", "") or os.getcwd()

base = os.path.basename(cwd.rstrip("/")) or "root"
base = re.sub(r"[^a-z0-9-]", "-", base.lower()).strip("-") or "dir"
suffix = re.sub(r"[^a-f0-9]", "", sid.lower())[:4] or "0000"

print(f"claude-{base[:20]}-{suffix}")
PY

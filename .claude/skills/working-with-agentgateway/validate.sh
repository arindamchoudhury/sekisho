#!/usr/bin/env bash
# Validate an agentgateway local config (YAML or JSON) against agentgateway's
# published JSON schema at a given tag (default: Sekisho's pin).
#   bash .claude/skills/working-with-agentgateway/validate.sh config.yaml [v1.7.0]
# Needs python with jsonschema + pyyaml: set SEKISHO_PY to such an interpreter,
# or the script creates a venv in ${TMPDIR:-/tmp}/sekisho-agw-venv once.
set -euo pipefail
cfg="${1:?usage: validate.sh <config.yaml> [tag]}"
tag="${2:-v1.7.0}"
cache="${TMPDIR:-/tmp}/sekisho-agw-schema"
schema="$cache/config-$tag.schema.json"
mkdir -p "$cache"
if [ ! -s "$schema" ]; then
  gh api "repos/agentgateway/agentgateway/contents/schema/config.json?ref=$tag" \
    -H 'Accept: application/vnd.github.raw' > "$schema"
fi
py="${SEKISHO_PY:-}"
if [ -z "$py" ]; then
  venv="${TMPDIR:-/tmp}/sekisho-agw-venv"
  if [ -x "$venv/Scripts/python.exe" ]; then py="$venv/Scripts/python.exe"
  elif [ -x "$venv/bin/python" ]; then py="$venv/bin/python"
  else
    python -m venv "$venv"
    if [ -x "$venv/Scripts/python.exe" ]; then py="$venv/Scripts/python.exe"; else py="$venv/bin/python"; fi
    "$py" -m pip install -q jsonschema pyyaml
  fi
fi
"$py" -I - "$schema" "$cfg" <<'EOF'
import json, sys, yaml
from jsonschema import Draft202012Validator
schema = json.load(open(sys.argv[1], encoding="utf-8"))
doc = yaml.safe_load(open(sys.argv[2], encoding="utf-8"))
errs = sorted(Draft202012Validator(schema).iter_errors(doc), key=lambda e: list(e.absolute_path))
if not errs:
    print("VALID"); sys.exit(0)
for e in errs[:15]:
    print(f"ERROR at {'/'.join(map(str, e.absolute_path)) or '<root>'}: {e.message[:300]}")
sys.exit(1)
EOF

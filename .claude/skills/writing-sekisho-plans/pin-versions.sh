#!/usr/bin/env bash
# Print, for each crate, the newest stable (non-yanked, non-prerelease) version
# published at least MIN_AGE_DAYS ago (default 14), plus the newest overall.
#   bash .claude/skills/writing-sekisho-plans/pin-versions.sh tokio axum sqlx
set -uo pipefail
[ $# -eq 0 ] && { echo "usage: pin-versions.sh <crate> [crate...]" >&2; exit 2; }
min_age="${MIN_AGE_DAYS:-14}"
for c in "$@"; do
  curl -s -A 'sekisho-pin-versions (planning script)' "https://crates.io/api/v1/crates/$c" |
  python -c "
import sys, json, datetime
c, min_age = sys.argv[1], int(sys.argv[2])
try:
    d = json.load(sys.stdin)
except Exception:
    print(f'{c:24s} ERROR: no response'); sys.exit()
if 'versions' not in d:
    print(f'{c:24s} ERROR: {d.get(\"errors\", d)}'); sys.exit()
cut = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=min_age)
vs = [v for v in d['versions'] if not v['yanked'] and '-' not in v['num']]
when = lambda v: datetime.datetime.fromisoformat(v['created_at'].replace('Z', '+00:00'))
ok = [v for v in vs if when(v) <= cut]
pick = f\"{ok[0]['num']} ({ok[0]['created_at'][:10]})\" if ok else 'none old enough'
print(f'{c:24s} use {pick:28s} newest {vs[0][\"num\"] if vs else \"-\"}')
" "$c" "$min_age"
done

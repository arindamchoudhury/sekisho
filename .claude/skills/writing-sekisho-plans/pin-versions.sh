#!/usr/bin/env bash
# Sekisho's dependency rule: every crate version we build with, direct or
# indirect, was published on crates.io at least MIN_AGE_DAYS ago (default 14)
# and is not yanked.
#
#   bash .claude/skills/writing-sekisho-plans/pin-versions.sh tokio axum sqlx
#     For choosing versions: each crate's newest stable version that is old
#     enough ("use") and its newest stable version overall.
#
#   bash .claude/skills/writing-sekisho-plans/pin-versions.sh --lock [--since REV] [LOCKFILE]
#     For checking Cargo.lock (default: the repo root's) after any change to
#     it. Cargo resolves indirect dependencies to their newest compatible
#     versions, which can be hours old. Prints each locked version that is too
#     new or yanked, with a `cargo update --precise` command that pins it to
#     the newest old-enough compatible version, and exits 1 if there are any.
#     --since REV checks only versions not already in REV's Cargo.lock.
#     A pin can fail when another crate needs the newer version: then pin
#     that crate back too, or wait until the version is old enough.
#
# Exit: 0 all good, 1 a version breaks the rule, 2 usage or network error.
# crates.io asks API clients for at most one request a second; this script
# keeps to that, so a full --lock check takes about a second per package.
set -euo pipefail

usage() { sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }
[ $# -eq 0 ] && usage

since=""
lock=""
if [ "$1" = "--lock" ]; then
  shift
  if [ "${1:-}" = "--since" ]; then
    [ $# -ge 2 ] || usage
    since=$2
    shift 2
  fi
  [ $# -le 1 ] || usage
  lock=${1:-"$(git rev-parse --show-toplevel)/Cargo.lock"}
  [ -f "$lock" ] || { echo "no such lockfile: $lock" >&2; exit 2; }
  if [ -n "$since" ]; then
    # The same lockfile path at REV, relative to the repo root.
    rel=$(git ls-files --full-name -- "$lock" 2>/dev/null || true)
    old=$(git show "$since:${rel:-Cargo.lock}") || { echo "cannot read Cargo.lock at $since" >&2; exit 2; }
    export PIN_VERSIONS_SINCE_LOCK="$old" PIN_VERSIONS_SINCE_REV="$since"
  fi
fi

exec python -I - "${MIN_AGE_DAYS:-14}" "$lock" "$@" <<'PY'
import datetime, json, os, sys, time, tomllib, urllib.error, urllib.request

min_age, lock, crates = int(sys.argv[1]), sys.argv[2], sys.argv[3:]
cut = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=min_age)
UA = "sekisho-pin-versions (dependency age check)"
CRATES_IO = ("registry+https://github.com/rust-lang/crates.io-index", "sparse+https://index.crates.io/")
last = 0.0

def get(path):
    global last
    wait = 1.0 - (time.monotonic() - last)
    if wait > 0:
        time.sleep(wait)
    last = time.monotonic()
    req = urllib.request.Request("https://crates.io/api/v1/" + path, headers={"User-Agent": UA})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        return {"errors": f"HTTP {e.code} for {path}"}

def semver(num):
    return tuple(int(p) for p in num.split("+")[0].split("."))

def compatible(num):
    """Cargo's caret range: same major, same minor for 0.x, same patch for 0.0.x."""
    major, minor, patch = semver(num)[:3]
    return (major,) if major else (0, minor) if minor else (0, 0, patch)

def published(v):
    return datetime.datetime.fromisoformat(v["created_at"].replace("Z", "+00:00"))

def stable_versions(name):
    """Stable, unyanked versions, highest first. crates.io lists them by
    publish date, which puts a patch to an old line (rand_core 0.1.1 in
    2026) ahead of the real newest."""
    d = get(f"crates/{name}")
    if "versions" not in d:
        return None, d.get("errors", d)
    vs = [v for v in d["versions"] if not v["yanked"] and "-" not in v["num"]]
    return sorted(vs, key=lambda v: semver(v["num"]), reverse=True), None

def pick(versions, in_range=None):
    for v in versions:
        if published(v) <= cut and (in_range is None or compatible(v["num"]) == in_range):
            return v
    return None

status = 0

if not lock:
    for name in crates:
        versions, err = stable_versions(name)
        if versions is None:
            print(f"{name:24s} ERROR: {err}")
            status = 2
            continue
        ok = pick(versions)
        use = f"{ok['num']} ({ok['created_at'][:10]})" if ok else "none old enough"
        print(f"{name:24s} use {use:28s} newest {versions[0]['num'] if versions else '-'}")
    sys.exit(status)

def registry_packages(text):
    return {(p["name"], p["version"]) for p in tomllib.loads(text).get("package", [])
            if p.get("source", "").startswith(CRATES_IO)}

with open(lock, encoding="utf-8") as f:
    packages = registry_packages(f.read())
before = os.environ.get("PIN_VERSIONS_SINCE_LOCK")
if before is not None:
    packages -= registry_packages(before)

bad = 0
for name, num in sorted(packages):
    d = get(f"crates/{name}/{num}")
    if "version" not in d:
        print(f"ERROR    {name} {num}: {d.get('errors', d)}")
        status = 2
        continue
    v = d["version"]
    if not v["yanked"] and published(v) <= cut:
        continue
    bad += 1
    why = "YANKED" if v["yanked"] else "TOO NEW"
    versions, err = stable_versions(name)
    target = pick(versions or [], compatible(num))
    fix = (f"cargo update -p {name}@{num} --precise {target['num']}" if target
           else f"no version of {name} in {num}'s range is old enough yet")
    print(f"{why:8s} {name} {num} (published {v['created_at'][:10]})  ->  {fix}")

scope = f"new since {os.environ['PIN_VERSIONS_SINCE_REV']}" if before is not None else "in the lockfile"
print(f"{len(packages)} crates.io package(s) {scope} checked; {bad} break the {min_age}-day rule")
sys.exit(status or (1 if bad else 0))
PY

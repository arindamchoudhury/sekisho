#!/usr/bin/env bash
# Mechanical part of a Sekisho docs audit: decision presence, broken links,
# leftover old names, and Git hygiene. Run from anywhere inside the repo:
#   bash .claude/skills/auditing-sekisho-docs/audit.sh
# It cannot find contradictions; the SKILL.md reading pass does that.
set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel 2>/dev/null || (cd "$here/../../.." && pwd))"
cd "$root" || exit 2

# Claude Code keeps a project's memory in ~/.claude/projects/<slug>/memory,
# where <slug> is the repo's absolute path with each character that isn't a
# letter or digit replaced by "-". Override with AUDIT_MEMORY_DIR.
abs="$(pwd -W 2>/dev/null || pwd)"
mem="${AUDIT_MEMORY_DIR:-$HOME/.claude/projects/$(printf '%s' "$abs" | sed 's/[^A-Za-z0-9]/-/g')/memory}"

# Private files, kept out of Git by these .git/info/exclude rules. The last one
# is a safety net: the superpowers skills save there by default.
rules=(/.private/ /CLAUDE.local.md /docs/superpowers/)

declare -A F=(
  [README]=README.md
  [design]=.private/superpowers/specs/2026-11-08-sekisho-release-1-design.md
  [plan1]=.private/superpowers/plans/2026-11-08-plan-1-governed-gateway-core.md
  [plan1b]=.private/superpowers/plans/2026-11-10-plan-1b-bedrock-and-azure.md
  [plan1c]=.private/superpowers/plans/2026-11-11-plan-1c-spend-accuracy.md
  [plan2]=.private/superpowers/plans/2026-11-12-plan-2-sign-in-and-people.md
  [plans]=.private/superpowers/plans/README.md
  [comparison]=.private/comparison.md
  [inventory]=.private/research/feature-inventory.md
  [agw]=.private/research/building-on-agentgateway.md
  [spike]=.private/research/spike-agentgateway.md
  [claude]=.claude/CLAUDE.md
  [memory]="$mem/sekisho-project.md"
  [memdocs]="$mem/sekisho-internal-docs-not-in-git.md"
)

echo "== 1. Decision presence (review every MISSING by hand: search miss or real gap?)"
if [ ! -f "${F[design]}" ]; then
  echo "  skipped: the private notes aren't here (a fresh clone?)"
else
  absent=""
  for key in "${!F[@]}"; do [ -f "${F[$key]}" ] || absent="$absent $key"; done
  [ -n "$absent" ] && echo "  NOT FOUND, so not checked:$absent"
  missing=0
  while IFS=$'\t' read -r id name rx where; do
    [ "$id" = "id" ] && continue
    miss=""
    for key in $where; do
      f="${F[$key]:-}"
      if [ -z "$f" ]; then miss="$miss $key(unknown-key)"; continue; fi
      [ -f "$f" ] || continue
      grep -q -i -E "$rx" "$f" || miss="$miss $key"
    done
    if [ -n "$miss" ]; then echo "  MISSING $id $name ->$miss"; missing=$((missing+1)); fi
  done < "$here/decisions.tsv"
  total=$(($(wc -l < "$here/decisions.tsv") - 1))
  echo "  $((total - missing)) of $total decisions present everywhere expected"
fi

echo "== 2. Broken relative links (superseded notes may point at removed files)"
for f in README.md $(find .private .claude docs -name '*.md' 2>/dev/null); do
  grep -o -E '\]\([^)]+\)' "$f" | sed -E 's/^\]\(//; s/\)$//' | grep -v -E '^(https?|mailto):' |
  while read -r link; do
    p="${link%%#*}"; [ -z "$p" ] && continue
    [ -e "$(dirname "$f")/$p" ] || echo "  $f -> $link"
  done
done

echo "== 3. Leftover old names (Mooring, mk_), except the recorded rename"
grep -r -n -i -E --exclude-dir=auditing-sekisho-docs 'mooring|\bmk_' README.md .private .claude docs 2>/dev/null |
  grep -v -i -E 'renamed from mooring|not mooring|formerly mooring|old name|mooring\.git|arindamchoudhury/mooring|projects.mooring|mooring, which' || echo "  none"

echo "== 4. Git hygiene"
if git rev-parse --git-dir >/dev/null 2>&1; then
  exc="$(git rev-parse --git-dir)/info/exclude"
  for rule in "${rules[@]}"; do
    grep -qx -- "$rule" "$exc" || echo "  EXCLUDE RULE MISSING: $rule"
  done
  stray="$(find docs/superpowers -type f 2>/dev/null)"
  [ -n "$stray" ] && echo "  STRAY PRIVATE NOTE, move it into .private/superpowers/: $(echo $stray)"
  # Everything Git tracks, plus everything `git add -A` would pick up.
  visible="$( { git ls-files; git ls-files --others --exclude-standard; } | sort -u)"
  for rule in "${rules[@]}"; do
    p="${rule#/}"; p="${p%/}"
    hit="$(printf '%s\n' "$visible" | grep -E "^${p//./\\.}(/|$)")"
    [ -n "$hit" ] && echo "  PRIVATE NOTE VISIBLE TO GIT: $(echo $hit)"
  done
  # Public files must not carry home folders or the committer's email.
  pat='[A-Za-z]:[\\/]Users[\\/]|C-{2}Users-|/home/[A-Za-z0-9_.-]+/|/Users/[A-Za-z0-9_.-]+/'
  email="$(git config user.email 2>/dev/null)"
  [ -n "$email" ] && pat="$pat|$(printf '%s' "$email" | sed 's/[][\.*^$+?(){}|]/\\&/g')"
  printf '%s\n' "$visible" | while read -r f; do
    [ -f "$f" ] && grep -n -I -H -E "$pat" "$f" | sed 's/^/  PERSONAL DETAIL: /'
  done
  echo "  tracked: $(git ls-files | tr '\n' ' ')"
  echo "  git add -A would add: $(git ls-files --others --exclude-standard | tr '\n' ' ')"
  echo "  README links to private files: $(grep -c -E '\]\((\./)?(\.private/|CLAUDE\.local|docs/superpowers/)' README.md)"
else
  echo "  not a Git repository"
fi

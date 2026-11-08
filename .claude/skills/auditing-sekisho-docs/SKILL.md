---
name: auditing-sekisho-docs
description: Use when asked whether Sekisho's documents reflect all decisions, after any decision, rename or design change, or before handing a design or plan to the user for review
---

# Auditing Sekisho docs

## Overview

An audit has three passes:
1. **Presence**, scripted: is every decision in every document that should
   carry it?
2. **Contradictions**, by reading: does any document say something a decision
   rules out?
3. **Hygiene**, scripted: links, old names, Git.

The script cannot see contradictions. In the 2026-11-08 audit, all three real
problems were found by reading, not by the script.

## Steps

1. **New decisions first.** If the conversation produced decisions since the
   last audit, add a row to `decisions.tsv`: `id`, `decision`, a case-insensitive
   `regex`, and the files that must carry it. File keys are `README`, `design`,
   `plan1`, `plans`, `comparison`, `inventory`, `agw`, `spike`, `claude`,
   `memory` and `memdocs`. Add a key in `audit.sh` when a new plan exists.
2. **Run** `bash .claude/skills/auditing-sekisho-docs/audit.sh`.
3. **Judge every `MISSING` by hand**: is it a narrow regex or a real gap?
   Widen the regex, or fix the document.
4. **Read for contradictions.** Read the whole design, and the Global
   Constraints and scope notes of each plan. Check these trouble spots:
   - which provider types each plan builds, against the design's provider list;
   - the routes per environment;
   - the budget path: refusal in `extAuthz`, while the rate-limit service only
     records spend;
   - client credentials are stripped, not refused;
   - design section 14's three lists: owner-decided, our call, and decided
     while planning;
   - agentgateway: the dev pin against "ships on the fixed release";
   - every design requirement has an owning plan in the plans index.
5. **Fix it in all places at once:** the design section, design section 14,
   the plans index, `.claude/CLAUDE.md` and the project memory, as relevant.
6. **Re-run the script**, then report: what you checked, what you found and
   fixed, and what is knowingly left.

## Known, accepted output

- Broken links in `.private/environment-spec.md`, which point at the removed
  `examples/`; it is superseded.
- "Mooring" in the design's note recording the rename.

## Common mistakes

- Reporting "all good" from the script alone.
- Fixing a decision in its section but not in section 14, or the other way
  round.
- Linking a private note from `README.md`.
- Committing a private note: any path in `.git/info/exclude`.
- Putting personal or machine-specific details in `.claude/`, which is public.
  They belong in `CLAUDE.local.md`.

## Without the private notes

A fresh clone has no `.private/superpowers/`, `.private/research/` or project memory.
The script then says so and runs only the hygiene checks.

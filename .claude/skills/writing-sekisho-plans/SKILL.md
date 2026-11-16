---
name: writing-sekisho-plans
description: Use when writing or revising a Sekisho release-1 implementation plan (plan 1b or plans 2–8), or when choosing or bumping dependency versions for one
---

# Writing Sekisho plans

## Overview

Plans implement the approved release 1 design, one plan at a time, each
written just before it starts. A plan is only as good as the facts it pins:
check upstream schemas, APIs and versions live, never from memory.

**REQUIRED SUB-SKILL:** use `superpowers:writing-plans` for the format: header,
Global Constraints, Review Focus, test-first tasks and self-review.

## Before writing

1. Read:
   - the design sections that the plans index (`.private/superpowers/plans/README.md`)
     lists for this plan;
   - plan 1's Global Constraints and Interfaces blocks;
   - the related research in `.private/research/`;
   - the pages in `docs/` that this plan's features touch, so each task
     can say what it adds or changes there.
2. Check upstream facts live:
   - **agentgateway:** config shapes, protos and CEL fields at the pinned tag.
     **REQUIRED SUB-SKILL:** `working-with-agentgateway`.
   - **Dex:** the gRPC API and feature flags at the pinned version
     (`api_connectors_crud`, `sessions_enabled`, `api_sessions_identities_crud`).
   - **Crate versions:** run
     `bash .claude/skills/writing-sekisho-plans/pin-versions.sh <crate>...`.
     Use the "use" column, which is at least 14 days old.
   - **Indirect versions:** the 14-day rule covers every crate in
     `Cargo.lock`, and Cargo resolves indirect dependencies to their newest
     versions, which can be hours old. Any task that changes `Cargo.lock`
     runs `pin-versions.sh --lock --since HEAD` before its commit and applies
     the `cargo update --precise` pins it prints. Say so in Global
     Constraints.
   - **ACH and Obot:** for each feature the plan builds, read how both
     [ACH](https://github.com/ackstorm/ach) and
     [Obot](https://github.com/obot-platform/obot) do it, as `.claude/CLAUDE.md`
     rule 7 says (fetch first). Then design it on its merits: the standards
     that apply, security, simplicity. Say in the plan what each does, where
     Sekisho does better or differently, and why.
3. Reuse plan 1's names rather than inventing parallel ones:
   - crates `sekisho-*`, binary `sekisho`, `SEKISHO_*` variables, `sek_`
     tokens;
   - `Store`, `StoreError`, `ChangeKind`, `AuthCache`, `HooksState`,
     `RenderOptions`, `GatewaySnapshot`.

## The plan

- Save it to `.private/superpowers/plans/YYYY-MM-DD-plan-<n>-<slug>.md`. It is
  internal: never `git add` it.
- Copy into Global Constraints the design values that apply: fail closed,
  micro-USD, the pins, and "a Sekisho token never reaches a provider".
- **Docs in every task.** `docs/` documents what works today, and each
  change updates it in the same commit (`.claude/CLAUDE.md` rule 2). So:
  - a task that changes what people, admins, operators or clients can do
    or see (API routes and fields, gateway answers, settings, commands,
    limits) lists the `docs/` pages in its **Files**, has a step before
    its commit that says what to add or change on each, and puts them in
    its commit's paths;
  - the step writes from the code as built, not from the plan, and
    corrects anything the change made wrong elsewhere in `docs/`;
  - a task that changes nothing anyone sees says "Docs: none" and why;
  - a new page is linked from `docs/README.md`, and a new API route from
    `docs/api.md`'s routes table;
  - Global Constraints' bookkeeping list includes "update `docs/` in the
    same task".
- **An independent review when the plan is done.** Global Constraints say
  that a reviewer with a fresh context, on the most capable model, reviews
  the plan's whole commit range before it is pushed: the code, and also
  `docs/` and the README against the code as built. For the docs it checks
  that every claim the range touched is true (statuses, error codes, fields,
  limits, settings, defaults, examples); that everything the range changed
  that anyone can see is documented, and nothing is still described the old
  way; that the "not yet proven end to end" list is honest; and that links
  and anchors resolve. Wrong docs are findings like wrong code, graded by
  their effect: docs that would lead to an insecure setup, a broken install
  or a wrong spend figure are Important.
- End with an acceptance task that proves the design sections this plan owns,
  against a real agentgateway where traffic is involved. It also updates
  `docs/README.md`'s list of what isn't yet proven end to end, and the
  README's status paragraph.
- Give commit steps explicit paths. Never include a private note (any path in
  `.git/info/exclude`).

## After writing

1. Update this plan's row in the plans index (status, coverage) and the project
   memory.
2. Run the `auditing-sekisho-docs` skill.
3. Hand over: ask the user to review it and to choose an execution approach.
   Recommend Native when the tasks share interfaces.

## Common mistakes

- Writing agentgateway config from memory instead of validating it.
- Checking only one of ACH and Obot, or a checkout that wasn't fetched
  first.
- Taking ACH's or Obot's way because they do it, without asking whether it
  is the best one (the standards, the security, Sekisho's own constraints).
- Picking a crate version less than 14 days old.
- Checking only direct dependencies: plan 1's first lockfile took `syn` and
  `itoa` versions published that day.
- A design requirement left with no owning plan: compare every plan's scope
  note with the index.
- Changing a decision inside a plan without also updating the design and
  section 14.
- Leaving docs to the plan's last task, or to the README alone: plan 1d's
  Task 5 changed every refusal clients see and shipped with no docs.

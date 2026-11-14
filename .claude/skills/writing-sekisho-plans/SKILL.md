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
   - the related research in `.private/research/`.
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
   - **ACH and Obot:** for each feature the plan builds, check how both
     [ACH](https://github.com/ackstorm/ach) and
     [Obot](https://github.com/obot-platform/obot) do it, as `.claude/CLAUDE.md`
     rule 7 says: fetch first, then read both. Say in the plan what each does
     and where Sekisho differs, and why.
3. Reuse plan 1's names rather than inventing parallel ones:
   - crates `sekisho-*`, binary `sekisho`, `SEKISHO_*` variables, `sek_`
     tokens;
   - `Store`, `StoreError`, `ChangeKind`, `AuthCache`, `HooksState`,
     `RenderOptions`, `GatewaySnapshot`.

## The plan

- Save it to `.private/superpowers/plans/YYYY-MM-DD-plan-<n>-<slug>.md`. It is
  internal: never `git add` it.
- Copy into Global Constraints the design values that apply: fail closed,
  micro-USD, one provider per API format, the pins, and "a Sekisho token never
  reaches a provider".
- End with an acceptance task that proves the design sections this plan owns,
  against a real agentgateway where traffic is involved.
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
- Picking a crate version less than 14 days old.
- Checking only direct dependencies: plan 1's first lockfile took `syn` and
  `itoa` versions published that day.
- A design requirement left with no owning plan: compare every plan's scope
  note with the index.
- Changing a decision inside a plan without also updating the design and
  section 14.

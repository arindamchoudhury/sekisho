# Sekisho: project notes for Claude

Sekisho is an open-source control plane for coding agents (Claude Code, Codex,
Gemini CLI, OpenCode). It decides which models, MCP servers, skills and plugins
each team's agents get, keeps provider keys, caps spend in USD and records what
happened. It is built in Rust on an unmodified agentgateway, shaped like Obot,
and aims for all of ACH's features over time.

**Status:** release 1 design approved; plan 1 of the release written; no code
yet.

## Maintainers' private notes (not in Git)

The design and plans are working notes kept outside Git, so a fresh clone
doesn't have them. Without them, the README and the code are the source of
truth. Where a maintainer has them:

| What | Path |
|---|---|
| Release 1 design. Section 14 lists every decision: **(decided)** = the owner decided; **(our call)** = can still change | `.private/superpowers/specs/2026-11-08-sekisho-release-1-design.md` |
| Plans index (1, 1b, 2–8) | `.private/superpowers/plans/README.md` |
| Plan 1, governed gateway core | `.private/superpowers/plans/2026-11-08-plan-1-governed-gateway-core.md` |
| Research: feature inventory, feasibility study, market research, building on agentgateway, spike results | `.private/research/` |
| Comparison with ACH, Blue, Obot | `.private/comparison.md` |
| Superseded first idea (history only, don't build from it) | `.private/architecture.md`, `.private/environment-spec.md` |

## Rules

1. **Private notes are never committed** and never linked from the README.
   They all live in `.private/`. Each maintainer excludes them in
   `.git/info/exclude` with `/.private/`, `/CLAUDE.local.md` and, as a safety
   net, `/docs/superpowers/`. The superpowers skills save specs and plans to
   `docs/superpowers/` by default: save them to `.private/superpowers/`
   instead, and ignore those skills' "commit the doc" steps.
   If `.git` is ever re-created, restore these lines before the first
   `git add`. Before committing, check `git status --short --ignored`.
2. **Tracked files:** `README.md`; `LICENSE` (the official Apache-2.0 text,
   SHA-256 `cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30`);
   `.claude/` (this file and the project skills); later the code.
3. **`.claude/` is public**, shared with everyone who clones the repo. Keep
   secrets, personal details and notes about one person's machine out of it.
   Put those in `CLAUDE.local.md` at the repo root, which Claude Code loads
   after this file.
4. **Ask before acting on Git or GitHub:** commit, push, force-push, add a
   remote, or create, rename or delete a GitHub repo only after the user's
   explicit yes.
5. **Record a new decision** in the relevant design section and in section 14,
   in the same change. Then run the `auditing-sekisho-docs` skill.
6. Mark something **(decided)** only when the user decided it. Choices Claude
   makes are **(our call)**.

## Names and conventions

| Thing | Convention |
|---|---|
| Product name | Sekisho (関所). Not Mooring, which was the old name |
| Crates | `sekisho-*` |
| Binary | `sekisho` |
| Environment variables | `SEKISHO_*` |
| HTTP headers | `x-sekisho-*` |
| Environment key tokens | `sek_<id>_<secret>`. Never `sk_`, which OpenAI and Stripe use |
| Money | Integer micro-USD; prices per 1,000,000 tokens |
| Line endings | LF, even where Git has `core.autocrlf=true`; `.gitattributes` sets `* text=auto eol=lf` |

## Pins

| What | Pin |
|---|---|
| Rust | 1.99.1, edition 2024; crate versions from plan 1's Tech Stack |
| New dependency versions | Must be at least 14 days old (`writing-sekisho-plans` has a script) |
| agentgateway | `cr.agentgateway.dev/agentgateway:v1.7.0` until a release contains PRs #4302 and #4304 (the DNS fix for #2810) |
| PostgreSQL | 18 |
| Dex | v2.46.1, the sign-in layer behind Sekisho's own OAuth 2.1 server |
| Web UI | React + TypeScript; everything else Rust |

## Tips

On Windows with Git Bash and Docker Desktop:

- Use `127.0.0.1`, not `localhost`, for published ports: curl tries `::1`
  first and hangs.
- Set `MSYS_NO_PATHCONV=1` when passing `/var/run/docker.sock` or container
  paths to `docker`. Use `pwd -W` for Windows paths in bind mounts.
- File-change events from Windows bind mounts don't reach Linux containers.
  Put watched files in a Docker volume and copy them in.
- Docker Desktop's memory is limited; don't run two big stacks at once.

Anywhere:

- GitHub's search API allows 30 requests a minute; space searches out.

## Project skills

- `auditing-sekisho-docs`: check that every document reflects every decision.
- `writing-sekisho-plans`: write plan 1b or plans 2–8.
- `working-with-agentgateway`: config, hooks, validation and version bumps.

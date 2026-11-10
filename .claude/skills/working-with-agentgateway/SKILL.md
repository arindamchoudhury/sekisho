---
name: working-with-agentgateway
description: Use when writing, validating or debugging agentgateway configuration for Sekisho, implementing Sekisho's agentgateway hooks (extAuthz, ext_proc, remote rate limit, ext_mcp, credential provider, token exchange), or checking whether to bump the pinned agentgateway version
---

# Working with agentgateway

## Overview

Sekisho uses agentgateway unmodified: Sekisho writes its config and answers its
hooks. Facts below were proven against **v1.7.0** in a spike (write-up in the
maintainers' private notes, `.private/research/spike-agentgateway.md`) and in
plan 1's end-to-end tests (`crates/sekisho-e2e`); the table stands on its own.
Anything else must be checked against the source at the pinned tag: a shallow
clone (`git clone --depth 1 --branch v1.7.0`) is enough.

## Pinned version and bumping

- The pin is `cr.agentgateway.dev/agentgateway:v1.7.0` (git `26f5aeb`).
- Bump to the first release containing PRs #4302 and #4304 (the DNS cache fix
  for #2810), and set its DNS `maxTTL` low. Check with
  `gh release list -R agentgateway/agentgateway` and
  `gh api repos/agentgateway/agentgateway/compare/v1.7.0...<tag>`.
- Before a bump, read the release notes and the config schema diff for
  anything Sekisho uses that is removed, deprecated or moved to a commercial
  edition. agentgateway has a closed enterprise edition, so a feature can
  leave the open-source one.
- After a bump, re-validate every config and re-run plan 1's e2e tests.

## Validate a config

Run `bash .claude/skills/working-with-agentgateway/validate.sh <config.yaml> [tag]`.
It prints `VALID` or the schema errors. Once per new config shape, break one
field on purpose to prove the check bites.

## Proven behaviour (v1.7.0)

| Area | Behaviour |
|---|---|
| Config file | Reloads about 25 ms after a write. A reload never ends running streams, even when it removes the stream's own route |
| `extAuthz` (HTTP) | Receives the original path. Its denial bodies reach the client. Runs per request only, so it does not stop a running stream. `protocol.http.addRequestHeaders` adds headers to the hook request from the config, overwriting any client copy; `protocol.http.metadata` exposes the hook's answer to later policies as `extauthz.*` |
| `ext_proc` (`fullDuplexStreamed`) | The processor must echo each chunk as a `streamed_response`. An `ImmediateResponse` mid-stream ends the stream: the client keeps its 200, gets no `[DONE]`, and the upstream connection closes |
| `remoteRateLimit` | With `type: tokens` and `cost: int(llm.cost.total * 1000000.0)` it is called twice per request: before, with cost 0, and after, with the exact cost in micro-USD. Its 429 has an empty body, so Sekisho refuses in `extAuthz`. Descriptor entries are evaluated after `backendAuth`, so `Authorization` holds the provider key there: use `extauthz.*` |
| Native USD budgets | Only on API keys (`apiKey.keys[].budgets`), not for signed-in users |
| Route `ai` backend | One provider or a load-balanced group, so it can't choose a provider by model. The top-level `llm:` block exists only once per config |
| `ai.routes` | Without it, every request is handled as an OpenAI chat completion: an Anthropic `/v1/messages` call is translated and answered in OpenAI format. It maps path suffixes to route types (`/v1/messages: messages`, `:generateContent: generateContent`, …); unmatched paths are still chat completions |
| `hostOverride` | For built-in providers (OpenAI, Anthropic, Gemini) without `pathPrefix`, the client's own path goes upstream, so a client can reach any provider endpoint with the provider key. Set `pathPrefix` (`/v1`, or `/` for Gemini). Custom providers always choose their own path. With `hostOverride`, built-in providers skip their default TLS |
| Methods and paths | The client's method goes upstream unchanged (a GET reaches the provider), so match `method: POST`. Routes match the raw path: no percent-decoding or dot-segment resolution (read in the source; encoded probes were refused end to end) |
| `SESSION_KEY` | Must be set, or agentgateway tries to write a generated key into its config file |
| Docker DNS | Keeps a restarted upstream's old IP (503) until agentgateway restarts (#2810) |
| `backendAuth.jwtSign` | Fixed claims only. Per-user tokens need token exchange to Sekisho |
| Overhead | Hooks add about 1.3 ms per request |

## Where to look upstream (at the pinned tag)

- Schemas: `schema/config.json`, `schema/cel.json` (CEL fields such as
  `llm.cost.total`, `jwt.*`, `mcp.tool.name`).
- Protos, all self-contained: `crates/protos/proto/{rls,ext_proc,shared_envoy,ext_mcp,credprovider,resource,xds}.proto`.
- Examples: `examples/` (`llm-keyed-rate-limit`, `mcp-authorization`,
  `traffic-token-exchange`, `traffic-jwt-sign`).
- LLM request handling: `crates/agentgateway/src/llm/mod.rs`
  (`set_default_path`, `default_connector_policies`, per-provider auth
  headers) and `crates/agentgateway/src/proxy/httpproxy.rs`
  (`resolve_llm_route_type`, the order of route, backend and LLM policies).
  Route matching: `crates/agentgateway/src/http/route.rs`.
- xDS: `config.xdsAddress` and `config.xdsAuthToken`. Not used in release 1.

## Common mistakes

- `localhost` on Windows: use `127.0.0.1`.
- Bind-mounting the config from a Windows folder: reloads never fire. Use a
  Docker volume.
- A plain `BodyResponse` in `fullDuplexStreamed` mode: the chunks are dropped.
- A Python `http.server` mock without `disable_nagle_algorithm = True`: you get
  a fake 40 ms latency.
- Assuming a config shape from memory: validate it.
- An `ai` backend without `ai.routes`: Anthropic and Gemini requests are
  parsed as OpenAI ones.
- A built-in provider with `hostOverride` and no `pathPrefix`: the client
  chooses the upstream path.
- Checking only status codes in an e2e test: record what reached the mock
  provider (method, path, headers) and assert on that.

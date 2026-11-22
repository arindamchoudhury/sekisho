# The gateway

Coding tools call models through agentgateway, at each environment's
routes. Sekisho writes agentgateway's configuration and checks every
request; agentgateway sends it on to the provider with the provider's own
credential, translating between APIs where needed.

## Calling a model

Send the request to the gateway (agentgateway's listener, port 3000) under
the environment's name, with a session token or an environment key as a
bearer token, and the model's name as Sekisho knows it:

```bash
curl -s https://gateway.example.com/env/data-team/openai/v1/chat/completions \
  -H "Authorization: Bearer $SEKISHO_KEY" -H 'content-type: application/json' \
  -d '{"model": "gpt-5", "messages": [{"role": "user", "content": "Hello"}]}'

curl -s https://gateway.example.com/env/data-team/anthropic/v1/messages \
  -H "Authorization: Bearer $SEKISHO_KEY" -H 'content-type: application/json' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{"model": "claude-sonnet-4-5", "max_tokens": 256, "messages": [{"role": "user", "content": "Hello"}]}'
```

The credential must be in `Authorization: Bearer`. agentgateway removes
`Authorization`, `x-api-key`, `api-key` and `x-goog-api-key` before the
provider, so a Sekisho token never reaches a provider, and a provider key a
tool sends is never used. It also removes Sekisho's own `x-sekisho-env` and
`x-sekisho-model` headers, so a client can't choose an environment or a
model with them, and they never reach a provider.

[`sekisho use`](cli.md#installing-an-environment) writes these settings
for Claude Code, Gemini CLI and Qwen Code. Claude Code asks `sekisho
token` for its key and sends it as a bearer token. Gemini CLI and Qwen
Code send theirs as `x-goog-api-key` or to any URL they're given, so
[`sekisho run`](cli.md#starting-a-tool-with-sekisho-run) starts them
behind a proxy on the developer's computer, which sends the token as
`Authorization: Bearer`. A tool set up by hand must send its token as
`Authorization: Bearer`, which not every tool does by default.

## Model requests

A model route takes a POST to one of these endpoints, with no query but
`?beta=true` or `?alt=sse`:

- `/env/<name>/anthropic/v1/messages`, and `…/messages/count_tokens`;
- `/env/<name>/openai/v1/chat/completions` and `/env/<name>/openai/v1/responses`;
- `/env/<name>/gemini/v1beta/models/<model>:generateContent`, also
  `streamGenerateContent` and `countTokens`, and under `v1` or `v1alpha`.

Any other method, path or query is refused before it reaches a provider,
so a key can't reach a provider's other endpoints (files, fine-tuning,
stored completions) with the provider's credential.

An environment lists models from any of its providers, and a request
reaches only those.

## Which API reaches which model

Each client API reaches some providers' models, passed through or
translated by agentgateway:

| Client API | Reaches |
|---|---|
| Anthropic Messages, OpenAI Chat Completions | every model |
| OpenAI Responses (Codex) | every model except Claude on the Anthropic provider and Claude on Azure Foundry, which agentgateway can't translate to |
| Gemini | Gemini models only |

So a Codex user who needs Claude needs it on Bedrock, or uses another
tool. One exception on Bedrock: a Claude model Bedrock serves only through
its newer Mantle endpoint (in agentgateway v1.7.0's catalog,
`anthropic.claude-mythos-5`) takes only the Messages API there, so Codex
gets agentgateway's own error for it rather than Sekisho's 400. An environment can offer the same kind of model from several
providers.

## When the gateway says no

Sekisho answers its refusals as JSON, with an `error` code and a `message`
for people. It checks in this order, so nobody learns which models an
environment has before their credential and access pass. One refusal
comes after Sekisho's checks and is agentgateway's own: a query other
than `?beta=true` or `?alt=sse` gets agentgateway's 403, not Sekisho's
JSON.

| Status | `error` | When |
|---|---|---|
| 401 | `unauthorized` | No `Authorization: Bearer` credential, or a session token that isn't valid (`reason`: `invalid_token`, `token_expired`, `session_ended`) |
| 403 | `forbidden` | The key or person may not use this environment (`reason`: `unknown_key`, `wrong_environment`, `revoked`, `expired`, `suspended`, `invalid` when the key's owner lost access, `no_access` for a session) |
| 404 | `not_found` | Not a POST to a model endpoint |
| 404 | `model_not_found` | The environment lists no model by that name, or the request names none. The answer is the same whether or not Sekisho has the model in another environment |
| 400 | `api_not_served` | The client's API can't reach the model; the message names the APIs that can |
| 429 | `budget_exhausted` | A budget the request counts against is used up; `budget` and `resets_at` say which and until when |
| 503 | `unavailable` | Sekisho couldn't check the request; try again |

For example:

```json
{"error": "api_not_served",
 "message": "claude-sonnet-4-5 can't be reached through the Responses API; use the Anthropic Messages API or the Chat Completions API"}
```

Once a request passes Sekisho's checks, any error comes from agentgateway
or the provider, as agentgateway relays it.

If Sekisho's hooks can't be reached, agentgateway refuses requests, unless
an operator chose otherwise ([`SEKISHO_HOOKS_FAIL_OPEN`](running.md#settings)).

## While a request runs

Sekisho checks a request when it starts, and keeps watching its stream: a
key that is revoked, suspended or expires, a key whose owner loses access,
or a session that ends cuts the stream about a second later. The tool keeps
its `200` and what it already got, and gets no more. An access token that
merely expires mid-stream doesn't cut it, and neither does a budget used
up: budgets refuse the next request. A stream still running when Sekisho
stops gets a grace to finish, and is cut the same way after it
([Running Sekisho](running.md#commands)).

Sekisho's hooks add about 1.3 ms to each request, and about 2.5–3 ms
together with agentgateway's prompt counting, which spend needs (medians
measured in Docker Desktop).

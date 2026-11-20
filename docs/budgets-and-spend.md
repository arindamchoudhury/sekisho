# Budgets and spend

Sekisho records what every model request costs, in US dollars, and refuses
new requests once a budget they count against is used up. Only admins
manage budgets; anyone can read [their own spend](#your-spend).

## Budgets

```bash
curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  https://sekisho.example.com/api/v1/budgets \
  -d '{"name": "data team", "scope": "environment", "environment": "data-team",
       "limit": "500", "window": "month"}'
```

| Field | Meaning |
|---|---|
| `name` | Up to 100 characters, shown in refusals |
| `scope` | What the budget counts, with the field it needs: `person` with `person` (a user ID) and optionally `environment` to count only that environment; `environment` with `environment`; `key` with `key` (an environment key's ID) |
| `limit` | Dollars, such as `"500"` or `"12.50"` |
| `window` | `day`, `week` or `month` |
| `calendar` | `true` (the default): UTC days, ISO weeks from Monday, calendar months. `false`: rolling windows of 1, 7 or 30 days, counted from when the budget was made |

A request counts against every budget that applies to it: its person's
(the key's owner, or whoever signed in), its environment's, and its key's.
`GET /api/v1/budgets` lists every budget and `DELETE /api/v1/budgets/{id}`
deletes one. Deleting an environment deletes its budgets and its keys'.

## How much is left

`GET /api/v1/budgets/{id}/status` answers the budget with `spent` in its
current window, `resets_at`, and `exhausted`.

When a budget is used up, the gateway refuses new requests that count
against it with 429 `budget_exhausted`, naming the budget and when it
resets ([refusals](gateway.md#when-the-gateway-says-no)). If several are
used up, it names the one that resets last. Requests already running are
not cut off, so spend can go a little over a limit.

## Your spend

`GET /api/v1/me/spend` answers anyone signed in the budgets that count
them: their own (in any environment), those of the environments they may
use, and those of their keys that aren't revoked. Called with an
environment key, it answers the key's own budgets and its environment's.

```json
[{"scope": "environment", "name": "data team", "environment": "data-team",
  "limit_usd": "500", "spent_usd": "123.45", "resets_at": "2026-12-01T00:00:00Z"}]
```

Each is `{scope, name, limit_usd, spent_usd, resets_at}` for the current
window, with `environment` for an environment's budget or a person's
within one, and `key` (its ID) for a key's.

## How a request is charged

Each request is charged once, when it ends, to every budget it counts
against:

1. When the provider reports how many tokens it used and the model has
   [its own prices](providers-and-models.md#models), Sekisho prices the
   provider's counts: input, cache reads, cache writes and output. The
   prices are the model's as the environment lists it, even when the
   provider reports a dated name such as `claude-sonnet-4-5-20250929`.
2. When the provider reports and the model has no prices of its own, the
   cost agentgateway's model catalog gives.
3. When a stream ends before the provider reports (the tool hung up, a
   revoked key cut it, or Sekisho stopped), an estimate, at the model's prices or else at
   agentgateway's catalog prices:
   - input: the provider's count if it sent one, else agentgateway's own
     count of the prompt;
   - output: the answer text the tool received, counted the way
     agentgateway counts (OpenAI's `o200k_base` tokens, times 1.3 for
     Claude models, or 1.6 from Claude 4.7).

   Sekisho waits 5 seconds after a stream ends for agentgateway's report
   before it estimates, so a late report still wins. A request that seems
   to hang is estimated only after an hour without a change, so a long
   answer is never cut short.
4. When the provider answered with an error and reported nothing, nothing;
   nor when the request failed before any answer reached the tool.

Counting tokens (Anthropic's `count_tokens`, Gemini's `countTokens`) is
never charged, whether the provider counts or agentgateway does for a
provider that can't. A request nothing can price (no prices of its own,
and none in the catalog) is charged nothing and logged as a warning. A request made with a
session token counts against its person's budgets, like one made with
their key.

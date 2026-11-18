# The API

Sekisho's API is served under `/api/v1` on its admin listener
(`SEKISHO_LISTEN`, port 8080 by default). There is no web UI or CLI for it
yet, so these pages show it with `curl`.

## Who may call it

Every call needs a session token, `Authorization: Bearer <token>`, which
you get by [signing in](people-and-sign-in.md#signing-in). Environment keys
(`sek_…`) work only on the gateway, never on the API.

- `/api/v1/me…` is for anyone signed in.
- Everything else is for admins: people marked as an admin, and members of
  the groups in `SEKISHO_ADMIN_GROUPS`. Anyone else gets 403.

The person is read from the database on every call, so disabling someone
or removing their admin mark applies to their next call.

```bash
TOKEN=...   # an access token from /oauth/token
curl -s -H "Authorization: Bearer $TOKEN" https://sekisho.example.com/api/v1/me
```

## Conventions

- Request and answer bodies are JSON, with `snake_case` fields. A request
  with a field the route doesn't know is refused (400; 422 for an
  environment, listed with its other problems), so a misspelt field never
  goes unnoticed. Environments may also be sent and read as YAML.
- Money is a decimal string of US dollars with at most six decimal places,
  such as `"3"` or `"0.075"`: no sign, exponent, separator or currency
  symbol.
- Model prices are dollars per 1,000,000 tokens.
- Times are RFC 3339 in UTC, such as `"2026-12-01T00:00:00Z"`.
- IDs are UUIDs, except environment keys' (12 characters).
- Secrets you send (provider credentials, connector settings, catalog
  sources' tokens, registry passwords) are never answered back, and never
  written to the log.

## Errors

Every error is JSON with an `error` code and a `message` for people:

```json
{"error": "validation", "message": "some fields are invalid",
 "fields": [{"field": "base_url", "message": "an OpenAI-compatible provider needs a base URL"}]}
```

| Status | `error` | When |
|---|---|---|
| 400 | `bad_request` | The body isn't JSON, misses a required field, or has one the route doesn't know |
| 401 | `unauthorized` | No session token, or one that isn't valid; the answer has `WWW-Authenticate: Bearer realm="sekisho"` |
| 403 | `forbidden` | Not an admin, on an admin route |
| 404 | `not_found` | No such thing, or a name that can't be one |
| 409 | `conflict` | It already exists, or it can't change that way (the message says why) |
| 409 | `in_use` | It can't be deleted while something uses it; the message names what |
| 422 | `validation` | Some fields are invalid; `fields` lists each with a message, all at once |
| 503 | `unavailable` | Something Sekisho needs, such as Dex, can't be reached; try again |
| 500 | `internal` | A fault in Sekisho; the details are in the server's log |

## Routes

| Method and path | What it does | Page |
|---|---|---|
| `GET /api/v1/me` | Who you are: your ID, email, name, groups and whether you're an admin | [People](people-and-sign-in.md#you) |
| `GET /api/v1/me/environments` | The environments you may use | [Environments](environments-and-keys.md#the-environments-you-may-use) |
| `GET`, `POST /api/v1/me/keys` | Your environment keys; make one | [Keys](environments-and-keys.md#environment-keys) |
| `POST /api/v1/me/keys/{id}/{suspend,resume,revoke}` | Change one of your keys | [Keys](environments-and-keys.md#environment-keys) |
| `GET`, `POST /api/v1/providers` | List or add model providers | [Providers](providers-and-models.md#providers) |
| `PATCH`, `DELETE /api/v1/providers/{name}` | Change or delete a provider | [Providers](providers-and-models.md#changing-a-provider) |
| `GET /api/v1/provider-presets` | The known OpenAI-compatible hosts | [Presets](providers-and-models.md#host-presets) |
| `GET`, `POST /api/v1/models` | List or add models | [Models](providers-and-models.md#models) |
| `DELETE /api/v1/models/{name}` | Delete a model | [Models](providers-and-models.md#models) |
| `GET /api/v1/environments` | List environments | [Environments](environments-and-keys.md#environments) |
| `GET`, `PUT`, `DELETE /api/v1/environments/{name}` | Read, create or replace, or delete one; as JSON, or as YAML with `Content-Type` or `Accept: application/yaml` | [Environments](environments-and-keys.md#environments), [YAML](environments-and-keys.md#yaml) |
| `PATCH /api/v1/environments/{name}` | Change its owner: `{"owner"}` only | [Owners](environments-and-keys.md#owners) |
| `POST /api/v1/environments/{name}/proposals` | Propose an environment's new version, or its deletion, as a GitHub pull request (Git mode) | [Git mode](environments-and-keys.md#git-mode) |
| `GET`, `PUT`, `DELETE /api/v1/git-mode` | Read, turn on or change, or turn off Git mode | [Git mode](environments-and-keys.md#git-mode) |
| `GET /api/v1/environments/{name}/keys` | Every key for an environment | [Keys](environments-and-keys.md#keys-for-admins) |
| `POST /api/v1/keys/{id}/{suspend,resume,revoke}` | Change anyone's key | [Keys](environments-and-keys.md#keys-for-admins) |
| `GET`, `POST /api/v1/budgets` | List or add budgets | [Budgets](budgets-and-spend.md) |
| `DELETE /api/v1/budgets/{id}` | Delete a budget | [Budgets](budgets-and-spend.md) |
| `GET /api/v1/budgets/{id}/status` | Spend in the current window | [Budgets](budgets-and-spend.md#how-much-is-left) |
| `GET /api/v1/users`, `GET`, `PATCH /api/v1/users/{id}` | People; mark admins, disable or enable | [People](people-and-sign-in.md#people) |
| `POST /api/v1/users/{id}/offboard` | Offboard someone in one step | [Offboarding](people-and-sign-in.md#offboarding) |
| `GET`, `POST /api/v1/groups`, `DELETE /api/v1/groups/{name}` | Sekisho's groups | [Groups](people-and-sign-in.md#groups) |
| `PUT`, `DELETE /api/v1/groups/{name}/members/{user}` | Add or remove a member | [Groups](people-and-sign-in.md#groups) |
| `POST /api/v1/local-accounts`, `DELETE /api/v1/local-accounts/{id}` | Make or delete a local account | [Local accounts](people-and-sign-in.md#local-accounts) |
| `POST /api/v1/local-accounts/{id}/reset` | Reset a local account's password | [Local accounts](people-and-sign-in.md#local-accounts) |
| `GET /api/v1/connectors`, `PUT`, `DELETE /api/v1/connectors/{id}` | Identity providers' connectors | [Identity providers](people-and-sign-in.md#identity-providers) |
| `POST /api/v1/signing-keys/rotate` | Rotate the key that signs session tokens | [Tokens](people-and-sign-in.md#tokens) |
| `GET`, `POST /api/v1/catalog-sources` | List or add catalog sources; adding starts a sync | [Sources](catalogs.md#sources) |
| `GET`, `PATCH`, `DELETE /api/v1/catalog-sources/{name}` | Read, change or delete a source (409 `in_use` while an environment, or another source's toolkit, lists its entries) | [Sources](catalogs.md#sources) |
| `POST /api/v1/catalog-sources/{name}/sync` | Sync a source now (202) | [Syncs](catalogs.md#syncs) |
| `GET /api/v1/catalog-sources/{name}/syncs/latest` | What the latest sync found and refused | [Syncs](catalogs.md#syncs) |
| `POST /api/v1/catalog-sources/{name}/apply` | Apply the latest sync | [Apply](catalogs.md#apply) |
| `POST /api/v1/catalog-sources/import-obot` | Add Obot catalog repositories as sources | [Importing Obot catalogs](catalogs.md#importing-obot-catalogs) |
| `GET /api/v1/catalog-sources/{name}/vmcps` | Obot virtual servers offered as environments | [Virtual servers](catalogs.md#virtual-servers) |
| `POST /api/v1/catalog-sources/{name}/vmcps/{key}/environment` | Create an environment from one (never replacing one) | [Virtual servers](catalogs.md#virtual-servers) |
| `GET /api/v1/catalog/{kind}` | The catalog entries of a kind (`mcp-servers`, `a2a-agents`, `skills`, `plugins`, `prompts`, `toolkits`), each with the `toolkits` that list it | [Entries](catalogs.md#entries) |
| `GET`, `PATCH /api/v1/catalog/{kind}/{source}/{name}` | An entry with its revisions (a toolkit's with their `members`); change its owner | [Entries](catalogs.md#entries), [Toolkits](catalogs.md#toolkits), [Owners](catalogs.md#owners) |
| `GET /api/v1/catalog/{kind}/{source}/{name}/files/{path}` | A file of an entry's applied revision | [Entries](catalogs.md#entries) |
| `POST`, `DELETE /api/v1/catalog/{kind}/{source}/{name}/quarantine` | Quarantine an entry, or lift that | [Quarantine](catalogs.md#quarantine) |
| `GET /api/v1/registry-credentials`, `PUT`, `DELETE /api/v1/registry-credentials/{host}` | Container registries' credentials | [Registry credentials](catalogs.md#registry-credentials) |

Gateway agents use one more route, `GET /internal/gateway/config`, with
the gateway token ([Running Sekisho](running.md#the-gateway-agent)).

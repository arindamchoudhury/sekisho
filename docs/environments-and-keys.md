# Environments and keys

An **environment** is what a team's coding tools use: a list of models,
the groups whose members may use it, and its own routes on the gateway
(`/env/<name>/…`, see [The gateway](gateway.md)). An **environment key**
lets a program use one environment without signing in, such as a CI job.

## Environments

Only admins manage environments. `PUT /api/v1/environments/{name}` creates
the environment or replaces it whole:

```bash
curl -s -X PUT -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  https://sekisho.example.com/api/v1/environments/data-team \
  -d '{
    "description": "The data team",
    "access_groups": ["data", "github/acme:data-eng"],
    "models": ["claude-sonnet-4-5", "gpt-5", "mistral-small-3.2"],
    "default_models": {"openai": "gpt-5"},
    "small_model": "mistral-small-3.2",
    "labels": {"cost-centre": "4711", "team": "data"},
    "mcp_servers": ["github", {"server": "docs@platform", "tools": ["search", "read"]}],
    "a2a_agents": ["cost-analyst"],
    "skills": ["pdf"],
    "plugins": [],
    "prompts": ["monthly-review"],
    "toolkits": ["finops"]
  }'
```

| Field | Meaning |
|---|---|
| `api_version` | Optional in JSON; `sekisho/v1alpha1` if given |
| `kind` | Optional in JSON; `Environment` if given |
| `name` | Optional in JSON; must match the name in the path. 1–63 lowercase letters, digits or hyphens, starting and ending with a letter or digit, but not `models`, which the gateway would misread in a Gemini path. It is also the route prefix |
| `description`, `notice` | Text for people. The notice is meant to be shown to whoever uses the environment |
| `access_groups` | Who may use it: Sekisho groups by name, or identity providers' groups as `<connector>/<group>`, such as `github/acme:platform`. No groups means no one may use it |
| `models` | The [models](providers-and-models.md#models) its routes serve, in order, from any providers. Each must exist, and none twice |
| `default_models` | The model each coding tool starts on, per API format: `anthropic`, `openai`, `gemini` ([below](#default-models)) |
| `small_model` | A model for tools that call one in the background, as Claude Code does; one of `models` |
| `labels` | Key–value pairs that spend exports carry ([below](#labels)) |
| `mcp_servers` | [Catalog](catalogs.md) MCP servers: a name, or `{"server", "tools"}` to allow only those tools |
| `a2a_agents`, `skills`, `plugins`, `prompts` | Catalog entries of each kind |
| `toolkits` | [Toolkits](catalogs.md#toolkits), whose entries the environment delivers too |
| `owner` | JSON only: the owner's user ID. Without it, the owner stays the same, or the admin who creates the environment owns it. `PATCH` changes it on its own ([below](#owners)) |

In [Git mode](#git-mode), environments change through pull requests instead.

Every list keeps its order. A field the environment doesn't know is
refused, and so is every problem at once (422, `fields` naming each, such
as `skills[1]` or `default_models.openai`).

The answer adds `id`, `owner_email`, `created_at`, `updated_at`,
`effective_default_models`, `catalog_status` and `delivered`. A fetched
environment can be sent back as it is: those fields are ignored.
`GET /api/v1/environments` lists every environment, and
`GET /api/v1/environments/{name}` reads one.

A change usually reaches the gateway within a second, and within a few
seconds after a burst of changes ([Running Sekisho](running.md#the-gateway-agent)),
without ending requests already running.

`DELETE /api/v1/environments/{name}` deletes the environment with its
keys, its budgets and its keys' budgets.

### Catalog entries

Each catalog list names entries as `name`, or as `name@source` when more
than one source has an entry of that kind and name; a bare name that
several sources hold is refused with the forms to use, such as
`pdf is in sources a, b: write pdf@a or pdf@b`. A name that matches
nothing is refused too (`no skill named nope`), and so is one entry named
twice.

**A saved reference stays bound** to the entry it named. Saved again the
same way, it keeps that entry even if another source has since added an
entry of the same name; written differently, it is looked up again.

An entry is added only when its source's sync has been
[applied](catalogs.md#apply) and it is neither
[quarantined](catalogs.md#quarantine) nor removed from its source. One the
environment lists already stays when it is quarantined or removed later,
and `catalog_status` says so: one row per reference, with `kind`,
`reference` as saved, the `source` and `name` it is bound to, and
`quarantined` and `removed`. Toolkits are listed there too.

A source can't be deleted while an environment lists one of its entries
(409 `in_use`, naming the environment).

### What an environment delivers

`delivered` is what the environment gives its coding tools: its own
entries, then those only its toolkits bring, **each entry once**. A
quarantined toolkit brings nothing; what the environment lists itself, or
gets from another toolkit, stays. Toolkits themselves aren't delivered.

| Field | Meaning |
|---|---|
| `kind`, `source`, `name` | The entry |
| `tools` | An MCP server's allowed tools: the union of what the environment and each toolkit allow, sorted; `null` when any of them allows every tool |
| `listed` | The environment lists it itself |
| `via` | The toolkits that bring it, as `<source>/<name>` |
| `quarantined`, `removed` | As in `catalog_status`; delivery leaves a quarantined entry out ([Quarantine](catalogs.md#quarantine)) |

Nothing reaches coding tools yet: `sekisho use` and the gateway's MCP and
A2A routes are still to come.

### Default models

Each coding tool starts on its own default model, which the environment
may not serve. An environment therefore has a default for each API
format, chosen from its models:

- a default named in `default_models` must be one of `models`, and every
  API of its format must reach it: for `openai`, both Chat Completions and
  the Responses API, so not Claude on an Anthropic provider (Codex sends
  Responses, which can't reach it); for `gemini`, a Gemini provider's
  model ([The gateway](gateway.md));
- a format with no default named uses the first of `models` that it
  reaches, and has none when no listed model is reached.

`effective_default_models` answers each format's default as named or as it
falls back, and `small`: the `small_model` when one is named, else `null`
(tools then use their default model). A model named as a default can't be
deleted, since it is one of the environment's models.

### Labels

Labels are free-form, such as `"cost-centre": "4711"`. At most 32; a key
is 1–63 lowercase letters, digits, `.`, `_` or `-`, starting and ending
with a letter or digit, and keys starting with `sekisho` are reserved; a
value is 1–256 characters with no control characters and no space at
either end. In YAML a number or `true` is read as its text.

### YAML

The same environment can be written as YAML, the format Git mode and
`sekisho check` will read. `PUT` with `Content-Type: application/yaml`
takes it, and `GET` with `Accept: application/yaml` answers it:

```yaml
api_version: sekisho/v1alpha1
kind: Environment
name: data-team
description: The data team
access_groups:
- data
models:
- claude-sonnet-4-5
- gpt-5
default_models:
  openai: gpt-5
labels:
  cost-centre: '4711'
mcp_servers:
- github
- server: docs@platform
  tools:
  - search
toolkits:
- finops
```

In YAML, `api_version` and `kind` are required, and the fields Sekisho
sets (`owner`, `id`, timestamps, `effective_default_models`,
`catalog_status`, `delivered`) are refused. `GET` leaves out empty lists
and unset defaults, and what it writes reads back to the same environment.

### Git mode

In Git mode, environments are kept as YAML files in one repository, and
Sekisho applies each new commit. Changes go through pull requests on
GitHub (github.com or GitHub Enterprise Server), so they are reviewed
where your code is. The API stops editing environments while it is on.

Turn it on, or change its settings, with `PUT /api/v1/git-mode`:

```bash
curl -s -X PUT -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  https://sekisho.example.com/api/v1/git-mode \
  -d '{"url": "https://github.com/acme/sekisho-environments.git",
       "folder": "environments", "token": "...", "github_token": "..."}'
```

| Field | Required | What it is |
|---|---|---|
| `url` | yes | The repository's `https://` URL, as for a [catalog source](catalogs.md#sources); its last two path segments are the GitHub repository (`owner/name`) |
| `ref` | no | The branch to follow; without one, the default branch. Pull requests are opened against it |
| `folder` | no | The folder holding the environment files; without one, the root |
| `username`, `token` | no | What reads the repository, as for a catalog source. A `PUT` without `token` clears it |
| `allow_private_network` | no | `true` lets the Git host and GitHub's API be on a private or local address |
| `github_api_url` | for other hosts | GitHub's REST API: `https://api.github.com` for `github.com` (the default), or GitHub Enterprise Server's, such as `https://ghe.example.com/api/v3` |
| `github_token` | yes | Opens pull requests. A fine-grained token for that repository alone, with read and write access to **Contents** and **Pull requests**. Sealed in the database and never answered back |

Every `PUT` replaces all the settings. Git mode reads the repository
through a catalog source named `environments` (a name no other source
can take), synced every minute; the admin who turns Git mode on owns the
environments it creates. `GET /api/v1/git-mode` answers `{"enabled":
false}` when it is off, and otherwise its settings (`source`, as a
catalog source answers, without tokens), `github_api_url`,
`has_github_token`, `enabled_by`, `enabled_at` and `latest_sync`.

**The files** are every `.yaml` or `.yml` file in the folder (and the
folders under it, hidden ones skipped) whose `kind` is `Environment`, in
the [YAML format](#yaml): one environment each, with `api_version` and
`kind`, and without an owner. Other YAML is left alone, and two files
naming one environment are both refused.

**Each sync** reads the newest commit and saves every environment it
holds, each file on its own, as a `PUT` would; no apply is needed. Its
report, in `latest_sync.environments`, has three lists:

- `applied`: the environments saved;
- `refused`: the files that couldn't be, as `{"path", "name", "fields"}`,
  each field with its message, such as a model that doesn't exist. **The
  environment stays as it was**;
- `not_in_git`: environments whose files are gone. **A sync never deletes
  an environment**: it stays, and can now be deleted with `DELETE`.

**While Git mode is on**, `PUT /api/v1/environments/{name}`, creating one
from an [Obot virtual server](catalogs.md#virtual-servers), and `DELETE`
of an environment whose file is in Git answer 409, pointing to
proposals. `PATCH` still changes an owner, since owners aren't in Git.

**Proposals.** `POST /api/v1/environments/{name}/proposals` opens a pull
request:

- `{"environment": {...}}` with the environment as JSON, or
  `{"environment": "<YAML>"}` with its YAML, proposes it as the
  environment's new file. It is checked as a save would be first (422 with
  every problem, and nothing reaches GitHub); nothing is saved;
- `{"delete": true}` proposes deleting the environment's file.

The file is the one the environment was read from, or
`<folder>/<name>.yaml` for a new one. Sekisho makes a branch
`sekisho/<name>-<8 hex>` off the followed branch, commits the change
there, and opens a pull request whose description names the admin who
proposed it. The answer (201) is `{"number", "url"}`. When the pull
request is merged, the next sync applies it. GitHub's refusals, such as
a token without the permissions above, answer 503 with GitHub's message.

`DELETE /api/v1/git-mode` turns Git mode off: its source and syncs go,
the environments stay as ordinary ones, and the API edits them again.

### Owners

`PATCH /api/v1/environments/{name}` with `{"owner": "<user ID>"}` hands the
environment to another owner, an enabled user, and changes nothing else;
it takes no other field. [Offboarding](people-and-sign-in.md#offboarding)
hands a person's environments over too.

## The environments you may use

`GET /api/v1/me/environments` lists the environments anyone signed in may
use (one of their groups is among the access groups), with each one's
name, description, notice and models. Being an admin gives no access of its
own: admins are listed the environments their groups give them.

## Environment keys

A key belongs to one person and one environment, and works only on that
environment's gateway routes. It works as long as its owner may use the
environment: someone who leaves the environment's groups, is disabled or
is offboarded can't use their keys any more.

People make their own keys:

```bash
curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  https://sekisho.example.com/api/v1/me/keys \
  -d '{"environment": "data-team", "name": "ci", "expires_at": "2027-03-01T00:00:00Z"}'
```

- `name` is up to 100 characters, without control characters, and unique
  in the environment.
- `expires_at` is optional, and must be in the future.
- For an environment you can't use, or one that doesn't exist, the answer
  is the same 422, so it tells nobody which environments exist.

The answer is the key with its `token`, `sek_<id>_<secret>`. **The token is
shown only this once**: Sekisho keeps only a hash of the secret.

Every key answer has a `state`:

| `state` | Meaning |
|---|---|
| `active` | It works |
| `invalid` | Its owner may no longer use the environment; it works again if they regain access |
| `suspended` | Paused; `suspended_by_admin` says whether an admin paused it |
| `expired` | Past `expires_at` |
| `revoked` | Ended for good |

`GET /api/v1/me/keys` lists your keys, and
`POST /api/v1/me/keys/{id}/suspend`, `…/resume` or `…/revoke` changes one
(204). A key an admin suspended can only be resumed by an admin (409), and
a revoked key stays revoked (409).

A key that is revoked, suspended or expires, or whose owner loses access,
is refused within about a second, and a stream already running with it is
cut about a second later: the tool keeps what it got and gets no more.

### Keys for admins

`GET /api/v1/environments/{name}/keys` lists every key for an environment,
with each owner's ID and email. `POST /api/v1/keys/{id}/suspend`,
`…/resume` or `…/revoke` changes anyone's key. Admins don't make keys for
other people: each person makes their own.

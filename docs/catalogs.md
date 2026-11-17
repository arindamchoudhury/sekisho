# Catalogs

A catalog source is a Git repository that holds MCP servers, A2A agents,
skills, plugins, prompts or [toolkits](#toolkits) of them. Sekisho reads each source at a commit, checks
every entry it finds, and keeps what it found in PostgreSQL: the entries,
their revisions and every file, so nothing is fetched from Git again.
Admins see what each sync found, apply updates, own and quarantine
entries.

What works today is the catalog itself: sources, syncs, entries, owners
and quarantine. **Environments can't list catalog entries yet, and
nothing in a catalog reaches a coding tool or the gateway yet**: MCP
servers and A2A agents can't be called through Sekisho.

## Sources

`POST /api/v1/catalog-sources` adds a source:

```bash
curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  https://sekisho.example.com/api/v1/catalog-sources \
  -d '{"name": "team-skills", "kind": "skills",
       "url": "https://github.com/example/skills.git",
       "ref": "main", "folder": "skills"}'
```

| Field | Required | What it is |
|---|---|---|
| `name` | yes | 1–63 lowercase letters, digits or hyphens, starting and ending with a letter or digit. `environments` is reserved |
| `kind` | yes | What the repository holds: `catalog`, `skills`, `plugins` or `prompts` ([below](#what-each-kind-reads)) |
| `url` | yes | The repository's `https://` URL, at most 2,048 characters, with no user name, password, query or fragment, not starting with `-` |
| `ref` | no | The branch or tag to follow (or a full `refs/…` name): 1–255 letters, digits or `. _ / -`, not starting with `-` or `/`, without `..`, `//` or `@{`. Without one, the repository's default branch |
| `folder` | no | The folder of the repository to read, such as `skills/team`: the same characters, without `.` or `..` parts. Without one, the root |
| `username` | no | Sent with `token`; without one, `x-access-token` (what GitHub expects). At most 255 characters, no spaces, control characters or `:` |
| `token` | no | A token that can read the repository, at most 4,096 characters, no spaces or control characters. Sealed in the database, sent only to that repository, and never answered back: answers say `has_token` |
| `allow_private_network` | no | `true` lets the repository's host, and the registries and agents its entries name, be on a private or local address. Default `false` |

The answer (201) is the source. Its first sync starts at once and
**applies itself**: environments use what it found. Every invalid field is
listed at once (422); a name that is taken is 409.

| Route | Does |
|---|---|
| `GET /api/v1/catalog-sources` | Every source, by name |
| `GET /api/v1/catalog-sources/{name}` | One source |
| `PATCH /api/v1/catalog-sources/{name}` | Change `url`, `ref`, `folder`, `username`, `token` or `allow_private_network`; `null` clears `ref`, `username` or `token`. The name and kind never change |
| `DELETE /api/v1/catalog-sources/{name}` | Delete it, with its syncs and entries (204); 409 `in_use` while a [toolkit](#toolkits) of another source lists one of its entries |

A source answers its settings and:

| Field | What it is |
|---|---|
| `has_token` | Whether a token is stored; the token never is |
| `added_by` | Who added it; its entries' owner until changed ([Owners](#owners)) |
| `applied_commit` | The commit environments use |
| `latest_sync` | The latest sync: `id`, `started_at`, `finished_at`, `commit`, `outcome` (`ok`, `unchanged` or `failed`), `error`, `applied_at` |
| `pending` | The latest successful sync waits for an [apply](#apply) |
| `syncing` | A sync of it is running |
| `updated_at` | When its settings last changed |

Changing the URL of a source that has a token needs the token again (or
`"token": null`), so nobody can send a stored token somewhere new. A new
URL, ref or folder makes the next sync due at once.

## Syncs

Every source is synced again `SEKISHO_CATALOG_SYNC_INTERVAL` after its
last sync (an hour by default; [Running](running.md#settings)). Sekisho
looks for due sources every 30 seconds and syncs one at a time; with
several replicas, each source is synced by one of them.
`POST /api/v1/catalog-sources/{name}/sync` syncs one now: 202 with the
source, or 409 `conflict` while a sync of it is running.

A sync asks Git for the ref's commit, checks it out, reads it, pins what
its entries name ([Exact versions](#exact-versions-and-image-digests),
[Agent cards](#a2a-agents-and-their-cards)) and records what it found.
When the ref hasn't moved, a `skills`, `prompts` or `environments` source
records `unchanged` without reading anything; `catalog` and `plugins`
sources are read again, since their images and cards live outside Git.

`GET /api/v1/catalog-sources/{name}/syncs/latest` is the latest sync's
report (404 before the first):

| Field | What it is |
|---|---|
| `id`, `started_at`, `finished_at`, `commit` | Which sync, and the commit it read |
| `outcome` | `ok`, `unchanged` or `failed` |
| `error` | Why it failed. A failed sync changes no entry |
| `applied` | Environments use what it found |
| `new`, `updated`, `unchanged` | The entries it found, each `{"kind", "name"}` |
| `missing` | The source's entries it didn't find; an apply marks them removed |
| `refused` | What it didn't take, and why: `{"path", "kind", "name", "reason"}`, `kind` and `name` when known |
| `warnings` | What it took with a remark: `{"path", "message"}`, such as an ignored field or a hidden character that doesn't refuse |
| `vmcps` | Obot virtual MCP servers it found ([Importing Obot catalogs](#importing-obot-catalogs)) |

Kinds in reports and entries are `mcp_server`, `a2a_agent`, `skill`,
`plugin`, `prompt` and `toolkit`.

**Anything Sekisho can't read with certainty is refused, never guessed.**
A refused entry keeps the revision environments use, and so does an entry
whose file or folder can't be read at all: a broken file never removes
entries. Two entries of one kind with the same name in one source are
both refused.

### Apply

Only a source's first sync applies itself. Later ones wait:
`POST /api/v1/catalog-sources/{name}/apply` applies the latest successful
sync, and answers what changed:

```json
{"updated": [{"kind": "skill", "name": "pdf"}], "removed": []}
```

`updated` entries now use the revision that sync found; `removed` ones
weren't found and are marked removed. It is 409 when there is nothing to
apply.

### How Sekisho reads Git

Sekisho runs the system `git` (2.37 or later; [Running](running.md)),
hardened:

- `https://` only, no redirects, no submodules, no hooks, no symlinks, and
  no system, global or user Git configuration; the server's proxy and
  `GIT_*` settings never reach it;
- the host's addresses are checked first and Git is pinned to them, so DNS
  can't move it. Private, local and metadata addresses are refused unless
  `allow_private_network` is set;
- the token goes in an `Authorization: Basic` header (`username:token`),
  never in the URL or on the command line, and is removed from every error
  and report Sekisho stores;
- a checkout over 100 MiB, or a sync over 5 minutes, fails.

## What each kind reads

Whatever the kind, an entry's files are read only from inside its folder:
a symlink anywhere in an entry, or a path that leaves the repository,
refuses it. `catalog`, `plugins` and `prompts` sources skip hidden folders
(starting with `.`); a `skills` source searches every folder but `.git`,
so skills under `.claude/skills` are found.

| Limit | Value |
|---|---|
| One file | 10 MiB |
| One entry's files | 50 MiB and 5,000 files |
| A `SKILL.md` | 1 MiB |
| A plugin's manifest or a marketplace file | 5 MiB |
| Catalog files in one `catalog` source | 1,000 (more fails the sync) |
| An A2A agent's card | 1 MiB |

### `catalog`: MCP servers, A2A agents and toolkits

A `catalog` source holds YAML or JSON files with one item or a list of
items each. Which files are read is chosen as Obot chooses them: the
patterns in `.obotcatalogs` (one a line, `#` comments; without it
`*.json`, `*.yaml` and `*.yml`), less those in `.ignoreobotcatalogs`. A
pattern without `/` matches a file name at any depth; one with `/` matches
a path from the folder's root.

**Obot's format** is read as Obot reads it. Each item is an MCP server,
named by its `entryKey` or `name` made lowercase, other characters turned
into `-`, at most 64 characters. Its runtime is one of:

| Runtime | What Sekisho takes |
|---|---|
| `npx` | `npxConfig.package` at one exact version, and `args` |
| `uvx` | `uvxConfig.package` at one exact version, `command` and `args` |
| `containerized` | `containerizedConfig.image`, `command`, `args`, `port` and `path`; the image is pinned to a digest at sync |
| `remote` | `remoteConfig.fixedURL`: `https://`, a query allowed. `urlTemplate`, `tunnelName` and `remoteConfig.headers` (move them to `config`) are refused |
| `openapi`, `composite` | Refused: Sekisho can't deliver them |

Configuration fields (`config`) keep their name, description, key, whether they are required, and a prefix;
each needs a `usage` of `env` or `header`. A **sensitive value** written
in the catalog is refused, since delivered files hold no secrets.
`userAllowed` is refused; `options` and `secretBinding` are ignored with a
warning. Fields only Obot's hosting uses are ignored with a warning:
`resources`, `minObotVersion`, `maxObotVersion`, `injectObotHeaders`, and
a runtime's `egressDomains`, `denyAllEgress`, `startupTimeoutSeconds` and
`healthzPath`. Other unknown fields are ignored, as Obot ignores them.

**Sekisho's format** (`apiVersion: sekisho/v1alpha1`) refuses unknown
fields. Like Obot's, a file holds one item or a list of them (not several
YAML documents). An MCP server gives exactly one of `remote`, `npx`, `uvx`
or `container`:

```yaml
- apiVersion: sekisho/v1alpha1
  kind: MCPServer
  name: github
  description: GitHub's MCP server.
  container:
    image: ghcr.io/github/github-mcp-server:v0.20.1
    args: [stdio]
  config:
    - name: GitHub token
      key: GITHUB_PERSONAL_ACCESS_TOKEN
      usage: env
      sensitive: true
      required: true
- apiVersion: sekisho/v1alpha1
  kind: MCPServer
  name: fetch
  npx:
    package: "@example/fetch-mcp@1.4.2"
    args: ["--quiet"]
- apiVersion: sekisho/v1alpha1
  kind: MCPServer
  name: docs
  remote:
    url: https://mcp.example.com/mcp
```

`uvx` takes `package` (such as `mcp-server-time==2026.1.0`), `command` and
`args`; `container` takes `image`, `command`, `args`, `port` and `path`.

An A2A agent names its `url`, and `card_url` when its card isn't at the
usual place:

```yaml
apiVersion: sekisho/v1alpha1
kind: A2AAgent
name: billing
description: Answers questions about invoices.
url: https://agents.example.com/billing
```

A [toolkit](#toolkits) is `kind: Toolkit` in the same format.

### `skills`

A skill is a folder with a `SKILL.md` that is a regular file, read by
Obot's rules (the [Agent Skills](https://agentskills.io) format): its YAML
frontmatter's `name` equals the folder's name and follows the entry name
rules; `description` has 1–1,024 characters; `compatibility`, if any, at
most 500. The skill is its folder's files. A skill's folder isn't searched
for more skills, and a `SKILL.md` at the source's root is refused: a skill
needs a folder of its own.

### `plugins`

A Claude Code marketplace: `.claude-plugin/marketplace.json` lists the
plugins, each from a folder of the same repository. A plugin from another
repository (`github`, `url`, `git-subdir` sources) is refused: add that
repository as a source of its own. Without a marketplace, the root or each
top-level folder with a manifest (`.claude-plugin/plugin.json`, or
`plugin.json` as APM writes it) is a plugin.

A plugin's MCP servers (its manifest's `mcpServers`, or `.mcp.json`) must
start the way catalog servers do: `npx` and `uvx` with one exact package,
`docker run` with one image and no option that changes what runs or how;
otherwise the plugin is refused.

**APM bundles** are checked as `apm install` checks them: every file that
`apm.lock.yaml`'s `pack.bundle_files` lists is present with its SHA-256,
and no other file is, but `apm.lock.yaml` and `plugin.json`. A bundle that
fails is refused, with every problem.

### `prompts`

Every Markdown file but `README.md` (in any case) is a prompt, named by
its file name without `.md`, which must follow the entry name rules.

### Names

An entry's name is 1–64 lowercase letters and digits, with single hyphens
between them. Two sources may hold entries of the same name: their
addresses differ by source.

### Exact versions and image digests

An npm package names one exact version (`name@1.2.3`, `@scope/name@1.2.3`),
a Python package one (`name==1.0`, or `name@1.0` as `uvx` writes it),
never a range, tag, wildcard, URL, Git or path. A container image may name
a tag; each sync resolves it to a digest at its registry
(`image:tag@sha256:…`), and the digest is what the entry holds. An image
without a tag is `latest`. A registry that wants a password gets the one
[set for its host](#registry-credentials).

### A2A agents and their cards

Each sync fetches an agent's card from its `card_url`, or else
`<url>/.well-known/agent-card.json` (A2A v1.0), then
`<url>/.well-known/agent.json` (v0.2) when that is missing. The card must
be a JSON object of at most 1 MiB with a `name` and either v1.0's
`supportedInterfaces` or v0.3's `url`, without hidden characters. The
checked card is kept with the entry's revision, so a changed card shows
as an update.

### Hidden characters

Every text file of an entry, and every catalog file, is scanned for
hidden characters, sorted as APM's content scanner sorts them:

- **refused**: bidirectional embeddings, overrides and isolates
  (U+202A–U+202E, U+2066–U+2069), tag characters (U+E0001–U+E007F) and
  supplementary variation selectors (U+E0100–U+E01EF);
- **listed as warnings**, with the file, line and column: zero-width
  characters and direction marks (U+200B–U+200F), word joiners and
  invisible operators (U+2060–U+2064), deprecated format characters
  (U+206A–U+206F), the soft hyphen, the Arabic letter mark, variation
  selectors (U+FE00–U+FE0E), interlinear annotations (U+FFF9–U+FFFB), and
  a byte order mark anywhere but the start.

## Entries

| Route | Does |
|---|---|
| `GET /api/v1/catalog/{kind}` | The entries of a kind: `mcp-servers`, `a2a-agents`, `skills`, `plugins`, `prompts` or `toolkits` |
| `GET /api/v1/catalog/{kind}/{source}/{name}` | One entry, with its revisions and their files |
| `PATCH /api/v1/catalog/{kind}/{source}/{name}` | Change its `owner` |
| `GET /api/v1/catalog/{kind}/{source}/{name}/files/{path}` | A file of its applied revision |
| `POST`, `DELETE /api/v1/catalog/{kind}/{source}/{name}/quarantine` | Quarantine it, or lift that |

An entry answers:

| Field | What it is |
|---|---|
| `kind`, `source`, `name` | Its address |
| `path` | Where in its source the latest sync found it |
| `owner` | Its owner's user ID |
| `applied` | The revision environments use: `id`, `commit`, `digest`, `applied_at`. `null` until its source's sync is applied |
| `pending` | A newer revision waiting for an apply, or `null` |
| `removed_at` | When an apply found it gone |
| `quarantine` | `{"reason", "by", "at"}` while quarantined, else `null` |
| `toolkits` | The toolkits whose applied revision lists it, as `"<source>/<name>"` |

One entry's answer also gives each revision's `spec` (what Sekisho read:
its `kind`, description, runtime and so on) and `files` (`path`, `size`,
`sha256`, `executable`), and its `quarantine_history`: every period in
quarantine, with who quarantined and lifted it, and when. A toolkit's
revisions also give their `members` ([Toolkits](#toolkits)).

A file is answered as `application/octet-stream`, never to be shown in a
browser. A revision's digest covers its spec and every file, so two syncs
that found the same thing share one revision.

### Owners

An entry's owner is, at first, the admin who added its source.
`PATCH` with `{"owner": "<user ID>"}` hands it to someone else, who must
exist and not be disabled (else 422). [Offboarding](people-and-sign-in.md#offboarding)
hands a person's entries to their new owner. An entry a later sync finds
is owned by whoever added the source.

### Quarantine

```bash
curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  https://sekisho.example.com/api/v1/catalog/mcp-servers/team/github/quarantine \
  -d '{"reason": "CVE-2026-1234: leaks tokens"}'
```

`reason` has 1–500 characters, newlines allowed. Quarantining an entry
that is quarantined already, or lifting one that isn't, is 409. Every
period is kept in the entry's `quarantine_history`. Today quarantine is
recorded and shown; what it stops comes with the parts that deliver
entries: environments will keep listing a quarantined entry, and the
gateway will refuse its MCP server or A2A agent everywhere at once.

## Toolkits

A toolkit is a reusable set of catalog entries, such as everything a
FinOps team uses, that environments will list as one item. It is itself
an entry of kind `toolkit`, written in Sekisho's format in a `catalog`
source:

```yaml
apiVersion: sekisho/v1alpha1
kind: Toolkit
name: finops
description: Cloud cost questions and reports.
mcp_servers:
  - aws-cost-explorer
  - server: github
    tools: [search_issues, get_issue]
a2a_agents: [cost-analyst]
skills: [finops-report]
plugins: [focus-export@platform-plugins]
prompts: [monthly-review]
```

It lists `mcp_servers`, `a2a_agents`, `skills`, `plugins` and `prompts`,
at least one entry in all. An MCP server is its name, or
`{server, tools}` to allow only those tools; without `tools` it allows
them all, and `tools: []` is refused. A toolkit holds catalog entries
only: `models` (models, default models, access and budgets belong to
environments) and `toolkits` (toolkits don't hold toolkits) are refused,
as is any other field, an entry listed twice, and a tool's name that is
empty or listed twice.

**References** are an entry's name, or `name@source` when more than one
source has an entry of that kind and name. Each is bound to an entry when
a sync records a new version of the toolkit, from any source, this sync's
entries included. **A version keeps its bindings**: a source added later
with an entry of the same name changes nothing until the toolkit itself
changes. A version is refused when a reference names no entry, names a
name several sources hold, or names an entry another reference names
too; the report says which, such as
`skills[0]: pdf is in sources a, b: write pdf@a or pdf@b`. As with any
refused entry, environments keep the version they use.

A changed toolkit is a new version that waits for an [apply](#apply) of
its source, like any entry. One toolkit's answer gives, for its `applied`
and `pending` revisions, `members`:

```json
[{"kind": "mcp_server", "reference": "github", "source": "platform",
  "name": "github", "tools": ["search_issues", "get_issue"],
  "quarantined": false, "removed": false}]
```

Every entry's answer lists the `toolkits` whose applied version lists it.
A toolkit can be [quarantined](#quarantine) like any entry. A source can't
be deleted while a toolkit of another source lists one of its entries in
its applied or pending version (409 `in_use`, naming the toolkit, such as
`toolkits/platform/finops`); a source's own toolkits go with it.

## Registry credentials

Some registries want a password to resolve an image's tag.

| Route | Does |
|---|---|
| `GET /api/v1/registry-credentials` | Every host with a credential, and its username: `[{"host", "username"}]` |
| `PUT /api/v1/registry-credentials/{host}` | Set `{"username", "password"}` for `host` (`host[:port]`, such as `ghcr.io` or `registry.example.com:5000`), replacing any |
| `DELETE /api/v1/registry-credentials/{host}` | Remove it (204) |

Hosts are kept lowercase. Passwords are sealed in the database, sent only
to their registry, and never answered back. Docker Hub is
`registry-1.docker.io`.

## Importing Obot catalogs

`POST /api/v1/catalog-sources/import-obot` adds Obot catalog repositories
as `catalog` sources, each synced at once:

```bash
curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  https://sekisho.example.com/api/v1/catalog-sources/import-obot \
  -d '{"urls": ["https://github.com/obot-platform/mcp-catalog"]}'
```

It takes 1–50 `urls`, and an optional `username` and `token` for all of
them. The URLs are the forms Obot accepts:

| URL | Repository | Ref |
|---|---|---|
| `https://github.com/<org>/<repo>` (also `gitlab.com`, `bitbucket.org`) | `https://github.com/<org>/<repo>.git` | The default branch |
| `https://github.com/<org>/<repo>/<branch>` | The same | `<branch>` |
| `https://<host>/<path>/<repo>.git` | As given | The default branch |
| `https://<host>/<path>/<repo>.git/<branch>` | Up to `.git` | `<branch>` |

`https://` may be left out. Other hosts need the `.git` form. Without a
branch, Sekisho follows the repository's default branch, where Obot
assumes `main`. Every URL is checked before any source is added; a bad one
is 422 with its position, such as `urls[1]`. The answer (201) is
`{"sources": [...]}`. Each is named `obot-<repo>` (lowercase, other
characters as `-`), then `obot-<repo>-2`, `-3` and so on when the name is
taken.

Obot's virtual MCP servers (`type: vmcp` items) are listed in each sync's
report under `vmcps`, with what Sekisho can't carry over.

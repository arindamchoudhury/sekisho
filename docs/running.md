# Running Sekisho

There is no packaged install yet: the Docker Compose file for trying
Sekisho and the Helm chart come later in release 1. Today Sekisho runs from
its `Dockerfile`, and `e2e/docker-compose.yml`, which the end-to-end tests
use, is a complete working example. It is built for the tests (with a test
route and a mock model), so copy its shape, not its secrets.

## What it needs

| Part | Version | Notes |
|---|---|---|
| PostgreSQL | 18 | Sekisho's only state. Run `sekisho migrate` before `serve`, and after each upgrade |
| [Dex](https://github.com/dexidp/dex) | v2.46.1 | Run it with `DEX_API_CONNECTORS_CRUD=true`, or Sekisho can't manage identity providers. Its gRPC API must be on, and `enablePasswordDB: true` for local accounts. Use PostgreSQL storage: with Dex's in-memory storage, a local account's refresh hangs. Sekisho registers its own client with Dex (`sekisho`), so Dex's config needs none |
| [agentgateway](https://github.com/agentgateway/agentgateway) | v1.7.0 | Unmodified. Run it with `-f <config file>` and `SESSION_KEY` set, beside a gateway agent that writes the file |
| `git` | 2.37 or later | On the server, which runs it to read catalog sources; the image has it. 2.37 brought `http.curloptResolve`, which Sekisho uses to pin each Git host to the addresses it checked: an older `git` ignores it |

Sekisho reaches some places itself, so the server needs outbound HTTPS to
them: the Git hosts of catalog sources, the container registries of their
images (to resolve each image tag to its digest), and A2A agents (to fetch
and pin their cards). Before each connection it resolves the host and checks
every address: a cloud metadata, link-local or unspecified address is never
reached, and a private or loopback address only for a source with
`allow_private_network`. It then connects to the checked addresses only, and
follows no redirects. It uses no HTTP proxy, and verifies certificates
against the system's certificate store (the image's `ca-certificates`).

## Commands

The `sekisho` binary (the image's entry point) has four commands:

| Command | What it does |
|---|---|
| `sekisho serve` | Runs the API, sign-in, agentgateway's hooks, and publishes agentgateway's configuration. It can run as several replicas on one database |
| `sekisho migrate` | Applies database migrations (needs `SEKISHO_DATABASE_URL`) |
| `sekisho gateway-agent --server <url> --config-path <file>` | Runs beside agentgateway, fetches its configuration from Sekisho and writes the file agentgateway reads (needs `SEKISHO_GATEWAY_TOKEN`) |
| `sekisho admin-link --email <email>` | Prints a one-time link that makes a local admin, for an operator who has lost every admin (needs `SEKISHO_DATABASE_URL` and `SEKISHO_PUBLIC_URL`) |

## Settings

`sekisho serve` reads `SEKISHO_*` environment variables. An empty value
counts as unset, and a bad value stops it at start, naming the variable
but never quoting a secret.

| Variable | Required | Meaning |
|---|---|---|
| `SEKISHO_DATABASE_URL` | yes | PostgreSQL, such as `postgres://sekisho:...@db/sekisho` |
| `SEKISHO_DATABASE_MAX_CONNECTIONS` | no | The connection pool's size, 1 to 1000; default 20 |
| `SEKISHO_ENCRYPTION_KEY` | yes | Standard base64 of 32 random bytes (`openssl rand -base64 32`). It encrypts provider credentials and signing keys in the database: keep it safe, and keep it the same |
| `SEKISHO_PUBLIC_URL` | yes | Where people and programs reach Sekisho, such as `https://sekisho.example.com` |
| `SEKISHO_BOOTSTRAP_TOKEN` | yes | At least 16 characters; makes the first admin on `/setup`, and nothing once there is one |
| `SEKISHO_GATEWAY_TOKEN` | yes | At least 16 characters, different from the bootstrap token; gateway agents use it to fetch agentgateway's configuration |
| `SEKISHO_DEX_ISSUER` | yes | Dex's issuer, as browsers reach it, such as `https://sekisho.example.com/dex` |
| `SEKISHO_DEX_URL` | no | Where Sekisho reaches Dex over HTTP, if not at the issuer's origin |
| `SEKISHO_DEX_GRPC` | yes | Dex's gRPC API, `host:port` |
| `SEKISHO_ADMIN_GROUPS` | no | Comma-separated groups whose members are admins: Sekisho groups, or identity providers' groups as `<connector>/<group>` |
| `SEKISHO_HOOKS_HTTP_TARGET` | yes | Where agentgateway reaches Sekisho's HTTP hook, `host:port`, such as `sekisho:9080` |
| `SEKISHO_HOOKS_GRPC_TARGET` | yes | Where agentgateway reaches Sekisho's gRPC hooks, such as `sekisho:9081` |
| `SEKISHO_HOOKS_FAIL_OPEN` | no | Comma-separated hooks agentgateway lets requests past when Sekisho can't be reached: `authz`, `ratelimit`, `extproc`. Unset, every hook fails closed, which is safest: an open `authz` lets anyone through while Sekisho is down |
| `SEKISHO_LISTEN` | no | The API and sign-in listener; default `0.0.0.0:8080` |
| `SEKISHO_HOOKS_HTTP_LISTEN` | no | The HTTP hook (agentgateway's `extAuthz`); default `0.0.0.0:9080` |
| `SEKISHO_HOOKS_GRPC_LISTEN` | no | The gRPC hooks (spend recording and the stream guard); default `0.0.0.0:9081` |
| `SEKISHO_CATALOG_SYNC_INTERVAL` | no | How long after a catalog source's sync the next is due: a whole number and a unit, `s`, `m`, `h` or `d`, such as `30m`; from `1m` to `7d`; default `1h`. Sekisho looks for due sources every 30 seconds, and syncs one at a time |

`SEKISHO_E2E_PLAIN_HTTP=1` lets catalog sources, registries and agents be
reached over plain HTTP. It is for the end-to-end tests' mock alone: only a
build with the `e2e` feature accepts it, and any other build refuses to
start with it set.

Expose only the API listener to people. The hook listeners are for
agentgateway alone.

Sekisho talks to Dex in plain HTTP and gRPC for now, so keep both on a
private network: Dex's gRPC API manages passwords and connectors, and must
never be published. Browsers reach Dex only at `SEKISHO_DEX_ISSUER`.

At start, `serve` registers itself with Dex in the background. Until that
succeeds, signing in answers 503, and it keeps trying.

## Logs

Sekisho logs to standard output at `info` level; there is no setting for
the level yet. Logs name people by user ID and never contain secrets:
provider credentials, tokens, passwords, setup links and connector
settings are left out. A request Sekisho estimated is logged at `info`
with who paid (person and key), the model, the token counts and the
amount, and a request nothing could price at `warn`.

## The gateway agent

Run one gateway agent beside each agentgateway, sharing a volume for the
config file:

```bash
sekisho gateway-agent --server http://sekisho:8080/ --config-path /config/config.yaml
```

It waits on `GET /internal/gateway/config` for each new version, which
Sekisho publishes as soon as an admin's change is saved, and writes the
file; agentgateway reloads it without ending running requests. A change is
usually live at the gateway within a second.

Two seconds after its last write, with no newer version, the agent writes
the same config once more. agentgateway v1.7.0 can miss a file that is
replaced while it is still loading the previous one, and would then serve
the older config until the next change; the second write makes it end on
the latest ([agentgateway/agentgateway#4427](https://github.com/agentgateway/agentgateway/issues/4427)).
So after a burst of changes, such as a script editing many
environments, the last one can take a few seconds longer to go live.

Measured with the end-to-end stack in Docker Desktop: up to 3,000
environments of five models each, with no added latency per request; a
change was live within 1.4 s, or within 3.6 s after a burst of 300 changes,
against a design target of 5 s. agentgateway's reload takes longer as the
config grows: about 25 ms for a few environments, about 0.6 s at 1,500. On Windows
with Docker Desktop, share the file through a Docker volume, not a folder
from Windows: changes to a Windows folder don't reach agentgateway, so it
never reloads.

The agent speaks plain HTTP to Sekisho only, so run it on the same private
network as Sekisho; `--server` refuses an `https://` URL for now.

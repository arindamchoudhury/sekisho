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
    "notice": "",
    "access_groups": ["data", "github/acme:data-eng"],
    "models": ["claude-sonnet-4-5", "gpt-5", "mistral-small-3.2"]
  }'
```

| Field | Meaning |
|---|---|
| `api_version` | Optional; `sekisho/v1alpha1` if given |
| `name` | Optional; must match the name in the path. 1–63 lowercase letters, digits or hyphens, starting and ending with a letter or digit. It is also the route prefix |
| `description`, `notice` | Text for people. The notice is meant to be shown to whoever uses the environment |
| `access_groups` | Who may use it: Sekisho groups by name, or identity providers' groups as `<connector>/<group>`, such as `github/acme:platform`. No groups means no one may use it |
| `models` | The [models](providers-and-models.md#models) its routes serve, in order, from any providers. Each must exist, and none twice |
| `owner` | The owner's user ID. Without it, the owner stays the same, or the admin who creates the environment owns it. Offboarding hands an owner's environments to someone else |

The answer adds `id`, `owner_email`, `created_at` and `updated_at`. A
fetched environment can be sent back as it is: those fields are ignored.
`GET /api/v1/environments` lists every environment, and
`GET /api/v1/environments/{name}` reads one.

A change reaches the gateway in about a second, without ending requests
already running.

`DELETE /api/v1/environments/{name}` deletes the environment with its
keys, its budgets and its keys' budgets.

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

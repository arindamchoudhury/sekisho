# People and sign-in

People sign in to Sekisho through [Dex](https://github.com/dexidp/dex), with
a local account or through their identity provider, and get session tokens
that work on the API and on the gateway. Sekisho runs its own OAuth 2.1
server for its clients; Dex is only the sign-in step behind it, and admins
never edit Dex's files.

## Signing in

The `sekisho` CLI, which will do this for people, isn't built yet. Until
then, a token comes from the **device flow** (RFC 8628), which needs only
`curl` and a browser:

```bash
curl -s -d client_id=sekisho-cli https://sekisho.example.com/oauth/device_authorization
```

Open `verification_uri_complete` from the answer, check that the code
matches, press *Continue* and sign in. The code looks like `BCDF-GHJK`:
eight consonants, typed in any case, with or without the dash. Then,
polling no faster than every 5 seconds (each `slow_down` answer adds 5
more) and within 10 minutes, fetch the tokens with the answer's
`device_code`:

```bash
curl -s -d client_id=sekisho-cli -d grant_type=urn:ietf:params:oauth:grant-type:device_code \
  -d device_code=DEVICE_CODE https://sekisho.example.com/oauth/token
```

Programs with a browser can use the authorization code flow with PKCE
instead, redirecting to a loopback address (`http://127.0.0.1:<port>/…` or
`http://[::1]:<port>/…`, RFC 8252). The server's metadata is at
`/.well-known/oauth-authorization-server`, and its signing keys at
`/oauth/jwks`. The only client so far is `sekisho-cli`, a public client
with no secret.

### Tokens

- The **access token** works for an hour, as a bearer token on the API and
  on the gateway's environment routes.
- The **refresh token** gets a new access token, and a new refresh token
  with it. Each works once: a used one that comes back later ends the
  session, since someone may have copied it. A session ends after 30 days
  without a refresh.

```bash
curl -s -d client_id=sekisho-cli -d grant_type=refresh_token \
  -d refresh_token=REFRESH_TOKEN https://sekisho.example.com/oauth/token
```

`/oauth/revoke` (RFC 7009) ends a session, given its refresh token or an
access token.

Each refresh asks the identity provider again, through Dex. Keys never
refresh, so once a day Sekisho also asks again for each person who owns an
active environment key. Dex v2.46.1 reports a provider's refusal as a
failure of its own, which Sekisho treats as Dex being unavailable rather
than ending sessions, so **remove someone who leaves by
[offboarding](#offboarding) them in Sekisho**.

While Dex can't be reached, signing in and refreshing answer 503 and
change nothing; they never succeed unchecked.

On the gateway, a session that ends (logout, revocation, disabling,
offboarding) cuts that person's running streams about a second later. An
access token that merely expires during a stream doesn't cut it.

### Checking a session token

An access token is a JWT signed with EdDSA (Ed25519), with a `kid` in its
header and these claims:

| Claim | Value |
|---|---|
| `iss` | `SEKISHO_PUBLIC_URL` |
| `aud` | `sekisho` |
| `sub` | the person's user ID |
| `sid` | the session's ID |
| `client_id` | the client that signed in, such as `sekisho-cli` |
| `iat`, `exp` | issued at, and an hour later; Sekisho allows no leeway |

The public keys are at `/oauth/jwks`. A valid signature isn't the whole
check: Sekisho also refuses a token whose session has ended or whose person
is disabled, which only Sekisho knows.

Sekisho rotates the signing key every 30 days by itself: the new key is
published a minute before it signs, and a retired key stays published for
65 minutes, past the last token it signed. `POST
/api/v1/signing-keys/rotate` rotates it now, the same way.

## The first admin

A new install has no one. Open `/setup`, give the bootstrap token
(`SEKISHO_BOOTSTRAP_TOKEN`) with an email, a name and a password, and that
local account becomes the first admin. `/setup` works only until then.

An operator who has lost every admin can run `sekisho admin-link --email
<email>` on a machine with the database settings. It prints a one-time
link that makes that local account an admin, and needs nothing from Dex.

## Local accounts

Local accounts are people Sekisho knows by email and password, without an
identity provider:

```bash
curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  https://sekisho.example.com/api/v1/local-accounts \
  -d '{"email": "ada@example.com", "name": "Ada"}'
```

The answer is the new person with a `link`: `setup_url`, where they set
their password, and its `expires_at`, 7 days on. Send them the link. It
works once, and a newer link for the same person (from a reset) replaces
it.

A password needs at least 15 characters and at most 72 bytes (letters
outside plain ASCII take two to four), and must be hard to guess: zxcvbn's
top score, with the person's own name and email counted as known words. A
few unrelated words make one.

- `POST /api/v1/local-accounts/{id}/reset` removes the password at once,
  ends their sessions and answers a new link.
- `DELETE /api/v1/local-accounts/{id}` deletes the password and disables
  them.

## Identity providers

People can sign in through GitHub, GitLab, Google, Microsoft, LDAP or any
OIDC provider, each a connector at Dex. SAML isn't supported: Dex v2.46.1
gives a SAML sign-in no refresh token, so it couldn't be checked again.
Connect such a provider through OIDC instead.

```bash
curl -s -X PUT -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  https://sekisho.example.com/api/v1/connectors/github \
  -d '{"kind": "github", "name": "GitHub", "config": {"clientID": "...", "clientSecret": "...", "redirectURI": "https://sekisho.example.com/dex/callback"}}'
```

- The ID (`github` above) is 1–63 lowercase letters, digits or hyphens;
  `local` is Dex's own.
- `config` is the connector's Dex configuration, as Dex's documentation
  gives it for the connector's type; its `redirectURI` is Dex's callback,
  `SEKISHO_DEX_ISSUER` followed by `/callback`. It holds secrets, so it
  goes to Dex and is never answered back or logged.
- `GET /api/v1/connectors` lists the connectors by ID, kind and name.
- `DELETE /api/v1/connectors/{id}` deletes one. Everyone whose latest
  sign-in came through it is treated as refused by their provider: their
  sessions end, and their keys stop working until they sign in another way.

An identity provider's groups are named `<connector>/<group>` in Sekisho,
such as `github/acme:platform`, so they can never be mistaken for a Sekisho
group or another provider's group of the same name. Sekisho asks Dex for
the scopes `openid email profile groups offline_access federated:id`, but a
person's groups arrive only if the connector sends them, which some need
set in their `config` (Dex's documentation for each type says how).

## People

`GET /api/v1/users` lists everyone who has signed in or has a local
account, and `GET /api/v1/users/{id}` reads one: email, name, whether
they're local, an admin (`admin` for the mark, `admin_by_group`) or
disabled, their Sekisho groups, their identity provider's groups at their
latest sign-in, and when they last signed in.

`PATCH /api/v1/users/{id}` with `admin` (`true` or `false`) sets or removes
the admin mark, and with `disabled` disables or enables them. Disabling
ends their sessions and stops their keys. Nobody can remove their own mark
or disable themselves, so there is always an admin left.

### You

`GET /api/v1/me` answers who you are: your ID, email, name, whether your
account is local, whether you're an admin, and every group you're in.

## Groups

Sekisho's own groups sit beside identity providers' groups. Either can be
in an environment's `access_groups` or in `SEKISHO_ADMIN_GROUPS`.

| Call | What it does |
|---|---|
| `POST /api/v1/groups` with `{"name": "data"}` | Make a group (1–63 lowercase letters, digits or hyphens) |
| `GET /api/v1/groups` | Every group with its members' user IDs |
| `PUT /api/v1/groups/{name}/members/{user}` | Add someone |
| `DELETE /api/v1/groups/{name}/members/{user}` | Remove someone |
| `DELETE /api/v1/groups/{name}` | Delete a group |

Admins are the people with the admin mark, and the members of the groups
in `SEKISHO_ADMIN_GROUPS`. An admin can't remove themselves from, or
delete, the group that alone makes them an admin (409): another admin can.

## Offboarding

`POST /api/v1/users/{id}/offboard` removes someone in one step, all or
nothing: it disables them, ends their sessions (and cuts their running
streams), revokes their keys, and hands their environments to
`new_owner`:

```bash
curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  https://sekisho.example.com/api/v1/users/USER_ID/offboard \
  -d '{"new_owner": "NEW_OWNER_ID"}'
```

`new_owner` is needed when they own environments, and must be someone
else who isn't disabled. The answer lists `ended_sessions`,
`revoked_keys` and `handed_over`, and `dex_refresh_revoked`: whether Dex
dropped their sign-in too. It is `false` when Dex couldn't be reached;
Sekisho's part is done anyway. Nobody can offboard themselves.

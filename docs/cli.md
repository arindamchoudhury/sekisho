# The `sekisho` CLI

Developers sign in to Sekisho with the same `sekisho` binary the server
runs. Today it signs in, keeps the session fresh and signs out, lists the
environments you may use, and manages your environment keys; installing an
environment into coding tools comes next.

## Installing it

There are no release binaries yet. Build it from a clone with Rust 1.99.1:

```bash
cargo install --path crates/sekisho-server
```

## Signing in

```bash
sekisho login https://sekisho.example.com
```

`login` opens your browser at Sekisho's sign-in page and waits up to 10
minutes. After you sign in, the browser comes back to a port on
`127.0.0.1` that `login` is listening on, and the page says you can close
the tab. Without a URL, `login` uses the server already set, or
`SEKISHO_URL`.

| Option | What it does |
|---|---|
| `--no-browser` | Prints the sign-in link on stderr and doesn't open a browser. The browser must still be on this machine, since it comes back to `127.0.0.1` |
| `--device` | For a machine without a browser, such as one you reach over SSH: prints a code and a link, which you open on any device to sign in. `login` checks every 5 seconds (longer when Sekisho asks it to slow down) until you approve, for up to 10 minutes |

The browser sign-in is OAuth's authorization code flow with PKCE; `login`
refuses an answer that doesn't carry the sign-in's own `state`, or that names
another server as its issuer, and keeps waiting. Both flows sign in as the
client `sekisho-cli` ([People and sign-in](people-and-sign-in.md)).

`login` keeps the server's URL and your email in `config.json`, and the
tokens in the operating system's credential store (below). It is signed in
to one server at a time: signing in to another ends the session on the
first and forgets its tokens.

The server URL must start with `https://`, or with `http://` only for this
computer (`127.0.0.1`, `[::1]` or `localhost`). It can't hold a user name,
password, query or fragment. The CLI never follows a redirect: if Sekisho
answers with one, the command fails and names the status, so check the URL.

## Where tokens are kept

Tokens never go in a file. `login` keeps the session's access token, its
expiry and its refresh token together as one entry in the credential store,
service `sekisho`, account the server's URL:

| System | Store | Who can read the entry |
|---|---|---|
| macOS | The login Keychain | Programs you allow; the Keychain asks |
| Windows | Credential Manager, as the generic credential `sekisho:<server URL>`. It is kept on this machine (*Local machine persistence*), so it never roams with your profile to other machines | Any program running as you |
| Linux with a desktop | Secret Service (GNOME Keyring, KWallet) | Any program running as you |
| Linux without Secret Service | The kernel's keyutils: your session keyring, and your persistent keyring where the kernel has one. It is kept in memory only, so a reboot loses it and you sign in again | Any program running as you |

On Windows, `cmdkey /list` shows the entry.

Some sessions have no credential store at all: Windows reached over SSH
with a key, or a Windows service or scheduled task that doesn't store your
password (Credential Manager needs your password to open), or a Linux
machine with neither Secret Service nor keyutils. There `login` says so
and stops before signing in, since it couldn't keep the session. Use
`SEKISHO_URL` with `SEKISHO_KEY` instead.

WSL runs the Linux build, so a WSL distribution keeps its own sign-in, in
Secret Service or keyutils, apart from Windows' Credential Manager. Sign in
in each.

If the store refuses to save after you have signed in, `login` warns that
the sign-in isn't kept.

## Fresh tokens

```bash
sekisho token
```

prints a token and a newline on stdout, and nothing else, so a coding tool
can run it for its key. The access token is reused until 5 minutes before
it expires (it lasts an hour); then `token` refreshes it. Refresh tokens
work once, and a used one that comes back ends the session, so a refresh
runs under a lock file that other `sekisho` processes wait for:
`refresh.lock` in `~/.cache/sekisho`, or `%LOCALAPPDATA%\sekisho` on
Windows, which never roams. When the session has ended (signed out,
revoked, or 30 days without a refresh), `token` forgets it and exits with
4.

## Signing out

```bash
sekisho logout
```

ends the session at Sekisho and forgets its tokens. They are forgotten even
when Sekisho can't be reached; then `logout` warns that Sekisho didn't
confirm the end, and the session lasts until its refresh token would have
expired.

## Your environments

```bash
sekisho env list
```

lists the environments you may use, with their models and descriptions:

```text
  NAME  MODELS                    MODELS FOR          DESCRIPTION
  ops
* pay   claude-sonnet-4-5, gpt-5  claude-code, codex  Payments team
```

A `*` marks an environment installed in the current folder (from its
`.sekisho/lock.json`, which `sekisho use` will write), and `MODELS FOR`
names the coding tools in this folder that take their models from it. A
lock file that isn't Sekisho's stops the command with an error naming it.

`--json` prints a list of `{name, description, notice, models, installed,
model_tools}`.

## Environment keys

A key lets a machine that can't sign in, such as a CI job, use one
environment ([Environment keys](environments-and-keys.md#environment-keys)).

```bash
sekisho keys create data-team --name ci --expires 90d
```

prints the key's token on stdout, alone, and warns on stderr that this is
the only time it is shown: Sekisho keeps only a hash of it, and the CLI
keeps it nowhere. `--expires` takes a number of days (`30d`, up to 36,500),
a date (`2027-03-01`, midnight UTC) or an RFC 3339 time, in the future;
without it the key doesn't expire. A name is unique in its environment,
among everyone's keys, revoked ones included.

| Command | What it does |
|---|---|
| `sekisho keys list [--env <env>] [--json]` | Your keys: ID, name, environment, state and expiry, never a token. `--json` prints `{id, environment, name, state, suspended_by_admin, expires_at, created_at}` for each |
| `sekisho keys suspend <key> [--env <env>]` | Pause a key until you resume it |
| `sekisho keys resume <key> [--env <env>]` | Let it work again (not one an admin suspended) |
| `sekisho keys revoke <key> [--env <env>]` | End it for good |

`<key>` is the key's ID, or the name of one of your keys that isn't
revoked. When keys in several environments share the name, the command
stops (exit 2) until `--env` picks one.

`env list` and `keys` act as you, so they need `sekisho login`: with
`SEKISHO_KEY` set they stop with exit 4.

## Settings

| Variable | Meaning |
|---|---|
| `SEKISHO_URL` | The server, used before the one in `config.json` |
| `SEKISHO_KEY` | An environment key. `sekisho token` prints it, and nothing is read from the credential store. With `SEKISHO_URL`, no config file is needed: for CI |
| `SEKISHO_CONFIG_DIR` | One folder for `config.json` and `refresh.lock`, instead of the defaults below |

`config.json` holds the server URL and your email only, and on Linux and
macOS only you can read it. It is in `~/.config/sekisho`, or
`%APPDATA%\sekisho` on Windows.

## Output and exit codes

Results go to stdout; progress and warnings to stderr. An error prints
`error: <what>` and, where there is one, `hint: <what to do>`.

| Exit code | Meaning |
|---|---|
| 0 | Done |
| 1 | An error |
| 2 | The command line was wrong |
| 3 | Refused: a check failed, or an answer is needed and there is no terminal |
| 4 | Not signed in |

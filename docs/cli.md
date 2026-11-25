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
4. Inside [`sekisho run`](#starting-a-tool-with-sekisho-run), `token`
asks the run's proxy instead.

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
`.sekisho/lock.json`, which [`sekisho use`](#installing-an-environment) writes), and `MODELS FOR`
names the coding tools in this folder that take their models from it. A
lock file that isn't Sekisho's stops the command with an error naming it.

`--json` prints a list of `{name, description, notice, models, installed,
model_tools}`.

## Installing an environment

```bash
sekisho use pay
```

writes the environment `pay` into the coding tools of the current folder:
its models, local MCP servers, skills, plugins, prompts and
[instructions](#instructions). Today
`sekisho use` installs for Claude Code, Gemini CLI and Qwen Code; the
other tools follow.

| Option | What it does |
|---|---|
| `--target <tool>,…` | The tools to install for (`claude-code`, `gemini-cli`, `qwen-code`). Without it, the tools found here: a tool's folder in the project (`.claude/`, `.gemini/`, `.qwen/`), or its program on `PATH` (`claude`, `gemini`, `qwen`). None found is an error listing the targets |
| `--global` | Your own config instead, for every folder: `~/.claude` (or `$CLAUDE_CONFIG_DIR`), `~/.gemini` (or `.gemini` in `$GEMINI_CLI_HOME`), `~/.qwen` (or `$QWEN_HOME`) |
| `--dir <path>` | The project folder, instead of the current one |
| `--models` | Make this environment the one these tools take their models from |
| `--dry-run` | Show what would change, with each update's diff, and write nothing |
| `--yes` | Overwrite your own files and settings that are in the way, without asking |
| `--force` | Replace or remove files and settings you edited since Sekisho wrote them |

It works signed in, or with an [environment key](#environment-keys) in
`SEKISHO_KEY`: a CI job can install the environment its key is for.

Every file is checked before anything is written: its SHA-256 against
what the server sent, its size, hidden Unicode characters (a dangerous
one stops the install; others are listed as warnings, but any invisible
character in an instruction stops it), and its path,
which can't leave its folder or pass through a symbolic link. Each file is
written whole and at once; if a later write fails, everything already
written is put back, and the lock file is unchanged.

`use` prints what it changed, one line each:

```text
+ claude-code: .claude/skills/pdf/SKILL.md
~ claude-code: .claude/settings.local.json model
- claude-code: .claude/commands/old-review.md
! claude-code: .claude/skills/deploy/SKILL.md
Claude Code takes its models from "pay".
```

`+` added, `~` updated, `-` removed (the environment no longer delivers
it, or it is quarantined), `!` kept: you edited it since Sekisho wrote
it, so it is left as you made it (`--force` replaces it). A setting in a
shared file is named by its key, such as
`.mcp.json mcpServers.fetch`. Running `use` again brings the folder up
to the environment's current version.

### Your own files

Sekisho never overwrites a file, or a setting in a shared config file,
that it didn't write and that holds something else: `use` lists them and
asks on the terminal, and without a terminal it stops with exit 3 (run
again with `--yes` to overwrite them). The same content already in place
is taken over quietly. Settings files are changed key by key, so your
other keys, comments and order are kept, and `uninstall` takes out only
Sekisho's keys.

### The lock file

`.sekisho/lock.json` in the project (or `global/lock.json` in Sekisho's
config folder for `--global`) records each installed environment, which
tool takes its models from which, and every file and setting Sekisho
wrote, with a SHA-256 of what it wrote. It holds no token or key. In a
Git repository, `.sekisho/` and the files Sekisho wrote are listed in a
marked block of `.git/info/exclude`, never `.gitignore`; `uninstall`
removes the block. A file already tracked by Git, such as a committed
`.mcp.json`, still shows Sekisho's change as a change.

### Several environments in one folder

Environments add up: `sekisho use ops` where `pay` is installed adds
`ops`'s skills, servers, plugins, prompts and instructions and keeps `pay`'s. Each tool
takes its models from one of them: the first installed, until
`sekisho use <env> --models` moves them. The same catalog entry in two
environments is written once and stays until neither delivers it; a newer
version of it, from either, is written for both. A name two environments
deliver with different content, such as two prompts called `deploy` from
different sources, stops the `use` before anything is written (exit 3,
naming the other environment): ask an admin to rename one, or uninstall
the other. So does an instruction name two environments deliver from
different sources with different text, though each environment's
instruction files carry its name. Skills and local MCP servers can't
clash this way, since their names are
[unique across the install](catalogs.md#names).

### Instructions

An environment's [instructions](catalogs.md#instructions) become files of
Sekisho's own that each tool loads in every session, one per instruction
(below for each tool). Each starts with a heading saying where it comes
from, its text's own first heading (or its name) then the environment and
the instruction:

```markdown
# Payments style (Sekisho environment pay, instruction style)

Short functions. Money is integer micro-USD.
```

A plugin's APM instructions (`instructions/*.instructions.md`) are
written the same way, as `<plugin>-<name>`, with `plugin <plugin>` in the
heading. One whose `applyTo` is `**`, or that has none, loads in every
session; any other `applyTo` makes it apply only to those files, which
Claude Code and Qwen Code support (as `paths:`) and Gemini CLI doesn't, so
it is skipped there with a warning.

**Sekisho never writes a repository's or your own instructions file**:
`AGENTS.md`, `CLAUDE.md`, `.claude/CLAUDE.md`, `AGENTS.override.md` or
`.github/copilot-instructions.md` in a project, nor `AGENTS.md`,
`CLAUDE.md` or `AGENTS.override.md` in a tool's own folder. Those belong
to the repository or to you, and writing one would change what the tool
reads: in Claude Code, a `CLAUDE.md` stops it reading the repository's
`AGENTS.md`. A plugin's top-level `AGENTS.md` is skipped with a warning.

When the environments installed for a tool together load more than
16 KiB of instructions in every session, `use` warns: every session pays
for them, and one environment may deliver at most that much.

### Beside a global install

A project install and a `--global` one both stay, and each tool combines
them its own way. `use` says what a session in the folder also gets from
a global environment that the folder doesn't hold, and, for a name both
define, whose copy runs. Claude Code uses the folder's model settings,
loads MCP servers, skills, prompts and plugins from both, and on a shared
name runs the folder's MCP server but the global skill or prompt (a
personal skill beats a project one):

```text
Claude Code also loads the global environment "tools": skill "deploy" (the session uses "tools"'s), MCP server "db".
```

Gemini CLI and Qwen Code use the folder's model settings too, and on a
shared name run the folder's MCP server and skill. A prompt both define
is kept twice, one of them renamed with a prefix.

### What Claude Code gets

| What | In a project | With `--global` |
|---|---|---|
| Model settings | `.claude/settings.local.json` | `settings.json` in `~/.claude` or `$CLAUDE_CONFIG_DIR` |
| Local MCP servers | `.mcp.json`, approved in `enabledMcpjsonServers` | `mcpServers` in `.claude.json` (`~/.claude.json`, or in `$CLAUDE_CONFIG_DIR`) |
| Skills | `.claude/skills/<name>/` | `skills/<name>/` |
| Prompts | `.claude/commands/<name>.md` | `commands/<name>.md` |
| Plugins | a marketplace `sekisho-<env>` in `.sekisho/claude-plugins/<env>/` | the same in Sekisho's config folder |
| Instructions | `.claude/rules/sekisho-<env>-<name>.md` | `rules/sekisho-<env>-<name>.md` |

The model settings, written only by the folder's model environment and
only when the environment has a model the Anthropic API reaches (else a
warning says so):

| Setting | Value |
|---|---|
| `apiKeyHelper` | `sekisho token`, so Claude Code asks for a fresh token itself; `sekisho`'s full path when it isn't on `PATH` |
| `model` | the environment's `anthropic` [default model](environments-and-keys.md#default-models) |
| `env.ANTHROPIC_BASE_URL` | `<gateway>/env/<name>/anthropic` |
| `env.ANTHROPIC_DEFAULT_OPUS_MODEL`, `env.ANTHROPIC_DEFAULT_SONNET_MODEL`, `env.CLAUDE_CODE_SUBAGENT_MODEL` | the default model |
| `env.ANTHROPIC_DEFAULT_HAIKU_MODEL` | the small model, else the default |
| `env.CLAUDE_CODE_API_KEY_HELPER_TTL_MS` | `3000000`: a token is asked for again after 50 minutes |

A local MCP server's settings from your own environment are written as
references, `${GITHUB_TOKEN}` (or `${LOG_LEVEL:-}` for one it can do
without), never as values. Plugins are turned on in `enabledPlugins` as
`<plugin>@sekisho-<env>`, from the marketplace registered in
`extraKnownMarketplaces`; Claude Code reads that registration from a
project's settings only once you trust the folder.

`.claude/settings.local.json` is yours alone and applied over the
committed `.claude/settings.json`, so in your sessions Sekisho's settings
win. A teammate who hasn't run `sekisho use` gets the committed file's
`model`, `env` and `apiKeyHelper`, if it sets them.

Instructions are rules: Claude Code loads `.claude/rules/` at launch with
the priority of `.claude/CLAUDE.md`, and rules don't count as a
`CLAUDE.md`, so the repository's `AGENTS.md` still loads beside them. A
plugin's instructions are written there too, since Claude Code loads no
instructions from inside a plugin; a path-scoped one gets `paths:`.

### What Gemini CLI gets

| What | In a project | With `--global` |
|---|---|---|
| Model settings, MCP servers | `.gemini/settings.json` | `settings.json` in `~/.gemini` (or `.gemini` in `$GEMINI_CLI_HOME`) |
| Skills | `.gemini/skills/<name>/` | `skills/<name>/` |
| Prompts | `.gemini/commands/<name>.toml` | `commands/<name>.toml` |
| Instructions | `.gemini/sekisho/<env>/<name>.md`, imported by `GEMINI.md` | an extension `extensions/sekisho-<env>/` |

The model settings, when the environment has a model the Gemini API
reaches: `model.name`, the `gemini` [default model](environments-and-keys.md#default-models),
and `security.auth.selectedType` `gemini-api-key`. Start Gemini CLI with
[`sekisho run gemini-cli`](#starting-a-tool-with-sekisho-run), which gives
it `GOOGLE_GEMINI_BASE_URL` and `GEMINI_API_KEY`. (Gemini CLI's own
`gateway` auth type is refused by its terminal interface, so the API-key
type, which reads the same base URL, is used.)

Gemini CLI reads a folder's `.gemini/` (settings, skills, commands and
local MCP servers) only once you trust the folder: accept its prompt, or
use `/permissions`. Until then, your own `~/.gemini/settings.json` applies.

Each prompt becomes a TOML command, with its frontmatter's `description`
and `$ARGUMENTS` written as `{{args}}`. A prompt whose text has `!{…}` or
`@{…}` is skipped with a warning: Gemini CLI would run the first as a
shell command and read the second as a file to inject.

A local MCP server's settings from your environment are written as
`${GITHUB_TOKEN}` references: Gemini CLI hands its servers only a cleaned
copy of its environment, without names like `*TOKEN*` or `*KEY*`, so every
one is named, and one you haven't set reaches the server empty.

Gemini CLI reads instructions only from its context files, `GEMINI.md`
by default, so in a project each instruction is a file under
`.gemini/sekisho/<env>/` and `GEMINI.md` gets one block importing them:

```markdown
<!-- sekisho:start: managed by `sekisho use`; edits are overwritten -->
@./.gemini/sekisho/pay/style.md
<!-- sekisho:end -->
```

A `GEMINI.md` that doesn't exist is created, and excluded from Git like
Sekisho's other files. One you wrote is asked about before the block is
added at its end ([Your own files](#your-own-files)), and only the block
is ever changed; the last `uninstall` takes the block out, and the file
too if Sekisho made it and nothing else is left. If your
`context.fileName` setting reads other files and not `GEMINI.md`, the
instructions are skipped with a warning: Sekisho never writes those
files. With `--global`, each environment gets an extension of its own,
`~/.gemini/extensions/sekisho-<env>/`, whose `SEKISHO.md` holds its
instructions; Gemini CLI ignores it if your `security.allowedExtensions`
setting is set.

Gemini CLI has no project folder for extensions and doesn't read Claude
Code plugins, so a plugin is taken apart: its skills go to `skills/`, its
commands to `commands/<plugin>/` (`/<plugin>:<command>`), and its MCP
servers to `mcpServers` as `<plugin>-<server>`. A server run through a
package runner gets the same publish-time cutoff and registries as a
catalog server; one that runs a file of the plugin's runs it from a copy
of the plugin in `.sekisho/plugins/<env>/<plugin>/` (or Sekisho's config
folder for `--global`). Its hooks, agents and container servers are
skipped, with a warning.

### What Qwen Code gets

| What | In a project | With `--global` |
|---|---|---|
| Model settings, MCP servers | `.qwen/settings.json` | `settings.json` in `~/.qwen` (or `$QWEN_HOME`) |
| Skills | `.qwen/skills/<name>/` | `skills/<name>/` |
| Prompts | `.qwen/commands/<name>.toml` | `commands/<name>.toml` |
| Instructions | `.qwen/rules/sekisho-<env>-<name>.md` | `rules/sekisho-<env>-<name>.md` |

Qwen Code takes one provider for a session: the Anthropic Messages API
when the environment has an `anthropic` default model, else Chat
Completions with its `openai` default. The model settings:

| Setting | Value |
|---|---|
| `modelProviders.anthropic` (or `.openai`) | every model of the environment that API reaches, each with `baseUrl` `${SEKISHO_RUN_URL}/env/<name>/anthropic` (or `…/openai/v1`, with `wireApi` `chat-completions`) and `envKey` `SEKISHO_RUN_KEY` |
| `security.auth.selectedType` | `anthropic` (or `openai`) |
| `model.name` | the default model |

Qwen Code fills in `${SEKISHO_RUN_URL}` from its environment when it reads
its settings, so these settings work only inside
[`sekisho run qwen-code`](#starting-a-tool-with-sekisho-run). Prompts and
plugins are written as for Gemini CLI. Qwen Code's MCP servers inherit
its environment, so a server's setting with the same name as your
variable isn't written at all.

Instructions are rules, which Qwen Code loads at session start (a
plugin's path-scoped ones, with `paths:`, when a matching file is
touched). It reads a project's `.qwen/rules/` only at the root of its Git
repository, and only in a trusted folder when you have turned folder
trust on: `use` says so when the folder is inside a repository but not at
its root.

## Starting a tool with `sekisho run`

```bash
sekisho run gemini-cli
sekisho run qwen-code -- -p "Explain this repository"
```

starts the tool with its arguments (after `--`) and the terminal handed
over, and exits with the tool's exit code. Gemini CLI and Qwen Code need
it; Claude Code doesn't, since it runs `sekisho token` itself.

The run takes the folder's install for the tool, else your global one:
the environment the tool takes its models from, and the others installed
for it. It asks the server for that environment's delivery document, and
from it alone decides which program starts and where tokens go, never
from the lock file, which a repository could carry. Then it listens on
`127.0.0.1` at a free port with a new random key, and starts the tool
with:

| Variable | Value |
|---|---|
| `SEKISHO_RUN_URL` | the proxy, `http://127.0.0.1:<port>` |
| `SEKISHO_RUN_KEY` | the run's key, which works only on that proxy and only while the run lasts |
| Gemini CLI: `GOOGLE_GEMINI_BASE_URL`, `GEMINI_API_KEY` | `$SEKISHO_RUN_URL/env/<name>/gemini`, and the run's key |

The proxy answers only requests that carry the run's key (as
`Authorization: Bearer`, `x-api-key` or `x-goog-api-key`), only under
`/env/<name>/` for the environments installed for the tool; anything
else is 401 or 404 and goes nowhere. It removes every credential the tool
sent (and a `key` in the query), adds `Authorization: Bearer` with a fresh
token, and streams the request and the answer through to the gateway. The
token is reused and refreshed as `sekisho token`'s is, so a run can last
longer than an hour. A redirect from the gateway is answered with 502 and
never followed. When the run has no token (your session has ended), the
tool gets 401 with Sekisho's reason.

Programs the tool starts, such as its shell commands and MCP servers, see
`SEKISHO_RUN_URL` and `SEKISHO_RUN_KEY`, and `sekisho token` inside the
run asks the proxy for a token, so it works without a keyring. Where no
keyring answers, `sekisho run` signs you in with a device code for the
run's lifetime and keeps the tokens in memory only. With `SEKISHO_KEY`
set, the proxy sends the environment key instead.

Ctrl+C reaches the tool from the terminal; the run waits for the tool to
finish.

### Removing an environment

```bash
sekisho uninstall pay
```

removes `pay` from the folder (or, with `--global`, your own config):
every file and setting it wrote that no other installed environment still
delivers, keeping the ones you edited (`!`, unless `--force`). With no
environment named, it removes every one. `--target`, `--dir`,
`--dry-run`, `--yes` and `--force` work as for `use`. When the
environment the tools took their models from goes, the earliest
installed one left takes them over; with none left, the model settings
go too. An environment that isn't installed is an error.

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
| `SEKISHO_RUN_URL`, `SEKISHO_RUN_KEY` | Set by [`sekisho run`](#starting-a-tool-with-sekisho-run) for the tool it starts; `sekisho token` then asks the run's proxy |

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
| 3 | Refused: a check failed, a name clashes with another environment's, or an answer is needed and there is no terminal |
| 4 | Not signed in |

Once the tool has started, `sekisho run` exits with the tool's own exit
code.

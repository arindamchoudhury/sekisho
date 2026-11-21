# Testing on Linux

`make check` proves that Sekisho builds, passes every test and runs its
end-to-end stack on a Linux machine. It runs these targets in order and
stops at the first that fails:

| Target | What it proves |
|---|---|
| `make prereqs` | cargo, git 2.37 or later, Docker and Compose v2 are there |
| `make build` | The workspace and the release binaries build with the locked dependencies; `sekisho --help` lists `serve`, `migrate`, `admin-link`, `gateway-agent`, `login`, `logout`, `token`, `env`, `keys`, `use` and `uninstall` |
| `make lint` | `cargo fmt --check` and `cargo clippy -D warnings` are clean |
| `make test` | Every workspace test passes (about 614); database tests start their own `postgres:18` through Docker |
| `make unix` | What only Linux and other Unix systems exercise, [below](#what-make-unix-checks) |
| `make images` | The release image and the end-to-end image build straight from the `Dockerfile`; the release image's `--version` works, and the end-to-end image has git 2.37 or later and runs as uid 65532 |
| `make e2e` | The end-to-end suite passes twice in a row on the same stack, then the scale test passes once, then Sekisho stops cleanly on SIGTERM; the stack is removed after |

Each target also runs on its own. Plan on about an hour on a 4-core
machine for `make check`, most of it the first build and the images.
[Testing on Linux by hand](testing-on-linux-by-hand.md) gives the same
checks as commands to run one at a time. On Windows, run them in WSL 2:
[Testing on WSL 2](testing-on-wsl.md) sets it up.

## Prerequisites

Any x86-64 or arm64 Linux with these works; Ubuntu 24.04 and Debian 13 are
the easiest.

| Need | Version or size | Why |
|---|---|---|
| GNU make and bash | any recent | The `Makefile` uses bash |
| Rust through `rustup` | 1.99.1, from `rust-toolchain.toml` | `rustup` installs it with clippy and rustfmt on the first `cargo` call |
| C compiler and linker | `build-essential` (gcc, make) | `ring` (TLS) compiles C |
| git | **2.37 or later** | Catalog syncs refuse older git, which ignores the address pin ([Running Sekisho](running.md)). Ubuntu 22.04's 2.34 is too old; 24.04 has 2.43, Debian 13 2.47 |
| Docker Engine with the Compose plugin | Compose v2 | Database tests start `postgres:18` themselves; the end-to-end stack runs on Compose |
| Memory | 8 GB or more | The end-to-end stack runs seven containers |
| Disk | about 15 GB free | `target/`, the images, PostgreSQL |
| Free ports on 127.0.0.1 | 18080, 3000, 5556, 18000 | The stack publishes Sekisho, agentgateway, Dex and the mock there |
| Network | crates.io, Docker Hub, `ghcr.io`, `cr.agentgateway.dev` | Crates and images download on first use |

On Ubuntu or Debian:

```bash
sudo apt-get update
sudo apt-get install -y build-essential git curl ca-certificates pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
. "$HOME/.cargo/env"
# Docker Engine and Compose: https://docs.docker.com/engine/install/
sudo usermod -aG docker "$USER"   # then log out and in again
```

## Run it

```bash
git clone https://github.com/arindamchoudhury/sekisho.git
cd sekisho
make check
```

Each check prints a line starting `ok`, and the run ends with
`All checks passed.` A failure prints a line starting `FAIL` that says what
failed and which log to read, and `make` exits non-zero.

Test output goes to logs in `target/check/`: `workspace.log`,
`unix-*.log`, `e2e-1.log`, `e2e-2.log` and `e2e-scale.log`. `make help`
lists the targets.

The scale test leaves 300 environments in the stack, which makes every
later config load take about half a second, so `make e2e` leaves it out of
both runs and runs it once at the end.

To look around the end-to-end stack after the suite, keep it running with
`make e2e KEEP=1`, then remove it with `make e2e-down`.

## What `make unix` checks

`make test` already runs these tests; `make unix` runs each on its own and
checks it ran the expected number of tests, so a test skipped or compiled
out shows up as a failure. On Windows some are skipped, compiled out or
take another path, and `make unix` refuses to run there.

| What | Tests |
|---|---|
| Real symlinks in a source are refused, with no case skipped (Windows skips them without the symlink privilege) | `sekisho-catalog` `catalog_source::symlinks_and_escaping_paths_are_refused` |
| A symlink and a submodule committed to Git are refused after checkout | `sekisho-server` `catalog_sync::symlinks_and_submodules_in_git_are_refused` |
| A skill's executable bit is kept (Unix only) | `sekisho-catalog` `content_sources::a_skills_executable_bit_is_kept` |
| The gateway agent writes its config file with mode `0600` (Unix only) | `sekisho-gateway` `agent::write_atomic_makes_the_file_private` |
| Installing an environment never writes through a symlink, and a failed install puts back a script's executable bit (Unix only) | `sekisho-cli` `install::symlinks_are_never_written_through`, `install::a_rollback_keeps_the_executable_bit` |
| Git runs hardened, with `GIT_CONFIG_GLOBAL=/dev/null` (Windows uses `NUL`), pinned to the checked address | `sekisho-server` `catalog_sync::git_pins_the_checked_address`, `catalog_sync::the_token_never_appears_in_errors_or_argv` |
| Only git 2.37 or later is used | `sekisho-server` `catalog::git` `only_git_that_honours_the_pin_is_used`, `every_git_call_is_hardened` |

SIGTERM, which Docker sends on stop, is checked by `make e2e`: Sekisho
handles it on Unix only, so it must exit 0 well within Compose's 10-second
stop timeout and log `shutting down`.

## Troubleshooting

| Symptom | Likely cause | What to do |
|---|---|---|
| `FAIL  Docker isn't reachable` | Docker isn't running, or your user can't reach its socket | `docker run --rm hello-world` must work without `sudo`; add yourself to the `docker` group and log in again. Rootless Docker: export `DOCKER_HOST` to its socket |
| `FAIL  git … is older than 2.37` | The host's git is older (Ubuntu 22.04 has 2.34) | Install a newer git (on Ubuntu, the `git-core` PPA), then run again |
| Database tests time out on the first run | `postgres:18` is still downloading | `docker pull postgres:18`, then run again |
| Many `postgres:18` (and `dex`) containers, running or stopped, in `docker ps --all` | A test run was stopped before its tests could remove their containers | The next test run removes those over two hours old. To remove them now, when no other test run is going: `docker rm --force --volumes $(docker ps --all --quiet --filter label=sekisho.test.started)` |
| Catalog sync tests can't run `git http-backend` | git installed without its helpers | Install the distribution's full `git` package |
| A gateway agent timing test fails once (`… not seen within 2s`) | A loaded machine; its windows are 2 to 3 seconds | Run `cargo test -p sekisho-gateway --test agent` alone; it is a bug only if it fails there too |
| `docker compose up --wait` fails on a port | Something else holds 18080, 3000, 5556 or 18000 | Stop it, or run on another machine |
| End-to-end tests refuse every environment save with 409 "kept in Git" | A stopped Git mode test left Git mode on | `make e2e-down`, then `make e2e` again |
| End-to-end tests get 404 where they expect 200, soon after creating an environment | The scale test ran earlier on this stack, and its 300 environments slow every config load | `make e2e-down`, then `make e2e` again; run the scale test only last |
| Docker problems on WSL 2 (no `/var/run/docker.sock`, `exec format error`) | Docker Desktop's setup for WSL | See [Testing on WSL 2](testing-on-wsl.md#troubleshooting) |
| Builds or containers get killed | Out of memory | Use 8 GB or more; build with `CARGO_BUILD_JOBS=2 make check`; don't run two stacks at once |

If something fails that isn't here, keep its log from `target/check/` and
note which target failed.

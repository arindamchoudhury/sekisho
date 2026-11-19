# Testing on Linux by hand

These steps check that Sekisho builds, passes every test and runs its
end-to-end stack on a Linux machine. They are the commands that
[`make check`](testing-on-linux.md) runs, one at a time, for when you want
to see each one, or have no `make`. They cover, in order:

1. the build, `cargo fmt` and `cargo clippy`;
2. the whole workspace's tests;
3. what only Linux and other Unix systems exercise: real symlinks,
   executable bits, the gateway agent's `0600` config file, git's
   `/dev/null` config, and SIGTERM;
4. the Docker images;
5. the end-to-end suite, twice, then the scale test once.

Plan on about an hour on a 4-core machine, most of it the first build and
the images. Record each result in the [checklist](#results).

## Prerequisites

Any x86-64 or arm64 Linux with these works; Ubuntu 24.04 and Debian 13 are
the easiest. On Windows, use WSL 2 with Docker Desktop:
[Testing on WSL 2](testing-on-wsl.md) sets it up, then continue here.

| Need | Version or size | Why |
|---|---|---|
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

Before starting, check that `git --version` shows 2.37 or later, and that
`docker run --rm hello-world` works without `sudo`.

Then get the code at the commit to test:

```bash
git clone https://github.com/arindamchoudhury/sekisho.git
cd sekisho
git log -1 --oneline
```

## Step 1: build, format and lint

The build must succeed with the locked dependencies, and fmt and clippy
must report nothing.

```bash
cargo build --workspace --locked --all-targets
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Expected: each ends with `Finished` and exits 0; `cargo fmt` prints
nothing. The first build downloads the toolchain and about 450 crates, and
takes 5 to 15 minutes.

Also build the release binaries the image ships, and check they start:

```bash
cargo build --release --locked -p sekisho-server -p sekisho-testkit --bins
./target/release/sekisho --help
```

Expected: `--help` lists the commands `serve`, `migrate`, `admin-link` and
`gateway-agent`.

## Step 2: the workspace tests

Every test must pass. Database tests start their own `postgres:18`
container each through Docker (testcontainers), so Docker must be running.

```bash
docker pull postgres:18   # once, so the first suite doesn't time out pulling it
cargo test --workspace --locked --no-fail-fast 2>&1 | tee workspace-tests.log
grep 'test result' workspace-tests.log | awk '{p+=$4; f+=$6} END {print "passed", p, "failed", f}'
grep -E 'FAILED|panicked' workspace-tests.log
```

Expected: about 614 passed and 0 failed, and the last `grep` prints
nothing. `--no-fail-fast` keeps going after a failing suite, so one failure
doesn't hide others. The run takes 5 to 10 minutes; most of it is starting
PostgreSQL containers. The end-to-end tests are marked `#[ignore]` and run
in [Step 5](#step-5-the-end-to-end-suite-twice).

If a test fails, run its file alone before calling it a bug, for example
`cargo test -p sekisho-gateway --test agent`. The gateway agent's timing
tests wait at most 2 to 3 seconds and can fail on a loaded machine.

## Step 3: what only Linux exercises

Step 2 already runs these; run them on their own to see them pass rather
than skip. On Windows some are skipped, compiled out or take another path.

| What | Command | Expected |
|---|---|---|
| Real symlinks in a source are refused (Windows skips these cases without the symlink privilege) | `cargo test -p sekisho-catalog --test catalog_source symlinks_and_escaping_paths_are_refused -- --nocapture` | `1 passed`, and **no** line `skipping the symlink case` |
| A symlink and a submodule committed to Git are refused after checkout | `cargo test -p sekisho-server --test catalog_sync symlinks_and_submodules_in_git_are_refused` | `1 passed` |
| A skill's executable bit is kept (Unix only) | `cargo test -p sekisho-catalog --test content_sources a_skills_executable_bit_is_kept` | `1 passed` |
| The gateway agent writes its config file with mode `0600` (Unix only) | `cargo test -p sekisho-gateway --test agent write_atomic_makes_the_file_private` | `1 passed` |
| Git runs hardened, with `GIT_CONFIG_GLOBAL=/dev/null` (Windows uses `NUL`), pinned to the checked address | `cargo test -p sekisho-server --test catalog_sync -- git_pins_the_checked_address the_token_never_appears_in_errors_or_argv` | `2 passed` |
| Only git 2.37 or later is used | `cargo test -p sekisho-server --lib -- only_git_that_honours_the_pin_is_used every_git_call_is_hardened` | `2 passed` |

SIGTERM, which Docker sends on stop, is checked in Step 5: Sekisho handles
it on Unix only.

## Step 4: the Docker images

Both images must build straight from the repository's `Dockerfile`.

```bash
# The release image, as an operator builds it (no e2e feature)
docker build -t sekisho:linux-test .
docker run --rm sekisho:linux-test --version

# The image the end-to-end stack uses (built with sekisho-server/e2e)
docker compose -f e2e/docker-compose.yml build
docker run --rm --entrypoint git sekisho-e2e:dev --version
docker run --rm --entrypoint id sekisho-e2e:dev
```

Expected:

- both builds finish; each takes 5 to 15 minutes the first time;
- `--version` prints `sekisho 0.1.0`;
- `git --version` in the image prints 2.37 or later (Debian trixie ships
  2.47);
- `id` prints `uid=65532 gid=65532`: Sekisho runs as that user, as
  agentgateway does, so both can share the config volume.

## Step 5: the end-to-end suite, twice

The suite must pass every test, twice in a row on the same stack: the
second run proves that tests don't depend on a fresh database. The stack
runs agentgateway v1.7.0, Dex v2.46.1, PostgreSQL 18 and Sekisho, with a
mock that serves the model APIs, Git, a registry, agent cards and GitHub.

```bash
docker compose -f e2e/docker-compose.yml up -d --wait
docker compose -f e2e/docker-compose.yml ps
for run in 1 2; do
  cargo test -p sekisho-e2e --locked --no-fail-fast -- --ignored --test-threads=1 \
    --skip many_environments_add_no_latency 2>&1 | tee e2e-$run.log
  grep 'test result' e2e-$run.log | awk '{p+=$4; f+=$6} END {print "run", '$run', "passed", p, "failed", f}'
done
cargo test -p sekisho-e2e --locked --test scale -- --ignored --nocapture 2>&1 | tee e2e-scale.log
```

Expected: `up` ends with every service healthy or, for `migrate` and
`dex-db`, exited after their one job. Each run shows 64 passed and 0
failed (more once tests are added), and the scale test 1 passed. Each run
takes 3 to 6 minutes; the Git mode test alone waits up to 150 s for a
merged change.

The scale test runs once, last, because it leaves 300 environments in the
stack. With them, each agentgateway config load takes about half a second,
longer than the other tests wait for a new environment, so tests run after
it can get 404.

Then check that Sekisho stops cleanly on SIGTERM:

```bash
docker compose -f e2e/docker-compose.yml stop -t 10 sekisho
docker compose -f e2e/docker-compose.yml ps -a sekisho --format '{{.Status}}'
docker compose -f e2e/docker-compose.yml logs sekisho | grep 'shutting down'
```

Expected: the status is `Exited (0)` within a second or two, not
`Exited (137)` after 10 seconds (killed), and the log has a line with
`shutting down`.

Tear down, removing the volumes:

```bash
docker compose -f e2e/docker-compose.yml down -v
```

## Results

Linux is proven when every check passes.

| Check | Step | Passed? | Notes |
|---|---|---|---|
| Build, `cargo fmt` and `cargo clippy` clean | 1 | | |
| Release binaries build; `sekisho --help` lists four commands | 1 | | |
| Workspace tests: all passed, 0 failed | 2 | | |
| Real symlinks refused, no skipped case | 3 | | |
| Symlink and submodule from Git refused | 3 | | |
| Skill's executable bit kept | 3 | | |
| Gateway agent's config file is `0600` | 3 | | |
| Git hardened and pinned | 3 | | |
| Git version check | 3 | | |
| Release image builds; `--version` is `sekisho 0.1.0` | 4 | | |
| e2e image: git 2.37 or later, uid 65532 | 4 | | |
| e2e run 1: all passed, 0 failed | 5 | | |
| e2e run 2: all passed, 0 failed | 5 | | |
| e2e scale test passed | 5 | | |
| SIGTERM: `Exited (0)` and `shutting down` logged | 5 | | |

## Troubleshooting

| Symptom | Likely cause | What to do |
|---|---|---|
| `start postgres:18 (is Docker running?)` in many database tests | Docker isn't running, or your user can't reach its socket | `docker run --rm hello-world` must work without `sudo`; add yourself to the `docker` group and log in again. Rootless Docker: export `DOCKER_HOST` to its socket |
| Database tests time out on the first run | `postgres:18` is still downloading | `docker pull postgres:18`, then run again |
| Catalog sync tests fail with `git 2.37 or later is needed` | The host's git is older (Ubuntu 22.04 has 2.34) | Install a newer git (on Ubuntu, the `git-core` PPA), then run again |
| Catalog sync tests can't run `git http-backend` | git installed without its helpers | Install the distribution's full `git` package |
| A gateway agent timing test fails once (`… not seen within 2s`) | A loaded machine; its windows are 2 to 3 seconds | Run `cargo test -p sekisho-gateway --test agent` alone; it is a bug only if it fails there too |
| `docker compose up --wait` fails on a port | Something else holds 18080, 3000, 5556 or 18000 | Stop it, or run on another machine |
| End-to-end tests refuse every environment save with 409 "kept in Git" | A stopped Git mode test left Git mode on | `docker compose -f e2e/docker-compose.yml down -v`, then `up` again |
| Every end-to-end test fails at once with `Connection refused` on 127.0.0.1:18080 | The stack isn't up | Run the `up -d --wait` line first and check `ps` |
| End-to-end tests get 404 where they expect 200, soon after creating an environment | The scale test ran earlier on this stack, and its 300 environments slow every config load | `down -v`, `up` again, and run the scale test only last |
| Docker problems on WSL 2 (no `/var/run/docker.sock`, `exec format error`) | Docker Desktop's setup for WSL | See [Testing on WSL 2](testing-on-wsl.md#troubleshooting) |
| Builds or containers get killed | Out of memory | Use 8 GB or more; build with `cargo build -j 2`; don't run two stacks at once |

If something fails that isn't here, keep its log (`workspace-tests.log`,
`e2e-1.log`, `e2e-2.log`, `e2e-scale.log`) and note the step in the
results table.

# Testing on WSL 2

On a Windows machine, WSL 2 runs the [Linux checks](testing-on-linux.md)
in a real Linux distribution, with Docker Desktop as the Docker engine.
This page sets that up. Once it's done, `make check` or the
[by-hand steps](testing-on-linux-by-hand.md) run as on any Linux machine.

What's different from a plain Linux machine:

- Docker comes from Docker Desktop on Windows, not from Docker Engine
  installed in the distribution. Don't install both: they compete for
  `/var/run/docker.sock`.
- The distribution and Docker Desktop's containers share one virtual
  machine, and so share its memory.
- The code must live on the distribution's own disk, not under `/mnt/c`
  ([why](#5-clone-onto-the-distributions-own-disk)).

These steps were checked on Windows 11 with Ubuntu 24.04 and Docker
Desktop on the WSL 2 backend.

## 1. Ubuntu in WSL 2

In PowerShell:

```powershell
wsl --install -d Ubuntu-24.04
wsl -l -v
```

`wsl -l -v` must show the distribution with `VERSION` 2. Create its user
when it first starts.

## 2. Memory for the virtual machine

WSL 2 gives its virtual machine half of Windows' memory by default. The
checks need 8 GB or more for the distribution and Docker Desktop together:
the workspace tests start a PostgreSQL container per test, and the
end-to-end stack runs seven containers. Set it in `%UserProfile%\.wslconfig`:

```ini
[wsl2]
memory=8GB
```

Then restart WSL from PowerShell. This also stops Docker Desktop; start it
again after:

```powershell
wsl --shutdown
```

## 3. Docker Desktop, connected to the distribution

1. Install [Docker Desktop](https://docs.docker.com/desktop/setup/install/windows-install/)
   with the WSL 2 backend (its default) and start it.
2. In Docker Desktop, open Settings → Resources → WSL integration (in the
   newer settings layout, **WSL integration** in the side list), turn on
   your distribution, and apply.
3. In a **new** terminal of the distribution, check that the socket is
   there and that you can use it without `sudo`:

   ```bash
   ls -la /var/run/docker.sock
   docker run --rm hello-world
   ```

   If `docker` says permission denied, add yourself to the `docker` group
   (`sudo usermod -aG docker "$USER"`) and open a new terminal.

Containers' published ports, such as the end-to-end stack's 18080, are
reachable at `127.0.0.1` from inside the distribution.

### Windows' credential helper

Docker Desktop may write `~/.docker/config.json` in the distribution with
`"credsStore": "desktop.exe"`, a Windows program. With `systemd=true` in
`/etc/wsl.conf` (Ubuntu's default), the distribution can lose the ability
to run Windows programs (`/proc/sys/fs/binfmt_misc/` then has no
`WSLInterop`), and every image build fails before it starts:

```text
error getting credentials - err: fork/exec /usr/bin/docker-credential-desktop.exe: exec format error
```

The builds pull only public images, so they need no credentials. Remove
`credsStore` from the file; if it holds nothing else:

```bash
cp ~/.docker/config.json ~/.docker/config.json.bak
echo '{}' > ~/.docker/config.json
```

## 4. Prerequisites in the distribution

The same as on Linux, except Docker:

```bash
sudo apt-get update
sudo apt-get install -y build-essential git curl ca-certificates pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
. "$HOME/.cargo/env"
git --version   # 2.37 or later; Ubuntu 24.04 has 2.43
```

## 5. Clone onto the distribution's own disk

```bash
cd ~
git clone https://github.com/arindamchoudhury/sekisho.git
cd sekisho
```

Not under `/mnt/c`, even to share a checkout with Windows. There the files
are on Windows' disk: modes and executable bits don't behave as on Linux,
symlinks may not be real ones, and every file access crosses into
Windows, so builds are several times slower. The [Linux-only
checks](testing-on-linux.md#what-make-unix-checks) test exactly those
modes, bits and symlinks, so they would prove nothing there.

## 6. Run the checks

```bash
make check
```

or follow [Testing on Linux by hand](testing-on-linux-by-hand.md) from
Step 1. Don't run the workspace tests and the end-to-end stack at the same
time: they share the virtual machine's memory.

Windows can read the logs at
`\\wsl$\Ubuntu-24.04\home\<user>\sekisho\target\check\` (or the
by-hand steps' `*.log` files in `\\wsl$\Ubuntu-24.04\home\<user>\sekisho\`).

## Troubleshooting

| Symptom | Likely cause | What to do |
|---|---|---|
| `/var/run/docker.sock: no such file or directory` | WSL integration is off for the distribution, or Docker Desktop isn't running | Start Docker Desktop and turn the distribution on under WSL integration; open a new terminal |
| `docker` commands hang, in Windows and the distribution alike | Docker Desktop's engine is stuck | Quit Docker Desktop; if it won't quit, run `wsl --terminate docker-desktop` in PowerShell; then start it again |
| `docker-credential-desktop.exe: exec format error` in a build | Windows' credential helper in `~/.docker/config.json` | Remove `credsStore`, as [above](#windows-credential-helper) |
| `docker` fails with `.../docker.exe: exec format error` | WSL integration is off, so `docker` is found on Windows' path, and the distribution can't run Windows programs | Turn on WSL integration, so `docker` is Docker Desktop's Linux client |
| An image build can't download crates: certificate errors | Antivirus or a proxy on Windows inspects TLS, and the build's containers don't trust its certificate | Exclude crates.io and static.crates.io from the inspection, or build on a machine without it |
| Builds or containers get killed | The virtual machine is out of memory | Raise `memory` in `.wslconfig`; build with `CARGO_BUILD_JOBS=2 make check`; run one stack at a time |
| Builds are very slow | The checkout is under `/mnt/c` | Clone onto the distribution's own disk |

For failures in the checks themselves, see [Testing on
Linux](testing-on-linux.md#troubleshooting).

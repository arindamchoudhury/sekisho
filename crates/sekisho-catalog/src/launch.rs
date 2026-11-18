//! How a plugin starts an MCP server, read from its `command` and `args`,
//! so the exact-version rule (D102–D103) reaches plugins' servers too.
//! Package runners (`npx`, `uvx`, and since D188 `bunx`, `pnpm dlx`,
//! `yarn dlx`, `npm exec`, `uv tool run`, `pipx run`) and `docker run`
//! must name one exact package or image; any option that could change what
//! is installed, or how a container runs, is refused, and so is a shell
//! given a command line to run.

use serde::{Deserialize, Serialize};

use crate::pins::{ImageRef, NpmPackage, PythonPackage};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Launch {
    Npx(NpmPackage),
    Uvx(PythonPackage),
    /// `bunx`, `pnpm dlx`, `yarn dlx` or `npm exec` (`runner`): one exact
    /// npm package (D188).
    NpmRunner {
        runner: String,
        package: NpmPackage,
    },
    /// `uv tool run` or `pipx run` (`runner`): one exact Python package
    /// (D188).
    PythonRunner {
        runner: String,
        package: PythonPackage,
    },
    /// The tag is kept; a sync resolves it to a digest.
    Docker {
        image: ImageRef,
    },
    Remote {
        url: String,
    },
    /// Anything else, such as `node ${CLAUDE_PLUGIN_ROOT}/server.js`: the
    /// plugin's own files.
    Other {
        command: String,
    },
}

/// Reads a stdio server's launch.
pub fn parse_stdio(command: &str, args: &[String]) -> Result<Launch, String> {
    let program = launcher(command);
    if runs_a_command_line(&program, args) {
        return Err(format!(
            "{program} runs a shell command line, which Sekisho can't check; start the server \
             through a package runner with an exact version, or a file of the plugin"
        ));
    }
    let arg = |i: usize| args.get(i).map(String::as_str);
    match (program.as_str(), arg(0), arg(1)) {
        ("npx", ..) => npm_package("npx", args, NPX_SWITCHES).map(Launch::Npx),
        ("uvx", ..) => python_package("uvx", args).map(Launch::Uvx),
        ("docker", ..) => docker(args),
        ("bunx", ..) => npm_runner("bunx", args, &["--bun", "-y", "--yes"]),
        ("pnpm", Some("dlx"), _) => npm_runner("pnpm dlx", &args[1..], &["-s", "--silent"]),
        ("yarn", Some("dlx"), _) => npm_runner("yarn dlx", &args[1..], &["-q", "--quiet"]),
        ("npm", Some("exec" | "x"), _) => npm_runner("npm exec", &args[1..], NPX_SWITCHES),
        ("uv", Some("tool"), Some("run")) => Ok(Launch::PythonRunner {
            runner: "uv tool run".to_owned(),
            package: python_package("uv tool run", &args[2..])?,
        }),
        ("pipx", Some("run"), _) => Ok(Launch::PythonRunner {
            runner: "pipx run".to_owned(),
            package: pipx_package(&args[1..])?,
        }),
        (other, ..) => Ok(Launch::Other {
            command: other.to_owned(),
        }),
    }
}

/// Shells, by file name.
const SHELLS: [&str; 10] = [
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "ash",
    "fish",
    "cmd",
    "powershell",
    "pwsh",
];

/// Whether the program is a shell given a command line to run (`sh -c`,
/// `bash -lc`, `cmd /c`, `powershell -Command`), rather than a script.
fn runs_a_command_line(program: &str, args: &[String]) -> bool {
    if !SHELLS.contains(&program) {
        return false;
    }
    args.iter().any(|arg| match program {
        "cmd" => arg.eq_ignore_ascii_case("/c") || arg.eq_ignore_ascii_case("/k"),
        "powershell" | "pwsh" => {
            let a = arg.to_ascii_lowercase();
            ["-c", "-cwa", "-e", "-ec", "-enc"].contains(&a.as_str())
                || (a.len() > 2 && "-command".starts_with(&a))
                || a.starts_with("-commandwithargs")
                || (a.len() > 3 && "-encodedcommand".starts_with(&a))
        }
        // A cluster of short options holding `c`, such as -c or -lc.
        _ => {
            arg.starts_with('-')
                && !arg.starts_with("--")
                && arg[1..].chars().all(|c| c.is_ascii_alphabetic())
                && arg.contains('c')
        }
    })
}

/// `npx` options that change nothing about what is installed.
const NPX_SWITCHES: &[&str] = &["-y", "--yes", "--no", "-q", "--quiet"];

fn npm_runner(runner: &str, args: &[String], switches: &[&str]) -> Result<Launch, String> {
    Ok(Launch::NpmRunner {
        runner: runner.to_owned(),
        package: npm_package(runner, args, switches)?,
    })
}

/// The one exact npm package a runner's arguments name: the first
/// argument that isn't an option, or `-p`/`--package`'s value. Any option
/// but `switches` is refused, as it may change what is installed.
fn npm_package(runner: &str, args: &[String], switches: &[&str]) -> Result<NpmPackage, String> {
    let mut package: Option<&str> = None;
    let mut it = args.iter();
    let mut first: Option<&str> = None;
    while let Some(arg) = it.next() {
        let (flag, inline) = split_long(arg);
        match flag {
            // Every later argument is the command and its arguments.
            "--" => {
                first = it.next().map(String::as_str);
                break;
            }
            f if switches.contains(&f) => {}
            "-p" | "--package" => {
                if package.is_some() {
                    return Err(format!(
                        "{runner} names more than one package; name one exact version"
                    ));
                }
                package = Some(value_of(flag, inline, &mut it)?);
            }
            f if f.starts_with('-') => {
                return Err(format!(
                    "{runner} option {f} isn't allowed; it may change what is installed"
                ));
            }
            other => {
                first = Some(other);
                break;
            }
        }
    }
    match package.or(first) {
        Some(p) => NpmPackage::parse_exact(p).map_err(|e| e.to_string()),
        None => Err(format!("{runner} names no package")),
    }
}

/// The one exact Python package `uvx`'s (or `uv tool run`'s) arguments
/// name: `--from`'s value, or the tool.
fn python_package(runner: &str, args: &[String]) -> Result<PythonPackage, String> {
    let mut from: Option<&str> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = split_long(arg);
        match flag {
            "-q" | "--quiet" => {}
            "--from" => from = Some(value_of(flag, inline, &mut it)?),
            "--with" | "--with-requirements" | "--with-editable" => {
                return Err(format!(
                    "{runner} {flag} adds packages without an exact version; it isn't allowed"
                ));
            }
            f if f.starts_with('-') => {
                return Err(format!(
                    "{runner} option {f} isn't allowed; it may change what is installed"
                ));
            }
            first => {
                let package = from.unwrap_or(first);
                return PythonPackage::parse_exact(package).map_err(|e| e.to_string());
            }
        }
    }
    Err(format!("{runner} names no tool"))
}

/// The one exact Python package `pipx run`'s arguments name: `--spec`'s
/// value, or the app.
fn pipx_package(args: &[String]) -> Result<PythonPackage, String> {
    const RUNNER: &str = "pipx run";
    let mut spec: Option<&str> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = split_long(arg);
        match flag {
            "-q" | "--quiet" | "-v" | "--verbose" => {}
            "--spec" => spec = Some(value_of(flag, inline, &mut it)?),
            f if f.starts_with('-') => {
                return Err(format!(
                    "{RUNNER} option {f} isn't allowed; it may change what is installed"
                ));
            }
            app => {
                return PythonPackage::parse_exact(spec.unwrap_or(app)).map_err(|e| e.to_string());
            }
        }
    }
    Err(format!("{RUNNER} names no app"))
}

/// The program a command runs, as its file name: `/usr/bin/npx`,
/// `npx.cmd` and `NPX.EXE` are all `npx`.
fn launcher(command: &str) -> String {
    let name = command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command)
        .to_ascii_lowercase();
    for suffix in [".cmd", ".exe", ".bat"] {
        if let Some(stem) = name.strip_suffix(suffix) {
            return stem.to_owned();
        }
    }
    name
}

/// Environment variables that change which package a package manager
/// installs, or what a runtime loads: a catalog can't fix their values,
/// or an exact version (D171) would name whatever another registry serves.
/// Matched case-insensitively.
pub fn steers_installs(key: &str) -> bool {
    let key = key.to_ascii_uppercase().replace('-', "_");
    const PREFIXES: [&str; 9] = [
        "NPM_CONFIG_",
        "YARN_",
        "PNPM_",
        "BUN_",
        "UV_",
        "PIP_",
        "PYTHON",
        "LD_",
        "DYLD_",
    ];
    const NAMES: [&str; 4] = [
        "NODE_OPTIONS",
        "NODE_PATH",
        "COREPACK_NPM_REGISTRY",
        "NPM_TOKEN",
    ];
    PREFIXES.iter().any(|p| key.starts_with(p)) || NAMES.contains(&key.as_str())
}

/// `--flag=value` split, or the flag alone.
fn split_long(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((flag, value)) if arg.starts_with("--") => (flag, Some(value)),
        _ => (arg, None),
    }
}

/// The value of a flag: after `=`, or the next argument.
fn value_of<'a>(
    flag: &str,
    inline: Option<&'a str>,
    rest: &mut impl Iterator<Item = &'a String>,
) -> Result<&'a str, String> {
    match inline {
        Some(v) => Ok(v),
        None => rest
            .next()
            .map(String::as_str)
            .ok_or_else(|| format!("{flag} needs a value")),
    }
}

/// `docker run` options that take a value.
const DOCKER_VALUE_FLAGS: [&str; 18] = [
    "-e",
    "--env",
    "--env-file",
    "-v",
    "--volume",
    "--mount",
    "--name",
    "--network",
    "-p",
    "--publish",
    "-w",
    "--workdir",
    "--entrypoint",
    "-u",
    "--user",
    "--platform",
    "-l",
    "--label",
];
/// `docker run` options without a value.
const DOCKER_SWITCHES: [&str; 6] = ["-i", "-t", "-it", "-ti", "--rm", "-d"];

fn docker(args: &[String]) -> Result<Launch, String> {
    let mut it = args.iter();
    if it.next().map(String::as_str) != Some("run") {
        return Err("only docker run is allowed for a container server".to_owned());
    }
    while let Some(arg) = it.next() {
        let (flag, inline) = split_long(arg);
        if DOCKER_SWITCHES.contains(&flag) {
            continue;
        }
        if DOCKER_VALUE_FLAGS.contains(&flag) {
            value_of(flag, inline, &mut it)?;
            continue;
        }
        if flag.starts_with('-') {
            return Err(format!(
                "docker run option {flag} isn't allowed; it may change how the container runs"
            ));
        }
        return ImageRef::parse(arg)
            .map(|image| Launch::Docker { image })
            .map_err(|e| e.to_string());
    }
    Err("docker run names no image".to_owned())
}

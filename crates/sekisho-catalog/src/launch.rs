//! How a plugin starts an MCP server, read from its `command` and `args`,
//! so the exact-version rule (D171) reaches plugins' servers too. `npx`,
//! `uvx` and `docker run` must name one exact package or image; any
//! option that could change what is installed, or how a container runs,
//! is refused.

use serde::{Deserialize, Serialize};

use crate::pins::{ImageRef, NpmPackage, PythonPackage};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Launch {
    Npx(NpmPackage),
    Uvx(PythonPackage),
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
    match command {
        "npx" => npx(args),
        "uvx" => uvx(args),
        "docker" => docker(args),
        other => Ok(Launch::Other {
            command: other.to_owned(),
        }),
    }
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

fn npx(args: &[String]) -> Result<Launch, String> {
    let mut package: Option<&str> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = split_long(arg);
        match flag {
            "-y" | "--yes" | "-q" | "--quiet" => {}
            "-p" | "--package" => {
                if package.is_some() {
                    return Err(
                        "npx names more than one package; name one exact version".to_owned()
                    );
                }
                package = Some(value_of(flag, inline, &mut it)?);
            }
            f if f.starts_with('-') => {
                return Err(format!(
                    "npx option {f} isn't allowed; it may change what is installed"
                ));
            }
            // The first other argument is the package, or with -p the command.
            first => {
                let package = package.unwrap_or(first);
                return NpmPackage::parse_exact(package)
                    .map(Launch::Npx)
                    .map_err(|e| e.to_string());
            }
        }
    }
    match package {
        Some(p) => NpmPackage::parse_exact(p)
            .map(Launch::Npx)
            .map_err(|e| e.to_string()),
        None => Err("npx names no package".to_owned()),
    }
}

fn uvx(args: &[String]) -> Result<Launch, String> {
    let mut from: Option<&str> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = split_long(arg);
        match flag {
            "-q" | "--quiet" => {}
            "--from" => from = Some(value_of(flag, inline, &mut it)?),
            "--with" | "--with-requirements" | "--with-editable" => {
                return Err(format!(
                    "uvx {flag} adds packages without an exact version; it isn't allowed"
                ));
            }
            f if f.starts_with('-') => {
                return Err(format!(
                    "uvx option {f} isn't allowed; it may change what is installed"
                ));
            }
            first => {
                let package = from.unwrap_or(first);
                return PythonPackage::parse_exact(package)
                    .map(Launch::Uvx)
                    .map_err(|e| e.to_string());
            }
        }
    }
    Err("uvx names no tool".to_owned())
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

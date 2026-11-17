//! An APM plugin bundle checked as `apm install` checks it (D169; APM's
//! `verify_bundle_integrity` in `src/apm_cli/bundle/local_bundle.py`,
//! unchanged from `f574a62c` to `2769573f`): every file listed in
//! `apm.lock.yaml`'s `pack.bundle_files` is present with its SHA-256, and
//! no other file is, but `apm.lock.yaml` and `plugin.json`. Sekisho checks
//! the files it has already read, so a key never touches the disk.

use std::collections::BTreeMap;

use serde_yaml_ng::Value;

use crate::model::sha256_hex;
use crate::tree::TreeFile;

const LOCK_FILE: &str = "apm.lock.yaml";
/// Files a bundle may hold without listing them, as in APM.
const ALLOWED_EXTRAS: [&str; 2] = [LOCK_FILE, "plugin.json"];

/// `Ok(true)` when the bundle matches its manifest, `Ok(false)` when there
/// is no lock file or it has no `pack.bundle_files`, and every problem
/// otherwise.
pub fn verify_bundle(files: &[TreeFile]) -> Result<bool, Vec<String>> {
    let Some(lock) = files.iter().find(|f| f.path == LOCK_FILE) else {
        return Ok(false);
    };
    let Ok(text) = std::str::from_utf8(&lock.bytes) else {
        return Err(vec![format!("{LOCK_FILE} isn't UTF-8 text")]);
    };
    let doc: Value = serde_yaml_ng::from_str(text)
        .map_err(|e| vec![format!("{LOCK_FILE} isn't valid YAML: {e}")])?;
    let manifest = match doc.get("pack").and_then(|p| p.get("bundle_files")) {
        None | Some(Value::Null) => return Ok(false),
        Some(Value::Mapping(m)) if m.is_empty() => return Ok(false),
        Some(Value::Mapping(m)) => m,
        Some(_) => return Err(vec!["pack.bundle_files isn't a mapping".to_owned()]),
    };

    let by_path: BTreeMap<&str, &TreeFile> = files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut errors = Vec::new();
    let mut listed = Vec::new();
    for (key, expected) in manifest {
        let Some(key) = key.as_str() else {
            errors.push("pack.bundle_files has a key that isn't text".to_owned());
            continue;
        };
        if let Err(why) = safe_key(key) {
            errors.push(format!("unsafe bundle_files key {key:?}: {why}"));
            continue;
        }
        listed.push(key);
        let Some(file) = by_path.get(key) else {
            errors.push(format!("missing bundle file {key}"));
            continue;
        };
        let expected = match expected.as_str().map(normalize_hash) {
            Some(Ok(hash)) => hash,
            Some(Err(why)) => {
                errors.push(format!("invalid hash for {key}: {why}"));
                continue;
            }
            None => {
                errors.push(format!("invalid hash for {key}: not text"));
                continue;
            }
        };
        if sha256_hex(&file.bytes) != expected {
            errors.push(format!("hash mismatch for {key}"));
        }
    }
    for file in files {
        let path = file.path.as_str();
        if !ALLOWED_EXTRAS.contains(&path) && !listed.contains(&path) {
            errors.push(format!(
                "unlisted bundle file (not in pack.bundle_files): {path}"
            ));
        }
    }
    if errors.is_empty() {
        Ok(true)
    } else {
        Err(errors)
    }
}

/// APM's `_normalize_hash`: an optional `sha256:` prefix, any case; any
/// other algorithm is refused rather than never checked.
fn normalize_hash(value: &str) -> Result<String, String> {
    let hex = match value.strip_prefix("sha256:") {
        Some(rest) => rest,
        None if value.contains(':') => {
            return Err(format!("unsupported hash algorithm in {value:?}"));
        }
        None => value,
    };
    Ok(hex.trim().to_lowercase())
}

/// One percent-decoding step; `None` when nothing changes.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut changed = false;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            changed = true;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    changed.then(|| String::from_utf8_lossy(&out).into_owned())
}

/// A key names a file inside the bundle: relative, `/`-separated, no
/// empty, `.` or `..` segment even once percent-decoded (as APM's
/// `validate_path_segments` decodes), and no backslash.
fn safe_key(key: &str) -> Result<(), &'static str> {
    if key.contains('\\') {
        return Err("it holds a backslash");
    }
    for segment in key.split('/') {
        let mut decoded = segment.to_owned();
        for _ in 0..8 {
            match percent_decode(&decoded) {
                Some(next) => decoded = next,
                None => break,
            }
        }
        if decoded.is_empty() {
            return Err("it is absolute or has an empty segment");
        }
        if decoded == "." || decoded == ".." || decoded.contains(['/', '\\']) {
            return Err("it leaves the bundle or names its folder");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_checked_after_percent_decoding() {
        assert!(safe_key("skills/notes/SKILL.md").is_ok());
        assert!(safe_key("a%20b.md").is_ok());
        for bad in [
            "../x",
            "%2e%2e/x",
            "%252E%252E/x",
            "a/%2F/b",
            "/x",
            "a//b",
            "a/.",
            "a%5cb",
        ] {
            assert!(safe_key(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hashes_normalise_as_apm_does() {
        assert_eq!(normalize_hash("sha256:ABC").unwrap(), "abc");
        assert_eq!(normalize_hash(" abc ").unwrap(), "abc");
        assert!(normalize_hash("md5:abc").is_err());
    }
}

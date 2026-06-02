#![forbid(unsafe_code)]
#![warn(clippy::pedantic, clippy::nursery)]
#![deny(missing_docs, missing_debug_implementations)]

//! # attractor-config
//!
//! One shared place to read configuration and secrets, so no Attractor crate
//! hardcodes a path, port, endpoint, or token. Three jobs, all **fail-loud**
//! (no silent defaults — RULES.md NO FALLBACKS):
//!
//! - [`load`] — deserialize a named section of `config.yaml` into the caller's
//!   own typed struct. The crate owns the *mechanism*; each crate owns its
//!   *schema* (so config stays modular, not a god-struct).
//! - [`load_dotenv`] + [`secret`] — read required secrets from `.env` (or the
//!   process environment), never from a tracked file.
//! - [`resolve_path`] — turn a config-relative path into an absolute one
//!   against a known base, so configs carry **relative** paths, not machine-
//!   specific absolutes.
//!
//! Pure + portable (serde + serde_yaml only, no OS/async deps) so it attests on
//! both 1.92 cross-platform legs.

use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Why a config/secret load failed. Every variant is a hard stop — callers
/// must not paper over a missing value with a default.
#[derive(Debug)]
pub enum ConfigError {
    /// Reading a file failed.
    Io(std::io::Error),
    /// Parsing YAML failed.
    Yaml(serde_yaml::Error),
    /// The requested top-level section is absent from `config.yaml`.
    MissingSection(String),
    /// A required secret was in neither `.env` nor the process environment.
    MissingSecret(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "config io error: {e}"),
            Self::Yaml(e) => write!(f, "config yaml error: {e}"),
            Self::MissingSection(s) => write!(f, "config.yaml has no section '{s}'"),
            Self::MissingSecret(k) => {
                write!(f, "required secret '{k}' not set (checked .env and environment)")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Yaml(e) => Some(e),
            Self::MissingSection(_) | Self::MissingSecret(_) => None,
        }
    }
}

/// Parsed `.env` contents (`KEY` → value). Read via [`secret`]; never holds a
/// reference into a tracked file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dotenv(BTreeMap<String, String>);

/// Deserialize the `section` top-level key of the YAML at `config_path` into
/// the caller's type `T`.
///
/// The caller defines `T` (its own config schema); this owns only the loading.
///
/// # Errors
/// [`ConfigError::Io`] if the file can't be read, [`ConfigError::Yaml`] on parse
/// or shape mismatch, [`ConfigError::MissingSection`] if `section` is absent.
pub fn load<T: DeserializeOwned>(config_path: &Path, section: &str) -> Result<T, ConfigError> {
    let text = std::fs::read_to_string(config_path).map_err(ConfigError::Io)?;
    let root: serde_yaml::Value = serde_yaml::from_str(&text).map_err(ConfigError::Yaml)?;
    let sect = root
        .get(section)
        .ok_or_else(|| ConfigError::MissingSection(section.to_owned()))?;
    serde_yaml::from_value(sect.clone()).map_err(ConfigError::Yaml)
}

/// Parse a `.env` file in `dir` into a [`Dotenv`]. A missing `.env` is **not**
/// an error (secrets may come from the process environment instead) — only a
/// read/parse failure is. Lines are `KEY=VALUE`; blanks and `#` comments are
/// skipped; surrounding whitespace and a single layer of matching quotes are
/// trimmed from the value.
///
/// # Errors
/// [`ConfigError::Io`] if a present `.env` can't be read.
pub fn load_dotenv(dir: &Path) -> Result<Dotenv, ConfigError> {
    let path = dir.join(".env");
    if !path.exists() {
        return Ok(Dotenv::default());
    }
    let text = std::fs::read_to_string(&path).map_err(ConfigError::Io)?;
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.trim().to_owned(), unquote(value.trim()).to_owned());
        }
    }
    Ok(Dotenv(map))
}

/// Strip one matching layer of single or double quotes, if present.
fn unquote(s: &str) -> &str {
    let bytes = s.as_bytes();
    if s.len() >= 2
        && ((bytes[0] == b'"' && bytes[s.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[s.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Read a required secret: `.env` first, then the process environment.
///
/// # Errors
/// [`ConfigError::MissingSecret`] if `key` is in neither — fail-loud, never a
/// default. Secrets live in `.env`/env, never a tracked file.
pub fn secret(env: &Dotenv, key: &str) -> Result<String, ConfigError> {
    if let Some(v) = env.0.get(key) {
        return Ok(v.clone());
    }
    std::env::var(key).map_err(|_| ConfigError::MissingSecret(key.to_owned()))
}

/// Resolve a config value that is a path: returned as-is if already absolute,
/// otherwise joined onto `base` (typically the config file's directory). Keeps
/// machine-specific absolutes out of `config.yaml`.
#[must_use]
pub fn resolve_path(base: &Path, value: &str) -> PathBuf {
    let p = Path::new(value);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::{load, load_dotenv, resolve_path, secret, ConfigError};
    use serde::Deserialize;
    use std::path::{Path, PathBuf};

    #[derive(Debug, Deserialize, PartialEq)]
    struct ApiCfg {
        port: u16,
        host: String,
    }

    fn tmp() -> PathBuf {
        // Unique-ish per test invocation without a tempfile dep.
        std::env::temp_dir().join(format!("attractor-config-test-{}", std::process::id()))
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn loads_named_section_into_typed_struct() {
        let dir = tmp().join("load");
        let cfg = write(&dir, "config.yaml", "api:\n  port: 8080\n  host: localhost\nother: 1\n");
        let got: ApiCfg = load(&cfg, "api").unwrap();
        assert_eq!(got, ApiCfg { port: 8080, host: "localhost".to_owned() });
    }

    #[test]
    fn missing_section_fails_loud() {
        let dir = tmp().join("missing");
        let cfg = write(&dir, "config.yaml", "api:\n  port: 1\n  host: h\n");
        assert!(matches!(
            load::<ApiCfg>(&cfg, "nope"),
            Err(ConfigError::MissingSection(_))
        ));
    }

    #[test]
    fn dotenv_parsed_and_secret_read() {
        let dir = tmp().join("env");
        write(&dir, ".env", "# comment\nATTRACTOR_TOKEN=\"s3cr3t\"\n\nEMPTYLINE_OK=1\n");
        let env = load_dotenv(&dir).unwrap();
        assert_eq!(secret(&env, "ATTRACTOR_TOKEN").unwrap(), "s3cr3t");
        assert_eq!(secret(&env, "EMPTYLINE_OK").unwrap(), "1");
    }

    #[test]
    fn missing_secret_fails_loud() {
        let env = load_dotenv(&tmp().join("nope-dir")).unwrap(); // no .env → empty
        assert!(matches!(
            secret(&env, "DEFINITELY_UNSET_KEY_XYZ"),
            Err(ConfigError::MissingSecret(_))
        ));
    }

    #[test]
    fn relative_path_resolves_against_base() {
        // Platform-agnostic: compare against join(), not a hardcoded separator.
        let base = Path::new("base_dir");
        assert_eq!(resolve_path(base, "data/shm"), base.join("data/shm"));
    }

    #[test]
    fn absolute_path_passes_through() {
        // current_dir() is absolute on every platform (unlike a hardcoded
        // "/abs", which isn't absolute on Windows).
        let abs = std::env::current_dir().unwrap();
        assert_eq!(resolve_path(Path::new("ignored"), abs.to_str().unwrap()), abs);
    }
}

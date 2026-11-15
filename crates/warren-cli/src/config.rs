//! The config file written by `warren login` and `warren signup`.
//!
//! `{ "api_url": "...", "api_key": "sk_live_..." }` at
//! `$XDG_CONFIG_HOME/warren/config.json`, or `~/.config/warren/config.json`
//! when `XDG_CONFIG_HOME` is unset.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use warren_core::{Error, Result};

use crate::cli::DEFAULT_API_URL;

/// Contents of the config file. Missing fields are `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileConfig {
    pub api_url: Option<String>,
    pub api_key: Option<String>,
}

impl FileConfig {
    /// The default location, from the process environment. `None` when neither
    /// `XDG_CONFIG_HOME` nor `HOME` is set.
    pub fn default_path() -> Option<PathBuf> {
        Self::path_from_env(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
    }

    /// The location rule with the environment passed in, so tests can cover it:
    /// `$XDG_CONFIG_HOME/warren/config.json`, else `$HOME/.config/warren/config.json`.
    pub fn path_from_env(
        xdg_config_home: Option<OsString>,
        home: Option<OsString>,
    ) -> Option<PathBuf> {
        let non_empty = |v: Option<OsString>| v.filter(|s| !s.is_empty());
        let base = match non_empty(xdg_config_home) {
            Some(xdg) => PathBuf::from(xdg),
            None => PathBuf::from(non_empty(home)?).join(".config"),
        };
        Some(base.join("warren").join("config.json"))
    }

    /// Read the file; a missing file is an empty config, a malformed one is
    /// `invalid_request`.
    pub fn load(path: &Path) -> Result<FileConfig> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let v: Value = serde_json::from_slice(&bytes).map_err(|e| {
                    Error::invalid(format!("{}: not valid JSON: {e}", path.display()))
                })?;
                Self::from_json(&v)
                    .map_err(|e| Error::invalid(format!("{}: {}", path.display(), e.message)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
            Err(e) => Err(Error::internal(format!("reading {}: {e}", path.display()))),
        }
    }

    /// Decode the JSON object; unknown fields are ignored, empty strings are `None`.
    pub fn from_json(v: &Value) -> Result<FileConfig> {
        let obj = v
            .as_object()
            .ok_or_else(|| Error::invalid("config must be a JSON object"))?;
        let field = |k: &str| -> Result<Option<String>> {
            match obj.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
                Some(_) => Err(Error::invalid(format!("field {k:?} must be a string"))),
            }
        };
        Ok(FileConfig {
            api_url: field("api_url")?,
            api_key: field("api_key")?,
        })
    }

    /// Encode as the JSON object stored on disk.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        if let Some(u) = &self.api_url {
            m.insert("api_url".into(), Value::String(u.clone()));
        }
        if let Some(k) = &self.api_key {
            m.insert("api_key".into(), Value::String(k.clone()));
        }
        Value::Object(m)
    }

    /// Write the file atomically with mode 0600, creating parent directories.
    pub fn save(&self, path: &Path) -> Result<()> {
        let wrap = |what: &str, e: std::io::Error| {
            Error::internal(format!("{what} {}: {e}", path.display()))
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| wrap("creating directory for", e))?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = format!("{}\n", serde_json::to_string_pretty(&self.to_json())?);
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| wrap("writing", e))?;
            f.write_all(body.as_bytes())
                .map_err(|e| wrap("writing", e))?;
            f.sync_all().map_err(|e| wrap("writing", e))?;
        }
        std::fs::rename(&tmp, path).map_err(|e| wrap("writing", e))
    }
}

/// Effective client settings after merging flags, environment, the file and defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Normalized base URL without a trailing slash or `/v1`.
    pub api_url: String,
    /// `None` when no key is configured anywhere.
    pub api_key: Option<String>,
}

/// Flags and environment (already merged by clap) win, then the file, then the
/// default URL. Blank values count as absent.
pub fn resolve(flag_url: Option<&str>, flag_key: Option<&str>, file: &FileConfig) -> Settings {
    let present = |s: Option<&str>| {
        s.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let api_url = present(flag_url)
        .or_else(|| present(file.api_url.as_deref()))
        .unwrap_or_else(|| DEFAULT_API_URL.to_string());
    let api_key = present(flag_key).or_else(|| present(file.api_key.as_deref()));
    Settings {
        api_url: normalize_api_url(&api_url),
        api_key,
    }
}

/// Trim, add `http://` when the scheme is missing, and drop trailing slashes
/// and a trailing `/v1` so both `http://host:7337` and `http://host:7337/v1/`
/// name the same daemon.
pub fn normalize_api_url(s: &str) -> String {
    let mut u = s.trim().to_string();
    if !u.contains("://") {
        u = format!("http://{u}");
    }
    loop {
        let trimmed = u.trim_end_matches('/');
        let trimmed = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
        if trimmed.len() == u.len() {
            break;
        }
        u = trimmed.to_string();
    }
    u
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "warren-cli-test-{tag}-{}-{n}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn path_rule() {
        let p = FileConfig::path_from_env(Some("/xdg".into()), Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/xdg/warren/config.json"));
        let p = FileConfig::path_from_env(None, Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.config/warren/config.json"));
        let p = FileConfig::path_from_env(Some("".into()), Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.config/warren/config.json"));
        assert!(FileConfig::path_from_env(None, None).is_none());
    }

    #[test]
    fn round_trip_through_disk() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("nested").join("warren").join("config.json");
        assert_eq!(FileConfig::load(&path).unwrap(), FileConfig::default());
        let cfg = FileConfig {
            api_url: Some("http://api.warren.localhost:7337".into()),
            api_key: Some("sk_live_abc".into()),
        };
        cfg.save(&path).unwrap();
        assert_eq!(FileConfig::load(&path).unwrap(), cfg);
        let text = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["api_key"], "sk_live_abc");
        assert_eq!(v["api_url"], "http://api.warren.localhost:7337");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // Overwrite keeps only the new values.
        let cfg2 = FileConfig {
            api_url: None,
            api_key: Some("sk_live_def".into()),
        };
        cfg2.save(&path).unwrap();
        assert_eq!(FileConfig::load(&path).unwrap(), cfg2);
        assert!(!dir.join("nested/warren/config.json.tmp").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_malformed_files() {
        let dir = temp_dir("malformed");
        let path = dir.join("config.json");
        std::fs::write(&path, "not json").unwrap();
        let e = FileConfig::load(&path).unwrap_err();
        assert_eq!(e.code, "invalid_request");
        std::fs::write(&path, r#"{"api_key": 5}"#).unwrap();
        assert!(FileConfig::load(&path).is_err());
        std::fs::write(&path, r#"{"api_key": "", "api_url": null, "extra": 1}"#).unwrap();
        assert_eq!(FileConfig::load(&path).unwrap(), FileConfig::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn precedence_and_normalization() {
        let file = FileConfig {
            api_url: Some("https://warren.example.com/v1/".into()),
            api_key: Some("sk_live_file".into()),
        };
        let s = resolve(None, None, &file);
        assert_eq!(s.api_url, "https://warren.example.com");
        assert_eq!(s.api_key.as_deref(), Some("sk_live_file"));
        let s = resolve(
            Some("api.warren.localhost:7337/v1"),
            Some("sk_live_flag"),
            &file,
        );
        assert_eq!(s.api_url, "http://api.warren.localhost:7337");
        assert_eq!(s.api_key.as_deref(), Some("sk_live_flag"));
        let s = resolve(Some("  "), Some(""), &FileConfig::default());
        assert_eq!(s.api_url, DEFAULT_API_URL);
        assert_eq!(s.api_key, None);
        assert_eq!(normalize_api_url("http://h:1/v1/v1//"), "http://h:1");
    }
}

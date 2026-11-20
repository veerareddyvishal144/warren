//! Request validation shared by create, patch and fork.

use std::collections::BTreeMap;

use warren_core::{
    Error, MAX_ENV_ENTRIES, MAX_ENV_VALUE_BYTES, MAX_IDLE_TIMEOUT_SEC, MAX_METADATA_ENTRIES,
    Result, ids,
};

/// Longest instance name, in characters.
pub const MAX_NAME_CHARS: usize = 120;
/// Longest metadata key, in bytes.
pub const MAX_METADATA_KEY_BYTES: usize = 128;
/// Longest metadata value, in bytes.
pub const MAX_METADATA_VALUE_BYTES: usize = 4096;
/// Longest backup note, in characters.
pub const MAX_NOTE_CHARS: usize = 1024;
/// Env keys the platform injects itself; a request may not set them. Other
/// `WARREN_*` keys (agent driver, agent command, model) are user-configurable.
pub const RESERVED_ENV_KEYS: &[&str] = &[
    warren_core::ENV_INSTANCE_ID,
    warren_core::ENV_INSTANCE_TOKEN,
    warren_core::ENV_WORKSPACE_ID,
    warren_core::ENV_HOME,
    warren_core::ENV_GATEWAY_PORT,
    warren_core::ENV_NETWORK,
    warren_core::ENV_BROKER,
];

/// An instance name is at most [`MAX_NAME_CHARS`] characters.
pub fn validate_name(name: &str) -> Result<()> {
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(Error::invalid(format!(
            "name must be at most {MAX_NAME_CHARS} characters"
        )));
    }
    Ok(())
}

/// Metadata holds at most [`MAX_METADATA_ENTRIES`] non-empty keys with bounded sizes.
pub fn validate_metadata(metadata: &BTreeMap<String, String>) -> Result<()> {
    if metadata.len() > MAX_METADATA_ENTRIES {
        return Err(Error::invalid(format!(
            "metadata may hold at most {MAX_METADATA_ENTRIES} entries"
        )));
    }
    for (k, v) in metadata {
        if k.is_empty() {
            return Err(Error::invalid("metadata keys must not be empty"));
        }
        if k.len() > MAX_METADATA_KEY_BYTES {
            return Err(Error::invalid(format!(
                "metadata key {k:?} is longer than {MAX_METADATA_KEY_BYTES} bytes"
            )));
        }
        if v.len() > MAX_METADATA_VALUE_BYTES {
            return Err(Error::invalid(format!(
                "metadata value for {k:?} is longer than {MAX_METADATA_VALUE_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

/// Env keys match `[A-Z_][A-Z0-9_]*`, never start with `WARREN_`, number at most
/// [`MAX_ENV_ENTRIES`] and carry values of at most [`MAX_ENV_VALUE_BYTES`].
pub fn validate_env(env: &BTreeMap<String, String>) -> Result<()> {
    if env.len() > MAX_ENV_ENTRIES {
        return Err(Error::invalid(format!(
            "env may hold at most {MAX_ENV_ENTRIES} entries"
        )));
    }
    for (k, v) in env {
        if !ids::valid_env_key(k) {
            return Err(Error::invalid(format!(
                "env key {k:?} is invalid; keys match [A-Z_][A-Z0-9_]*"
            )));
        }
        if RESERVED_ENV_KEYS.contains(&k.as_str()) {
            return Err(Error::invalid(format!(
                "env key {k:?} is set by the platform and cannot be overridden"
            )));
        }
        if v.len() > MAX_ENV_VALUE_BYTES {
            return Err(Error::invalid(format!(
                "env value for {k:?} is longer than {MAX_ENV_VALUE_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

/// The idle timeout lies within `[min, MAX_IDLE_TIMEOUT_SEC]`.
pub fn validate_idle_timeout(seconds: u32, min: u32) -> Result<()> {
    let min = min.min(MAX_IDLE_TIMEOUT_SEC);
    if seconds < min || seconds > MAX_IDLE_TIMEOUT_SEC {
        return Err(Error::invalid(format!(
            "idle_timeout_seconds must be between {min} and {MAX_IDLE_TIMEOUT_SEC}"
        )));
    }
    Ok(())
}

/// A backup note is at most [`MAX_NOTE_CHARS`] characters.
pub fn validate_note(note: &str) -> Result<()> {
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(Error::invalid(format!(
            "note must be at most {MAX_NOTE_CHARS} characters"
        )));
    }
    Ok(())
}

/// Only this deployment's region exists; an empty request means that region.
pub fn resolve_region(requested: Option<&str>, configured: &str) -> Result<String> {
    match requested.map(str::trim).filter(|r| !r.is_empty()) {
        None => Ok(configured.to_string()),
        Some(r) if r == configured => Ok(configured.to_string()),
        Some(r) => Err(Error::invalid(format!(
            "unknown region {r:?}; this deployment serves {configured:?}"
        ))),
    }
}

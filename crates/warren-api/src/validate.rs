//! Request validation that needs no I/O. Whatever depends on templates,
//! runtimes or balances is validated again by the lifecycle; the checks here
//! answer early and with precise messages.

use std::collections::BTreeMap;

use warren_core::{
    Budget, Config, CreateInstanceRequest, Error, ExecRequest, ForkRequest, InstanceType,
    MAX_ENV_ENTRIES, MAX_ENV_VALUE_BYTES, MAX_IDLE_TIMEOUT_SEC, MAX_METADATA_ENTRIES,
    PatchInstanceRequest, RegisterTemplateRequest, Result, ShapeRequest, ids,
};

/// Longest accepted name (workspace, key, instance, cron) in characters.
pub const MAX_NAME_CHARS: usize = 100;
/// Longest accepted template description in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 1000;
/// Longest accepted exec timeout.
pub const MAX_EXEC_TIMEOUT_SEC: u32 = 86_400;
/// Longest accepted exec command and user strings.
pub const MAX_COMMAND_BYTES: usize = 1 << 20;
const MAX_USER_CHARS: usize = 32;

/// A short human label: bounded length, no control characters.
pub fn name(label: &str, value: &str) -> Result<()> {
    text(label, value, MAX_NAME_CHARS)
}

fn text(label: &str, value: &str, max_chars: usize) -> Result<()> {
    if value.chars().count() > max_chars {
        return Err(Error::invalid(format!(
            "{label} must be at most {max_chars} characters"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(Error::invalid(format!(
            "{label} must not contain control characters"
        )));
    }
    Ok(())
}

/// Env maps: at most `MAX_ENV_ENTRIES`, keys `[A-Z_][A-Z0-9_]*`, values up to
/// `MAX_ENV_VALUE_BYTES`.
pub fn env(env: &BTreeMap<String, String>) -> Result<()> {
    if env.len() > MAX_ENV_ENTRIES {
        return Err(Error::invalid(format!(
            "env may have at most {MAX_ENV_ENTRIES} entries; got {}",
            env.len()
        )));
    }
    for (k, v) in env {
        if !ids::valid_env_key(k) {
            return Err(Error::invalid(format!(
                "env key {k:?} is invalid; keys must match [A-Z_][A-Z0-9_]*"
            )));
        }
        if v.len() > MAX_ENV_VALUE_BYTES {
            return Err(Error::invalid(format!(
                "env value for {k} is {} bytes; the limit is {MAX_ENV_VALUE_BYTES}",
                v.len()
            )));
        }
    }
    Ok(())
}

/// Metadata maps: at most `MAX_METADATA_ENTRIES`, non-empty keys.
pub fn metadata(md: &BTreeMap<String, String>) -> Result<()> {
    if md.len() > MAX_METADATA_ENTRIES {
        return Err(Error::invalid(format!(
            "metadata may have at most {MAX_METADATA_ENTRIES} entries; got {}",
            md.len()
        )));
    }
    if md.keys().any(|k| k.trim().is_empty()) {
        return Err(Error::invalid("metadata keys must not be empty"));
    }
    for (k, v) in md {
        text(&format!("metadata key {k:?}"), k, MAX_NAME_CHARS)?;
        if v.len() > MAX_ENV_VALUE_BYTES {
            return Err(Error::invalid(format!(
                "metadata value for {k:?} is {} bytes; the limit is {MAX_ENV_VALUE_BYTES}",
                v.len()
            )));
        }
    }
    Ok(())
}

/// `[config.min_idle_timeout_sec, MAX_IDLE_TIMEOUT_SEC]`.
pub fn idle_timeout(cfg: &Config, seconds: u32) -> Result<()> {
    let min = cfg.min_idle_timeout_sec;
    if seconds < min || seconds > MAX_IDLE_TIMEOUT_SEC {
        return Err(Error::invalid(format!(
            "idle_timeout_seconds must be between {min} and {MAX_IDLE_TIMEOUT_SEC}; got {seconds}"
        )));
    }
    Ok(())
}

/// Caps are never negative.
pub fn budget(b: &Budget) -> Result<()> {
    if b.monthly_cap_micros < 0 || b.credit_micros < 0 {
        return Err(Error::invalid("budget amounts must not be negative"));
    }
    Ok(())
}

/// "name" or "name@revision" with a well-formed name and a positive revision.
pub fn template_ref(value: &str) -> Result<()> {
    let (name, rev) = match value.split_once('@') {
        Some((n, r)) => (n, Some(r)),
        None => (value, None),
    };
    if !ids::valid_template_name(name) {
        return Err(Error::invalid(format!(
            "template {value:?} is not a valid template reference; names are 3-40 lowercase letters, digits or dashes, optionally followed by @revision"
        )));
    }
    if let Some(r) = rev
        && !r.parse::<u32>().is_ok_and(|n| n > 0)
    {
        return Err(Error::invalid(format!(
            "template revision in {value:?} must be a positive integer"
        )));
    }
    Ok(())
}

/// Requested shape fields, when present, are positive. Range checks need the
/// template and happen in `warren_core::shapes::normalize` downstream.
pub fn shape(s: &ShapeRequest) -> Result<()> {
    for (label, v) in [("cpu", s.cpu), ("memory", s.memory), ("disk", s.disk)] {
        if v == Some(0) {
            return Err(Error::invalid(format!(
                "resources.{label} must be positive"
            )));
        }
    }
    Ok(())
}

/// A resize must change something.
pub fn resize(s: &ShapeRequest) -> Result<()> {
    if s.cpu.is_none() && s.memory.is_none() && s.disk.is_none() {
        return Err(Error::invalid(
            "specify at least one of cpu, memory or disk",
        ));
    }
    shape(s)
}

/// `POST /v1/instances`.
pub fn create_instance(cfg: &Config, req: &CreateInstanceRequest) -> Result<()> {
    if let Some(t) = &req.template {
        template_ref(t)?;
    }
    if req.kind == Some(InstanceType::Performance) {
        return Err(Error::not_supported(
            "performance instances are not available in this build; use type \"default\" or \"vm\"",
        ));
    }
    if let Some(n) = &req.name {
        name("name", n)?;
    }
    if let Some(r) = &req.region
        && (r.trim().is_empty() || r.chars().any(char::is_control))
    {
        return Err(Error::invalid("region must be a non-empty label"));
    }
    if let Some(m) = &req.metadata {
        metadata(m)?;
    }
    if let Some(e) = &req.env {
        env(e)?;
    }
    if let Some(b) = &req.budget {
        budget(b)?;
    }
    if let Some(t) = req.idle_timeout_seconds {
        idle_timeout(cfg, t)?;
    }
    if let Some(s) = &req.shape {
        shape(s)?;
    }
    Ok(())
}

/// `PATCH /v1/instances/{id}`.
pub fn patch_instance(cfg: &Config, req: &PatchInstanceRequest) -> Result<()> {
    if let Some(n) = &req.name {
        name("name", n)?;
    }
    if let Some(m) = &req.metadata {
        metadata(m)?;
    }
    if let Some(e) = &req.env {
        env(e)?;
    }
    if let Some(b) = &req.budget {
        budget(b)?;
    }
    if let Some(t) = req.idle_timeout_seconds {
        idle_timeout(cfg, t)?;
    }
    Ok(())
}

/// `POST /v1/instances/{id}/fork`.
pub fn fork(cfg: &Config, req: &ForkRequest) -> Result<()> {
    if let Some(n) = &req.name {
        name("name", n)?;
    }
    if let Some(m) = &req.metadata {
        metadata(m)?;
    }
    if let Some(t) = req.idle_timeout_seconds {
        idle_timeout(cfg, t)?;
    }
    if let Some(s) = &req.shape {
        shape(s)?;
    }
    Ok(())
}

/// `POST /v1/instances/{id}/exec`.
pub fn exec(req: &ExecRequest) -> Result<()> {
    if req.command.trim().is_empty() {
        return Err(Error::invalid("command must not be empty"));
    }
    if req.command.len() > MAX_COMMAND_BYTES {
        return Err(Error::invalid(format!(
            "command is longer than {MAX_COMMAND_BYTES} bytes"
        )));
    }
    if !req.user.is_empty()
        && (req.user.chars().count() > MAX_USER_CHARS
            || req
                .user
                .chars()
                .any(|c| c.is_control() || c.is_whitespace()))
    {
        return Err(Error::invalid(
            "user must be a user name such as \"root\" or the image user (empty)",
        ));
    }
    if req.timeout_seconds > MAX_EXEC_TIMEOUT_SEC {
        return Err(Error::invalid(format!(
            "timeout_seconds must be at most {MAX_EXEC_TIMEOUT_SEC}"
        )));
    }
    if !req.workdir.is_empty() && !req.workdir.starts_with('/') {
        return Err(Error::invalid("workdir must be an absolute path"));
    }
    env(&req.env)
}

/// `POST /v1/templates`.
pub fn register_template(req: &RegisterTemplateRequest) -> Result<()> {
    if !ids::valid_template_name(&req.name) {
        return Err(Error::invalid(format!(
            "template name {:?} is invalid; use 3-40 lowercase letters, digits or dashes",
            req.name
        )));
    }
    if req.name.starts_with("warren-") {
        return Err(Error::invalid(
            "template names starting with \"warren-\" are reserved for the built-in catalog",
        ));
    }
    if req.image_ref.trim().is_empty() {
        return Err(Error::invalid("image_ref is required"));
    }
    text("image_ref", &req.image_ref, 512)?;
    if req.image_ref.chars().any(char::is_whitespace) {
        return Err(Error::invalid("image_ref must not contain whitespace"));
    }
    if !req.mount_path.is_empty() && !req.mount_path.starts_with('/') {
        return Err(Error::invalid("mount_path must be an absolute path"));
    }
    text("description", &req.description, MAX_DESCRIPTION_CHARS)?;
    env(&req.env)
}

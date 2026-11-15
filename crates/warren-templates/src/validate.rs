//! Validation of template references (`name@revision`) and of the fields of a
//! `RegisterTemplateRequest`. Pure functions, no I/O.

use std::collections::BTreeMap;

use warren_core::{
    Error, MAX_ENV_ENTRIES, MAX_ENV_VALUE_BYTES, RegisterTemplateRequest, Result, ids,
};

/// Where the persistent disk mounts when a registration does not say.
pub const DEFAULT_MOUNT_PATH: &str = "/home/agent";

/// Names with this prefix belong to the built-in catalog and cannot be registered.
pub const RESERVED_NAME_PREFIX: &str = "warren-";

/// A template reference split into its name and optional revision pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TemplateRef<'a> {
    pub name: &'a str,
    /// `Some` for `name@N`; always positive.
    pub revision: Option<u32>,
}

/// Split `name` or `name@revision`. Surrounding whitespace is ignored; the
/// revision must be a positive integer.
pub(crate) fn parse_ref(raw: &str) -> Result<TemplateRef<'_>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::invalid("template name is required"));
    }
    let Some((name, rev)) = raw.split_once('@') else {
        return Ok(TemplateRef {
            name: raw,
            revision: None,
        });
    };
    if name.is_empty() {
        return Err(Error::invalid("template name is required before '@'"));
    }
    let revision = rev.parse::<u32>().ok().filter(|r| *r > 0).ok_or_else(|| {
        Error::invalid(format!(
            "invalid template revision {rev:?}; use name@<positive integer>"
        ))
    })?;
    Ok(TemplateRef {
        name,
        revision: Some(revision),
    })
}

/// The validated, normalized fields of a registration. `name` is validated
/// separately by [`validate_name`] and kept as sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegisterFields {
    pub image_ref: String,
    pub mount_path: String,
    pub description: String,
}

/// A workspace template name: `[a-z0-9-]{3,40}`, not starting with `warren-`.
pub(crate) fn validate_name(name: &str) -> Result<()> {
    if !ids::valid_template_name(name) {
        return Err(Error::invalid(
            "template name must be 3 to 40 lowercase letters, digits or dashes and must not start or end with a dash",
        ));
    }
    if name.starts_with(RESERVED_NAME_PREFIX) {
        return Err(Error::invalid(format!(
            "template names starting with {RESERVED_NAME_PREFIX:?} are reserved for the built-in catalog"
        )));
    }
    Ok(())
}

/// Check every field of a registration except the name. `default_port` is a
/// `u16`, so the `<= 65535` rule holds by construction.
pub(crate) fn validate_register(req: &RegisterTemplateRequest) -> Result<RegisterFields> {
    let image_ref = validate_image_ref(&req.image_ref)?;
    let mount_path = normalize_mount_path(&req.mount_path)?;
    validate_env(&req.env)?;
    Ok(RegisterFields {
        image_ref,
        mount_path,
        description: req.description.trim().to_string(),
    })
}

/// A non-empty image reference without whitespace or control characters. The
/// runtime decides whether it exists.
pub(crate) fn validate_image_ref(raw: &str) -> Result<String> {
    let image_ref = raw.trim();
    if image_ref.is_empty() {
        return Err(Error::invalid("image_ref is required"));
    }
    if image_ref
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(Error::invalid(
            "image_ref must not contain whitespace or control characters",
        ));
    }
    Ok(image_ref.to_string())
}

/// Empty means [`DEFAULT_MOUNT_PATH`]. Otherwise the path must be absolute, must
/// not be `/` and must not contain `.` or `..` components. Repeated and trailing
/// slashes are collapsed.
pub(crate) fn normalize_mount_path(raw: &str) -> Result<String> {
    let path = raw.trim();
    if path.is_empty() {
        return Ok(DEFAULT_MOUNT_PATH.to_string());
    }
    if !path.starts_with('/') {
        return Err(Error::invalid(format!(
            "mount_path must be an absolute path such as {DEFAULT_MOUNT_PATH}"
        )));
    }
    if path.chars().any(char::is_control) {
        return Err(Error::invalid(
            "mount_path must not contain control characters",
        ));
    }
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if components.is_empty() {
        return Err(Error::invalid("mount_path must not be the root directory"));
    }
    if components.iter().any(|c| *c == "." || *c == "..") {
        return Err(Error::invalid(
            "mount_path must not contain . or .. components",
        ));
    }
    Ok(format!("/{}", components.join("/")))
}

/// Template env defaults follow the instance env rules: at most
/// `MAX_ENV_ENTRIES` keys matching `[A-Z_][A-Z0-9_]*`, values up to
/// `MAX_ENV_VALUE_BYTES`.
pub(crate) fn validate_env(env: &BTreeMap<String, String>) -> Result<()> {
    if env.len() > MAX_ENV_ENTRIES {
        return Err(Error::invalid(format!(
            "env may hold at most {MAX_ENV_ENTRIES} entries"
        )));
    }
    for (key, value) in env {
        if !ids::valid_env_key(key) {
            return Err(Error::invalid(format!(
                "invalid env key {key:?}; keys must match [A-Z_][A-Z0-9_]*"
            )));
        }
        if value.len() > MAX_ENV_VALUE_BYTES {
            return Err(Error::invalid(format!(
                "env value for {key} exceeds {MAX_ENV_VALUE_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use warren_core::errors::CODE_INVALID_REQUEST;

    fn is_invalid<T: std::fmt::Debug>(r: Result<T>) -> bool {
        matches!(r, Err(e) if e.code == CODE_INVALID_REQUEST)
    }

    #[test]
    fn parses_refs() {
        assert_eq!(
            parse_ref("my-agent").unwrap(),
            TemplateRef {
                name: "my-agent",
                revision: None
            }
        );
        assert_eq!(
            parse_ref("  my-agent@3 ").unwrap(),
            TemplateRef {
                name: "my-agent",
                revision: Some(3)
            }
        );
        assert!(is_invalid(parse_ref("")));
        assert!(is_invalid(parse_ref("   ")));
        assert!(is_invalid(parse_ref("@1")));
        assert!(is_invalid(parse_ref("my-agent@")));
        assert!(is_invalid(parse_ref("my-agent@0")));
        assert!(is_invalid(parse_ref("my-agent@-1")));
        assert!(is_invalid(parse_ref("my-agent@latest")));
        assert!(is_invalid(parse_ref("my@agent@2")));
    }

    #[test]
    fn names() {
        assert!(validate_name("my-agent").is_ok());
        assert!(validate_name("abc").is_ok());
        assert!(is_invalid(validate_name("ab")));
        assert!(is_invalid(validate_name("My-Agent")));
        assert!(is_invalid(validate_name("my_agent")));
        assert!(is_invalid(validate_name("-my-agent")));
        assert!(is_invalid(validate_name("my-agent@1")));
        assert!(is_invalid(validate_name("warren-custom")));
        assert!(is_invalid(validate_name(&"a".repeat(41))));
    }

    #[test]
    fn image_refs() {
        assert_eq!(
            validate_image_ref(" ghcr.io/acme/agent:1.2 ").unwrap(),
            "ghcr.io/acme/agent:1.2"
        );
        assert!(validate_image_ref("alpine@sha256:abc").is_ok());
        assert!(is_invalid(validate_image_ref("")));
        assert!(is_invalid(validate_image_ref("  ")));
        assert!(is_invalid(validate_image_ref("ghcr.io/acme/agent 1")));
        assert!(is_invalid(validate_image_ref("ghcr.io/acme\nagent")));
    }

    #[test]
    fn mount_paths() {
        assert_eq!(normalize_mount_path("").unwrap(), DEFAULT_MOUNT_PATH);
        assert_eq!(normalize_mount_path("   ").unwrap(), DEFAULT_MOUNT_PATH);
        assert_eq!(normalize_mount_path("/home/agent").unwrap(), "/home/agent");
        assert_eq!(normalize_mount_path("/home/agent/").unwrap(), "/home/agent");
        assert_eq!(normalize_mount_path("//data///work").unwrap(), "/data/work");
        assert!(is_invalid(normalize_mount_path("home/agent")));
        assert!(is_invalid(normalize_mount_path("./home")));
        assert!(is_invalid(normalize_mount_path("/")));
        assert!(is_invalid(normalize_mount_path("///")));
        assert!(is_invalid(normalize_mount_path("/home/../etc")));
        assert!(is_invalid(normalize_mount_path("/home/./agent")));
        assert!(is_invalid(normalize_mount_path("/home/\u{0}agent")));
    }

    #[test]
    fn env() {
        let mut env = BTreeMap::new();
        assert!(validate_env(&env).is_ok());
        env.insert("OPENAI_API_KEY".into(), "x".into());
        env.insert("_PRIVATE".into(), "y".into());
        assert!(validate_env(&env).is_ok());
        env.insert("lower".into(), "z".into());
        assert!(is_invalid(validate_env(&env)));
        env.remove("lower");
        env.insert("BIG".into(), "v".repeat(MAX_ENV_VALUE_BYTES + 1));
        assert!(is_invalid(validate_env(&env)));
        env.remove("BIG");
        for i in 0..MAX_ENV_ENTRIES {
            env.insert(format!("K{i}"), "v".into());
        }
        assert!(is_invalid(validate_env(&env)));
    }

    #[test]
    fn register_fields() {
        let req = RegisterTemplateRequest {
            name: "my-agent".into(),
            image_ref: " ghcr.io/acme/agent:1 ".into(),
            default_port: 8080,
            mount_path: "".into(),
            agent: true,
            description: "  An agent.  ".into(),
            env: BTreeMap::new(),
        };
        let f = validate_register(&req).unwrap();
        assert_eq!(f.image_ref, "ghcr.io/acme/agent:1");
        assert_eq!(f.mount_path, DEFAULT_MOUNT_PATH);
        assert_eq!(f.description, "An agent.");
    }
}

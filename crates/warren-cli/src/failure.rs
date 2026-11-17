//! Exit codes and the error type every command returns.
//!
//! `0` success, `1` the API (or something local) failed, `2` wrong invocation.

use serde_json::json;
use warren_core::Error;

/// The command succeeded.
pub const EXIT_OK: i32 = 0;
/// The API returned an error, or a local step (file, browser) failed.
pub const EXIT_API: i32 = 1;
/// Wrong invocation: unknown flags, missing key, bad values.
pub const EXIT_USAGE: i32 = 2;

/// Why a command did not succeed.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// Wrong invocation. Exit 2.
    Usage(String),
    /// The API or the SDK returned an error. Exit 1.
    Api(Error),
    /// A local step failed: reading a file, writing the config, opening a browser. Exit 1.
    Local(String),
}

impl Failure {
    /// A usage error (exit 2).
    pub fn usage(msg: impl Into<String>) -> Self {
        Failure::Usage(msg.into())
    }

    /// A local error (exit 1).
    pub fn local(msg: impl Into<String>) -> Self {
        Failure::Local(msg.into())
    }

    /// The process exit code for this failure.
    pub fn exit_code(&self) -> i32 {
        match self {
            Failure::Usage(_) => EXIT_USAGE,
            Failure::Api(_) | Failure::Local(_) => EXIT_API,
        }
    }

    /// The text printed to stderr. With `json`, API errors are printed as the
    /// `{"error": {...}}` envelope the daemon uses.
    pub fn render(&self, json: bool) -> String {
        match self {
            Failure::Usage(m) => format!("warren: {m}\nRun 'warren --help' for usage."),
            Failure::Api(e) if json => serde_json::to_string(&json!({ "error": e }))
                .unwrap_or_else(|_| {
                    format!(
                        "{{\"error\":{{\"code\":{:?},\"message\":{:?}}}}}",
                        e.code, e.message
                    )
                }),
            Failure::Api(e) => {
                let mut s = format!("warren: {}: {}", e.code, e.message);
                if let Some(d) = &e.details {
                    s.push_str(&format!(" ({d})"));
                }
                s
            }
            Failure::Local(m) => format!("warren: {m}"),
        }
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure::Api(e)
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Failure::Local(e.to_string())
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render(false))
    }
}

impl std::error::Error for Failure {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_contract() {
        assert_eq!(Failure::usage("x").exit_code(), 2);
        assert_eq!(Failure::local("x").exit_code(), 1);
        assert_eq!(Failure::from(Error::not_found("instance")).exit_code(), 1);
    }

    #[test]
    fn renders_code_and_message() {
        let f = Failure::from(Error::conflict("instance is stopped"));
        assert_eq!(f.render(false), "warren: conflict: instance is stopped");
        let j: serde_json::Value = serde_json::from_str(&f.render(true)).unwrap();
        assert_eq!(j["error"]["code"], "conflict");
        assert_eq!(j["error"]["message"], "instance is stopped");
        let f = Failure::from(Error::instance_limit(3, 3));
        assert!(
            f.render(false)
                .starts_with("warren: instance_limit_reached: ")
        );
        let j: serde_json::Value = serde_json::from_str(&f.render(true)).unwrap();
        assert_eq!(j["error"]["details"]["limit"], 3);
        assert!(
            Failure::usage("no key")
                .render(false)
                .starts_with("warren: no key")
        );
    }
}

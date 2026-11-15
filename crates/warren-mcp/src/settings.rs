//! Process configuration: the two environment variables the server needs.

use warren_core::{Error, Result};

/// The variable names and the local-daemon default are the SDK's, so the CLI,
/// the SDK and this server read the same environment.
pub use warren_sdk::{DEFAULT_API_URL, ENV_API_KEY, ENV_API_URL};

/// What the binary needs to reach a Warren daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Sandbox API base URL without a trailing slash.
    pub api_url: String,
    /// Plaintext workspace API key.
    pub api_key: String,
}

impl Settings {
    /// Read `WARREN_API_URL` and `WARREN_API_KEY` from the process environment.
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Build settings from any variable source; `None` means unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let api_key = lookup(ENV_API_KEY)
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "{ENV_API_KEY} is not set. Create a key with POST /v1/signup or POST /v1/api-keys and export it."
                ))
            })?;
        let api_url = lookup(ENV_API_URL)
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| DEFAULT_API_URL.to_string());
        if !(api_url.starts_with("http://") || api_url.starts_with("https://")) {
            return Err(Error::invalid(format!(
                "{ENV_API_URL} must start with http:// or https://, got {api_url:?}"
            )));
        }
        Ok(Settings { api_url, api_key })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn defaults_url_and_trims() {
        let s = Settings::from_lookup(vars(&[(ENV_API_KEY, " sk_live_abc ")])).unwrap();
        assert_eq!(s.api_url, DEFAULT_API_URL);
        assert_eq!(s.api_key, "sk_live_abc");
        let s = Settings::from_lookup(vars(&[
            (ENV_API_KEY, "sk_live_abc"),
            (ENV_API_URL, "https://api.example.com/ "),
        ]))
        .unwrap();
        assert_eq!(s.api_url, "https://api.example.com");
    }

    #[test]
    fn rejects_missing_key_and_bad_url() {
        let e = Settings::from_lookup(vars(&[])).unwrap_err();
        assert_eq!(e.code, warren_core::errors::CODE_INVALID_REQUEST);
        assert!(e.message.contains(ENV_API_KEY));
        let e = Settings::from_lookup(vars(&[
            (ENV_API_KEY, "k"),
            (ENV_API_URL, "api.example.com"),
        ]))
        .unwrap_err();
        assert!(e.message.contains(ENV_API_URL));
        let e = Settings::from_lookup(vars(&[(ENV_API_KEY, "   ")])).unwrap_err();
        assert!(e.message.contains(ENV_API_KEY));
    }
}

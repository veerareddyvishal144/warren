//! The contract between the lifecycle, the runtimes and the egress broker
//! (`warren-broker`): what an allowlist pattern is, how a host name matches
//! one, which addresses the broker never connects to, how a secret is
//! validated and which variables an instance receives.
//!
//! The broker is an HTTP forward proxy (plain requests and `CONNECT`) plus
//! "provider routes" on the host. An instance with `network: allowlist` gets
//! the proxy variables pointing at it and can reach nothing else; a request
//! is forwarded only when its host matches one of the instance's
//! `allowed_domains` and resolves to a public address. A secret becomes a
//! route `http://<broker>/r/<token>/...` that the broker rewrites onto the
//! secret's `upstream`, adding `header: value`; the instance only ever sees
//! the token.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use crate::{
    ENV_BROKER, ENV_NETWORK, Error, InstanceSecret, MAX_ALLOWED_DOMAINS, MAX_SECRET_VALUE_BYTES,
    MAX_SECRETS, NetworkMode, Result, SecretSpec, ids,
};

/// Default `--broker-listen`.
pub const DEFAULT_BROKER_LISTEN: &str = "0.0.0.0:7349";
/// Port of the default listen address; also used when a listen address names none.
pub const DEFAULT_BROKER_PORT: u16 = 7349;
/// Path prefix of provider routes: `/r/<token>/<upstream path>`.
pub const ROUTE_PREFIX: &str = "/r/";
/// Value of a secret's `name` variable inside the instance: non-empty so
/// client libraries that insist on a key are satisfied, useless anywhere else.
pub const SECRET_PLACEHOLDER: &str = "warren-broker";
/// Proxy variables set on instances that use the broker as a proxy, in the
/// spellings the common tools read.
pub const PROXY_VARS: [&str; 4] = ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"];
/// Bypass variables, both spellings.
pub const NO_PROXY_VARS: [&str; 2] = ["NO_PROXY", "no_proxy"];
/// Headers a secret may not name: the broker owns them.
const PROTECTED_HEADERS: [&str; 6] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "te",
    "upgrade",
];
/// Longest header name accepted for a secret.
const MAX_HEADER_NAME: usize = 64;
/// Longest upstream URL accepted for a secret.
const MAX_UPSTREAM_BYTES: usize = 2048;
/// Longest allowlist pattern (the DNS limit).
const MAX_DOMAIN_BYTES: usize = 253;

/// Port of a `host:port` listen address; the default broker port when absent
/// or unparsable (`:7349`, `0.0.0.0:7349`, `[::]:7349` all work).
pub fn listen_port(listen: &str) -> u16 {
    listen
        .trim()
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(DEFAULT_BROKER_PORT)
}

/// Host part of a `host:port` address (brackets stripped from IPv6 literals).
pub fn host_of(addr: &str) -> &str {
    let addr = addr.trim();
    if let Some(rest) = addr.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return &rest[..end];
    }
    match addr.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !host.contains(':') => {
            host
        }
        _ => addr,
    }
}

/// Lowercase, no trailing dot: the form patterns are stored in and hosts are
/// compared as.
pub fn normalize_domain(s: &str) -> String {
    s.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

/// A host name with at least two labels, every label `[a-z0-9-]{1,63}` not
/// starting or ending with a dash; `s` must already be normalized.
fn valid_hostname(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_DOMAIN_BYTES
        && s.split('.').count() >= 2
        && s.split('.').all(valid_label)
}

/// An allowlist pattern: a normalized host name (`api.anthropic.com`) or a
/// wildcard `*.suffix` (`*.npmjs.org`), where the suffix is itself a host
/// name with at least two labels. Raw IP addresses are not patterns.
pub fn valid_domain_pattern(pattern: &str) -> bool {
    let p = normalize_domain(pattern);
    if p != pattern {
        return false;
    }
    if p.parse::<IpAddr>().is_ok() {
        return false;
    }
    match p.strip_prefix("*.") {
        Some(suffix) => !suffix.contains('*') && valid_hostname(suffix),
        None => !p.contains('*') && valid_hostname(&p),
    }
}

/// Whether `host` (any case, optional trailing dot) matches one of
/// `patterns`: equal to an exact pattern, or any number of labels under the
/// suffix of a wildcard (`*.npmjs.org` matches `registry.npmjs.org` and
/// `a.b.npmjs.org`, never `npmjs.org` itself).
pub fn host_matches(host: &str, patterns: &[String]) -> bool {
    let host = normalize_domain(host);
    if host.is_empty() {
        return false;
    }
    patterns.iter().any(|p| match p.strip_prefix("*.") {
        Some(suffix) => host
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1),
        None => host == *p,
    })
}

/// Normalize and check an `allowed_domains` list: at most
/// [`MAX_ALLOWED_DOMAINS`] valid patterns, duplicates removed.
pub fn normalize_allowed_domains(domains: &[String]) -> Result<Vec<String>> {
    if domains.len() > MAX_ALLOWED_DOMAINS {
        return Err(Error::invalid(format!(
            "allowed_domains may hold at most {MAX_ALLOWED_DOMAINS} entries"
        )));
    }
    let mut out = Vec::with_capacity(domains.len());
    for d in domains {
        let n = normalize_domain(d);
        if !valid_domain_pattern(&n) {
            return Err(Error::invalid(format!(
                "allowed_domains entry {d:?} is not a host name such as api.example.com or a pattern such as *.example.com"
            )));
        }
        if !out.contains(&n) {
            out.push(n);
        }
    }
    Ok(out)
}

/// An HTTP header field name: RFC 9110 token characters, not one of the
/// headers the broker manages itself.
pub fn valid_header_name(name: &str) -> bool {
    const EXTRA: &[u8] = b"!#$%&'*+-.^_`|~";
    !name.is_empty()
        && name.len() <= MAX_HEADER_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || EXTRA.contains(&b))
        && !PROTECTED_HEADERS
            .iter()
            .any(|h| h.eq_ignore_ascii_case(name))
}

/// An upstream is `https://host[:port][/path]`: no userinfo, query or
/// fragment, no whitespace, a valid host name or IP literal.
pub fn valid_upstream(upstream: &str) -> bool {
    if upstream.len() > MAX_UPSTREAM_BYTES || upstream.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let Some(rest) = upstream.strip_prefix("https://") else {
        return false;
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() || authority.contains('@') || path.contains(['?', '#']) {
        return false;
    }
    let host = host_of(authority);
    if host.is_empty() {
        return false;
    }
    if authority.ends_with(':') {
        return false;
    }
    let port_ok = match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') || h.starts_with('[') => {
            p.parse::<u16>().is_ok_and(|p| p > 0) || h.starts_with('[') && !p.contains(']')
        }
        _ => true,
    };
    port_ok && (host.parse::<IpAddr>().is_ok() || valid_hostname(&normalize_domain(host)))
}

/// Variable names a secret may not claim: the platform's and the proxy ones.
fn reserved_secret_var(name: &str) -> bool {
    name.starts_with("WARREN_") || PROXY_VARS.contains(&name) || NO_PROXY_VARS.contains(&name)
}

/// Check a secrets list: at most [`MAX_SECRETS`], distinct `name`s and `env`s
/// that are valid, non-reserved variable names and do not collide with each
/// other, `https://` upstreams, token header names, values of at most
/// [`MAX_SECRET_VALUE_BYTES`] bytes without control characters.
pub fn validate_secrets(secrets: &[SecretSpec]) -> Result<()> {
    if secrets.len() > MAX_SECRETS {
        return Err(Error::invalid(format!(
            "secrets may hold at most {MAX_SECRETS} entries"
        )));
    }
    let mut vars: BTreeSet<&str> = BTreeSet::new();
    for s in secrets {
        for (what, var) in [("name", s.name.as_str()), ("env", s.env.as_str())] {
            if !ids::valid_env_key(var) {
                return Err(Error::invalid(format!(
                    "secret {what} {var:?} is invalid; variable names match [A-Z_][A-Z0-9_]*"
                )));
            }
            if reserved_secret_var(var) {
                return Err(Error::invalid(format!(
                    "secret {what} {var:?} is set by the platform and cannot be used"
                )));
            }
            if !vars.insert(var) {
                return Err(Error::invalid(format!(
                    "secret variable {var:?} is used more than once across names and envs"
                )));
            }
        }
        if !valid_upstream(&s.upstream) {
            return Err(Error::invalid(format!(
                "secret {} upstream {:?} must be an https:// URL with a host and an optional path",
                s.name, s.upstream
            )));
        }
        if !valid_header_name(&s.header) {
            return Err(Error::invalid(format!(
                "secret {} header {:?} is not a usable header name",
                s.name, s.header
            )));
        }
        if s.value.is_empty() {
            return Err(Error::invalid(format!("secret {} has an empty value", s.name)));
        }
        if s.value.len() > MAX_SECRET_VALUE_BYTES {
            return Err(Error::invalid(format!(
                "secret {} value is longer than {MAX_SECRET_VALUE_BYTES} bytes",
                s.name
            )));
        }
        if s.value.chars().any(char::is_control) {
            return Err(Error::invalid(format!(
                "secret {} value must not contain control characters",
                s.name
            )));
        }
    }
    Ok(())
}

/// The header value the broker sends: `Authorization` gets `Bearer <value>`
/// unless the value already starts with a scheme (`Bearer x`, `Basic x`,
/// `token x`); any other header carries the value as is.
pub fn header_value(header: &str, value: &str) -> String {
    if header.eq_ignore_ascii_case("authorization") {
        let has_scheme = value
            .split_once(' ')
            .is_some_and(|(scheme, rest)| {
                !scheme.is_empty()
                    && scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                    && !rest.trim().is_empty()
            });
        if !has_scheme {
            return format!("Bearer {value}");
        }
    }
    value.to_string()
}

/// A fresh route token: 32 hex characters from the kernel's randomness.
pub fn new_route_token() -> String {
    hex::encode(ids::random_bytes(16))
}

/// `http://<broker>/r/<token>`: what the secret's `env` variable holds.
pub fn route_url(broker: &str, token: &str) -> String {
    format!("http://{broker}{ROUTE_PREFIX}{token}")
}

/// Whether an instance in `mode` needs the broker at all: every `allowlist`
/// instance does, other modes only for their secrets.
pub fn uses_broker(mode: NetworkMode, has_secrets: bool) -> bool {
    mode == NetworkMode::Allowlist || has_secrets
}

/// Whether the proxy variables point at the broker: on `allowlist` always, on
/// `deny-all` when there are secrets (so a tool that ignores the route URL
/// fails fast with a refusal instead of a timeout). `allow-all` never
/// proxies through the broker.
pub fn proxy_env_wanted(mode: NetworkMode, has_secrets: bool) -> bool {
    match mode {
        NetworkMode::Allowlist => true,
        NetworkMode::DenyAll => has_secrets,
        NetworkMode::AllowAll => false,
    }
}

/// The variables an instance receives for its network mode, the broker
/// address it can reach (`host:port`, None when the daemon has none) and its
/// secrets: always `WARREN_NETWORK`; with a broker also `WARREN_BROKER`, the
/// proxy variables when [`proxy_env_wanted`], and per secret `<env>` = the
/// route URL and `<name>` = [`SECRET_PLACEHOLDER`].
pub fn instance_env(
    mode: NetworkMode,
    broker: Option<&str>,
    secrets: &[InstanceSecret],
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(ENV_NETWORK.to_string(), mode.as_str().to_string());
    let Some(broker) = broker else {
        return env;
    };
    env.insert(ENV_BROKER.to_string(), broker.to_string());
    if proxy_env_wanted(mode, !secrets.is_empty()) {
        let url = format!("http://{broker}");
        for var in PROXY_VARS {
            env.insert(var.to_string(), url.clone());
        }
        let bypass = format!("localhost,127.0.0.1,{}", host_of(broker));
        for var in NO_PROXY_VARS {
            env.insert(var.to_string(), bypass.clone());
        }
    }
    for s in secrets {
        env.insert(s.env.clone(), route_url(broker, &s.route_token));
        env.insert(s.name.clone(), SECRET_PLACEHOLDER.to_string());
    }
    env
}

/// Addresses the broker never connects to on an instance's behalf: loopback,
/// unspecified, private (RFC 1918), carrier-grade NAT (100.64/10), link-local
/// (the cloud metadata service lives there), multicast, broadcast,
/// documentation and reserved ranges, and their IPv6 counterparts (unique
/// local, link-local, multicast, IPv4-mapped and NAT64 forms of the above).
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => forbidden_v4(v4),
        IpAddr::V6(v6) => forbidden_v6(v6),
    }
}

fn forbidden_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (64..=127).contains(&o[1]))
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        || o[0] >= 240
}

fn forbidden_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return forbidden_v4(v4);
    }
    let s = ip.segments();
    // NAT64 well-known prefix 64:ff9b::/96 embeds an IPv4 address.
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6].iter().all(|x| *x == 0) {
        return forbidden_v4(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            s[6] as u8,
            (s[7] >> 8) as u8,
            s[7] as u8,
        ));
    }
    // Deprecated IPv4-compatible addresses (::a.b.c.d).
    if s[..6].iter().all(|x| *x == 0) && (s[6] != 0 || s[7] > 1) {
        return true;
    }
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] == 0x2001 && s[1] == 0x0db8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(name: &str, env: &str) -> SecretSpec {
        SecretSpec {
            name: name.into(),
            upstream: "https://api.example.com".into(),
            header: "x-api-key".into(),
            value: "sk-test".into(),
            env: env.into(),
        }
    }

    #[test]
    fn listen_port_and_host() {
        assert_eq!(listen_port("0.0.0.0:7349"), 7349);
        assert_eq!(listen_port(":8000"), 8000);
        assert_eq!(listen_port("[::]:9000"), 9000);
        assert_eq!(listen_port("garbage"), DEFAULT_BROKER_PORT);
        assert_eq!(host_of("172.18.0.1:7349"), "172.18.0.1");
        assert_eq!(host_of("[fd00::1]:7349"), "fd00::1");
        assert_eq!(host_of("broker.internal"), "broker.internal");
        assert_eq!(host_of("fd00::1"), "fd00::1");
    }

    #[test]
    fn domain_patterns() {
        for ok in [
            "api.anthropic.com",
            "*.npmjs.org",
            "a-b.example.co.uk",
            "*.a.b",
        ] {
            assert!(valid_domain_pattern(ok), "{ok}");
        }
        for bad in [
            "",
            "localhost",
            "com",
            "*.com",
            "*",
            "Api.Anthropic.com",
            "api.anthropic.com.",
            "-bad.example.com",
            "bad-.example.com",
            "a.*.example.com",
            "**.example.com",
            "10.0.0.1",
            "exa mple.com",
            "under_score.example.com",
            "http://example.com",
        ] {
            assert!(!valid_domain_pattern(bad), "{bad}");
        }
        let long = format!("{}.example.com", "a".repeat(64));
        assert!(!valid_domain_pattern(&long));
        assert_eq!(normalize_domain(" API.Example.COM. "), "api.example.com");
        let domains = normalize_allowed_domains(&[
            "API.anthropic.com".into(),
            "api.anthropic.com.".into(),
            "*.npmjs.org".into(),
        ])
        .unwrap();
        assert_eq!(domains, ["api.anthropic.com", "*.npmjs.org"]);
        assert!(normalize_allowed_domains(&["localhost".into()]).is_err());
        let many: Vec<String> = (0..=MAX_ALLOWED_DOMAINS)
            .map(|i| format!("h{i}.example.com"))
            .collect();
        assert!(normalize_allowed_domains(&many).is_err());
    }

    #[test]
    fn host_matching() {
        let patterns = vec!["api.anthropic.com".to_string(), "*.npmjs.org".to_string()];
        assert!(host_matches("api.anthropic.com", &patterns));
        assert!(host_matches("API.Anthropic.COM.", &patterns));
        assert!(host_matches("registry.npmjs.org", &patterns));
        assert!(host_matches("a.b.npmjs.org", &patterns));
        assert!(!host_matches("npmjs.org", &patterns));
        assert!(!host_matches("evilnpmjs.org", &patterns));
        assert!(!host_matches("anthropic.com", &patterns));
        assert!(!host_matches("api.anthropic.com.evil.com", &patterns));
        assert!(!host_matches("", &patterns));
        assert!(!host_matches("api.anthropic.com", &[]));
    }

    #[test]
    fn header_names_and_values() {
        assert!(valid_header_name("x-api-key"));
        assert!(valid_header_name("Authorization"));
        assert!(!valid_header_name("Host"));
        assert!(!valid_header_name("content-length"));
        assert!(!valid_header_name("x api"));
        assert!(!valid_header_name(""));
        assert!(!valid_header_name("x:y"));
        assert_eq!(header_value("Authorization", "sk-1"), "Bearer sk-1");
        assert_eq!(header_value("authorization", "Bearer sk-1"), "Bearer sk-1");
        assert_eq!(header_value("Authorization", "Basic abc="), "Basic abc=");
        assert_eq!(header_value("Authorization", "token ghp_x"), "token ghp_x");
        assert_eq!(header_value("x-api-key", "sk-1"), "sk-1");
        assert_eq!(header_value("x-api-key", "two words"), "two words");
    }

    #[test]
    fn upstreams() {
        for ok in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            "https://api.openai.com/v1",
            "https://host.example.com:8443/base/path",
            "https://[2001:db8::1]:8443/x",
            "https://93.184.216.34/x",
        ] {
            assert!(valid_upstream(ok), "{ok}");
        }
        for bad in [
            "http://api.anthropic.com",
            "https://",
            "https://user:pw@api.example.com",
            "https://api.example.com?x=1",
            "https://api.example.com/#frag",
            "https://api example.com",
            "https://localhost",
            "https://api.example.com:",
            "https://api.example.com:notaport",
            "ftp://api.example.com",
        ] {
            assert!(!valid_upstream(bad), "{bad}");
        }
    }

    #[test]
    fn secret_validation() {
        assert!(validate_secrets(&[]).is_ok());
        assert!(
            validate_secrets(&[
                secret("ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL"),
                secret("OPENAI_API_KEY", "OPENAI_BASE_URL")
            ])
            .is_ok()
        );
        let bad = [
            secret("lower", "X_BASE_URL"),
            secret("X_KEY", "bad env"),
            secret("WARREN_X", "X_BASE_URL"),
            secret("HTTPS_PROXY", "X_BASE_URL"),
            secret("SAME", "SAME"),
            SecretSpec {
                upstream: "http://api.example.com".into(),
                ..secret("X_KEY", "X_BASE_URL")
            },
            SecretSpec {
                header: "Host".into(),
                ..secret("X_KEY", "X_BASE_URL")
            },
            SecretSpec {
                value: String::new(),
                ..secret("X_KEY", "X_BASE_URL")
            },
            SecretSpec {
                value: "a\nb".into(),
                ..secret("X_KEY", "X_BASE_URL")
            },
            SecretSpec {
                value: "v".repeat(MAX_SECRET_VALUE_BYTES + 1),
                ..secret("X_KEY", "X_BASE_URL")
            },
        ];
        for s in bad {
            let err = validate_secrets(std::slice::from_ref(&s)).unwrap_err();
            assert_eq!(err.code, crate::errors::CODE_INVALID_REQUEST, "{s:?}");
        }
        // Duplicate names and env/name collisions across entries.
        assert!(
            validate_secrets(&[secret("A_KEY", "A_URL"), secret("A_KEY", "B_URL")]).is_err()
        );
        assert!(
            validate_secrets(&[secret("A_KEY", "A_URL"), secret("B_KEY", "A_KEY")]).is_err()
        );
        let many: Vec<SecretSpec> = (0..=MAX_SECRETS)
            .map(|i| secret(&format!("K{i}"), &format!("U{i}")))
            .collect();
        assert!(validate_secrets(&many).is_err());
        // Debug output never shows the value.
        let dbg = format!("{:?}", secret("A_KEY", "A_URL"));
        assert!(dbg.contains("<redacted>") && !dbg.contains("sk-test"));
    }

    #[test]
    fn env_for_each_mode() {
        let stored = InstanceSecret {
            instance_id: "ab12cd34ef".into(),
            name: "ANTHROPIC_API_KEY".into(),
            upstream: "https://api.anthropic.com".into(),
            header: "x-api-key".into(),
            value: "sk-real".into(),
            env: "ANTHROPIC_BASE_URL".into(),
            route_token: "tok".into(),
            created_at: 1,
        };
        let env = instance_env(NetworkMode::Allowlist, Some("172.18.0.1:7349"), &[stored.clone()]);
        assert_eq!(env["WARREN_NETWORK"], "allowlist");
        assert_eq!(env["WARREN_BROKER"], "172.18.0.1:7349");
        for var in PROXY_VARS {
            assert_eq!(env[var], "http://172.18.0.1:7349");
        }
        assert_eq!(env["NO_PROXY"], "localhost,127.0.0.1,172.18.0.1");
        assert_eq!(env["no_proxy"], "localhost,127.0.0.1,172.18.0.1");
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://172.18.0.1:7349/r/tok");
        assert_eq!(env["ANTHROPIC_API_KEY"], SECRET_PLACEHOLDER);
        assert!(
            !env.values().any(|v| v.contains("sk-real")),
            "the value never reaches the instance"
        );

        let env = instance_env(NetworkMode::DenyAll, Some("b:1"), &[stored.clone()]);
        assert_eq!(env["HTTPS_PROXY"], "http://b:1");
        let env = instance_env(NetworkMode::DenyAll, Some("b:1"), &[]);
        assert!(!env.contains_key("HTTPS_PROXY"));
        assert_eq!(env["WARREN_BROKER"], "b:1");
        let env = instance_env(NetworkMode::AllowAll, Some("b:1"), &[stored]);
        assert!(!env.contains_key("HTTPS_PROXY"));
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://b:1/r/tok");
        let env = instance_env(NetworkMode::Allowlist, None, &[]);
        assert_eq!(env.len(), 1);
        assert_eq!(env["WARREN_NETWORK"], "allowlist");

        assert!(uses_broker(NetworkMode::Allowlist, false));
        assert!(!uses_broker(NetworkMode::DenyAll, false));
        assert!(uses_broker(NetworkMode::AllowAll, true));
        assert_eq!(new_route_token().len(), 32);
        assert_ne!(new_route_token(), new_route_token());
    }

    #[test]
    fn forbidden_addresses() {
        let bad = [
            "127.0.0.1",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.64.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.254",
            "192.0.0.1",
            "198.18.0.5",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "ff02::1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a00:1",
            "2001:db8::1",
            "::10.0.0.1",
        ];
        for ip in bad {
            assert!(is_forbidden_ip(ip.parse().unwrap()), "{ip}");
        }
        let good = [
            "93.184.216.34",
            "1.1.1.1",
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ];
        for ip in good {
            assert!(!is_forbidden_ip(ip.parse().unwrap()), "{ip}");
        }
    }
}

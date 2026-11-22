# Security policy

Warren runs code written by agents on hardware you own, so a hole in it is a hole in your machine. If you find one, we want to hear about it privately and fix it quickly. The model this policy defends is described in [docs/security-model.md](docs/security-model.md); the detailed reference is [docs/security.md](docs/security.md).

## Supported versions

| Version | Security fixes |
|---|---|
| `main` | Yes. Fixes land here first. |
| The latest tagged release | Yes, as a new release from `main`; there are no maintenance branches. |
| Older releases | No. Upgrade to the latest release. |

Warren is pre-1.0 (0.1.0 is not yet tagged at the time of writing). Until 1.0, every release may contain breaking changes and only the newest one receives fixes.

## How to report

Report through **GitHub private vulnerability reporting**: [github.com/veerareddyvishal144/warren/security/advisories/new](https://github.com/veerareddyvishal144/warren/security/advisories/new). It opens a private thread with the maintainer; nothing is visible to others until an advisory is published. Do not open a public issue, pull request or discussion for a vulnerability, and do not post about it before a fix is available.

Include what you can of: the version or commit you tested, the host (OS, Docker and gVisor versions, Firecracker or Lume version), the instance type, steps to reproduce, the impact you believe it has, and whether you have seen it exploited. A minimal proof of concept is more useful than a long description.

## Response targets

| Step | Target |
|---|---|
| Acknowledgement that we received the report | 3 business days |
| Triage: confirmed, not a vulnerability, or we need more information | 10 business days |
| Fix released for a critical issue (instance escape, edge authentication bypass, cross-workspace data access, a secret reaching an instance it was not scoped to) | 30 days from confirmation, sooner where possible |
| Fix released for other confirmed issues | 90 days from the report |
| Public disclosure | Coordinated with you when the fix ships; at most 90 days after the report unless we agree otherwise; sooner, with mitigations, if the issue is being exploited |

If a fix depends on an upstream project (gVisor, Firecracker, Docker, Lume), we will say so, tell you what we can mitigate on our side, and agree a new date with you. We credit reporters in the release notes unless they prefer not to be named.

## Scope

In scope: everything in this repository: `warrend`, `warren-gateway`, the `warren` CLI, `warren-mcp`, the SDKs, the template images, the macOS base-image scripts under `scripts/mac/` and the other scripts.

Of particular interest:

- escaping an instance to the host or to another instance, on any runtime;
- bypassing edge authentication, forging signed URLs or session cookies, or reaching an instance of another workspace;
- the per-instance gateway token or a workspace key becoming visible inside an instance;
- the egress allowlist being bypassed, or the secret broker injecting a credential for an instance or destination it was not scoped to;
- on a Mac, guest-to-guest reachability past the `pf` rules, or a remote path to Lume's local API;
- the base-image install scripts leaving the guest weaker than documented.

Out of scope:

- vulnerabilities in third-party software that are already public (report them upstream; tell us if Warren is affected so we can ship a mitigation);
- anything that requires a compromised host or operator, or physical access;
- limitations documented in [docs/limits.md](docs/limits.md), [docs/security-model.md](docs/security-model.md) under "What is not isolated today", and [docs/security.md](docs/security.md) under "Known limitations", including the lack of authentication on Lume's local API, the absence of TLS in the daemon, and the lack of a privilege boundary inside an instance. A report that shows one of these is worse than described, or reachable from outside the host, is in scope;
- resource exhaustion that stays within the limits the operator configured.

## Safe harbour

Good-faith research that follows this policy, avoids privacy violations, data destruction and service disruption, and tests only installations you own or have permission to test, will not be met with legal action from this project.

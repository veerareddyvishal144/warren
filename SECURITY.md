# Security policy

## Reporting a vulnerability

Email **security@** (replace with the project's domain, for example `security@warren.example.com`) with a description of the issue, the version or commit you tested, steps to reproduce, and the impact you believe it has. Encrypt sensitive details if you can; a PGP key will be published alongside the address.

Please do not open a public GitHub issue or discuss the problem publicly before a fix is available.

You should receive an acknowledgement within 3 business days. We will keep you informed as we confirm the issue, work on a fix and plan the release.

## Coordinated disclosure: 90 days

- We aim to fix confirmed vulnerabilities and publish a release within **90 days** of the report.
- When a fix is ready earlier we release it earlier and coordinate the publication date with you.
- If a fix needs longer (for example because it depends on an upstream project such as gVisor, Firecracker or Docker), we will tell you why and agree on a new date; absent agreement, you are free to publish 90 days after your report.
- Reports about actively exploited issues may be disclosed sooner, together with mitigations.

We credit reporters in the release notes unless they prefer otherwise.

## Scope

In scope: `warrend`, `warren-gateway`, the CLI, the MCP server, the SDKs, the template images and the scripts in this repository. Of particular interest: escaping an instance to the host or to another instance, bypassing edge authentication, reading another workspace's data, forging signed URLs, and billing bypasses.

Out of scope: vulnerabilities in third-party dependencies that are already public (report them upstream, but tell us if Warren is affected), issues that require a compromised host, and limitations documented in [docs/security.md](docs/security.md) under "What is not isolated or protected yet" (these are known gaps and are welcome as pull requests).

## Supported versions

Warren is pre-1.0. Security fixes land on the `main` branch and in the next release; there are no maintenance branches yet.

## Safe harbour

Good-faith research that respects this policy, avoids privacy violations, data destruction and service disruption, and only tests against installations you own or have permission to test, will not be met with legal action from the project.

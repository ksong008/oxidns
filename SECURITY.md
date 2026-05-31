# Security Policy

OxiDNS is a DNS policy orchestration engine that can run on gateways,
servers, homelabs, and other network edge systems. Security reports are
handled privately so fixes can be prepared before details are disclosed.

## Supported Versions

Security fixes are prioritized for:

- The latest stable release published on GitHub Releases.
- The current `main` branch.

Older releases may receive fixes when the issue is critical and a narrow,
low-risk patch can be prepared. In general, users should upgrade to the latest
stable release after a security fix is published.

## Reporting a Vulnerability

Please do not open a public GitHub issue for suspected vulnerabilities.

Use one of these private channels instead:

- Email: `isvenshi@gmail.com`
- GitHub Security Advisory / private vulnerability reporting, if available for
  this repository.

Use a subject such as `[OxiDNS Security] <short summary>`.

Helpful reports include:

- Affected OxiDNS version, release artifact, platform, and commit if known.
- Whether the issue affects server listeners, upstreams, cache behavior,
  plugins, WebUI hosting, install scripts, or one of the workspace crates.
- A minimal configuration or reproduction steps with secrets redacted.
- Expected behavior, observed behavior, and any logs or packet captures needed
  to understand impact.
- Whether the issue is reachable remotely, locally, or only by a trusted
  operator with configuration access.

## Response Process

After a report is received, the maintainer will try to:

- Acknowledge the report within 7 days.
- Triage the impact and affected components.
- Coordinate reproduction details and patch validation with the reporter when
  needed.
- Publish a fix, release note, and advisory or changelog entry when appropriate.

Please keep vulnerability details private until a fix or mitigation is
available and public disclosure has been coordinated.

## Security Scope

Reports are most useful when they involve one of these areas:

- DNS protocol handling for UDP, TCP, DoT, DoQ, DoH, HTTP/2, or HTTP/3 paths.
- Upstream connection pooling, TLS validation, bootstrap resolution, or fallback
  behavior.
- Cache correctness, TTL handling, negative caching, rewrite logic, or synthetic
  response generation.
- Plugin execution boundaries, especially plugins with side effects such as
  `script`, `http_request`, `download`, `upgrade`, `ipset`, `nftset`, and
  `ros_address_list`.
- Management API, health endpoints, metrics, WebUI hosting, and release/install
  scripts.
- Workspace crates such as `oxidns-proto`, `oxidns-ripset`, macros, and the
  zone parser.
- Dependency or supply-chain issues that create a concrete risk for OxiDNS
  users.

The following are usually not treated as security vulnerabilities by
themselves:

- Behavior that requires an already-trusted operator to install a malicious
  configuration.
- DNS routing, filtering, or privacy outcomes that follow from the documented
  configuration.
- Denial of service from intentionally expensive local configurations without a
  separate remote trigger.
- Outdated dependencies without a demonstrated impact on OxiDNS.

## Safe Testing

Only test against systems you own or have permission to assess. Avoid actions
that disrupt public DNS service, leak user query data, bypass access controls,
or modify third-party systems. Keep proof-of-concept traffic and captured data
to the minimum needed for verification.

## Deployment Hardening

OxiDNS operators should consider these baseline precautions:

- Bind management APIs and WebUI hosting to localhost or a trusted management
  network unless they are protected by a firewall, VPN, or authenticated reverse
  proxy.
- Protect `config.yaml`, local rule/provider files, TLS private keys, query
  logs, and query recorder output. DNS logs can contain sensitive metadata.
- Run the service with the least privileges possible. Grant elevated privileges
  only when required for low ports, `ipset`, `nftset`, service management, or
  route synchronization.
- Disable unused server protocols and plugins.
- Review configurations that execute commands, download remote content, issue
  HTTP requests, upgrade binaries, or synchronize external systems.
- Keep OxiDNS, Rust toolchains, operating system packages, and TLS certificates
  up to date.

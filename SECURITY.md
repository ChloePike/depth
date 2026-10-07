# Security

Depth handles exchange API keys and can place orders, so security reports matter.

Please report vulnerabilities privately through
[GitHub security advisories](https://github.com/ChloePike/depth/security/advisories/new), not in a
public issue. Include the version, what an attacker could do, and how to reproduce.

Scope that matters most: anything that could leak API keys (Keychain handling, logs, crash reports),
send orders the user did not confirm, or bypass the rate-limit protection.

Never include real API keys or account data in a report.

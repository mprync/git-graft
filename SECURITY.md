# Security policy

## Supported versions

Security fixes are released for the latest published version.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's [private vulnerability reporting](https://github.com/mprync/git-graft/security/advisories/new) for this repository. Do not open a public issue, and do not include credentials, graft leases or private repository data in the report.

Include the affected version or commit, the operating system and filesystem, a minimal reproduction and the impact you observed. You will receive an acknowledgement, and fixes are coordinated with you before details are published.

## Trust boundary

Graft runs local Git operations, Git hooks and filters, and optional package-manager commands with the permissions of the user who invokes it. Use it with repositories, dependencies and MCP clients you trust. Graft leases authorize changes to a graft and should be shared only with trusted local tools. Archived grafts can retain sensitive files, such as ignored credentials. See [safety and recovery](docs/safety.md) for details.

Every release publishes `SHA256SUMS` and GitHub build provenance for its archives and packages; the install scripts verify the checksum before installing. See [verifying a download](docs/install.md#verify-a-download).

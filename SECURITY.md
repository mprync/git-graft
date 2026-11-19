# Security policy

Graft runs local Git operations and optional commands with the invoking user's permissions. Use trusted repositories, dependencies and MCP clients. Leases coordinate local clients and should only be shared with trusted local tools. Archives may retain sensitive workspace data. See [safety and recovery](docs/safety.md) for the trust boundary.

## Reporting a vulnerability

Please contact the repository maintainer privately through an established, trusted channel. This repository does not yet publish a dedicated security contact or reporting URL. Avoid publishing exploit details, credentials, leases or private repository data in a public issue.

Include the affected version or commit, operating system, a minimal reproduction and the potential impact. Redact secrets and sensitive paths. The maintainer can coordinate investigation and disclosure through the private channel used for the report; no response timeline is currently specified.

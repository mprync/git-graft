# Contributing

Build with Rust 1.89 or newer and Git. Run these checks before submitting changes:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
npm test
```

The end-to-end suite uses disposable repositories and needs the tools listed in [verification](docs/verification.md). Exercise user-facing operations through the CLI and MCP, and cover supported and fallback filesystem paths where relevant. For retirement and recovery changes, verify that refused or interrupted operations preserve the original files and refs.

When reporting a bug, include the operating system, Git version, storage receipt and a minimal reproduction. Remove leases and private repository data from logs. Contributions use the [MIT license](LICENSE).

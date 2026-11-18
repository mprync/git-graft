# Contributing

Build with stable Rust and Git. Before submitting a change run `cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked` and `npm test`. Tests create disposable repositories.

Keep Git lifecycle decisions in the shared engine. CLI argument definitions also generate the MCP input schemas. A new user-facing operation needs assertions through the binary and the stdio protocol in the TypeScript suite. Filesystem changes need both a supported-volume case and a fallback case. Destructive changes need a test that proves the original files or refs survive refusal and interrupted retries.

Include the operating system, Git version, reported storage backend and a minimal reproduction when reporting a bug. Remove lease values and private repository data from shared logs. Contributions use the project's MIT license.

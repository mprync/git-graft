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

## Releasing

Pushing a `v*` tag builds, verifies, attests and publishes a release; a tag with a pre-release suffix such as `v0.2.0-rc.1` publishes a GitHub pre-release only. Running the Release workflow by hand is a dry run that builds, packages and smoke-tests everything without publishing.

Each package manager is updated only when its repository secret exists:

| Secret | Used for |
| --- | --- |
| `HOMEBREW_TAP_DEPLOY_KEY` | Write deploy key on `mprync/homebrew-tap` |
| `SCOOP_BUCKET_DEPLOY_KEY` | Write deploy key on `mprync/scoop-bucket` |
| `CHOCOLATEY_API_KEY` | Chocolatey community repository API key |
| `WINGET_TOKEN` | Classic GitHub token with `public_repo` scope, used to open winget-pkgs pull requests |
| `AUR_SSH_KEY` | SSH key registered on the AUR account that owns `git-graft-bin` |

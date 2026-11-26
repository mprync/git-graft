# Contributing

Build with Rust 1.89 or newer and Git. Run these checks before submitting changes:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
npm test
```

The end-to-end suite uses disposable repositories and needs the tools listed in [verification](docs/verification.md). Exercise user-facing operations through the CLI and MCP, and cover supported and fallback filesystem paths where relevant. For retirement and recovery changes, verify that refused or interrupted operations preserve the original files and refs.

Benchmarks live in `e2e/*-bench.ts` (see `npm run` for the list) and are written up in [benchmarks](docs/benchmarks.md). [bench/agents/PLAN.md](bench/agents/PLAN.md) describes a many-agent end-to-end run for stress-testing Graft. The README charts and diagrams are drawn by `scripts/charts/charts.js`; run `node scripts/charts/serve.ts` to regenerate them in `docs/assets`.

Pushes that change only Markdown or `docs/` skip CI.

When reporting a bug, include the operating system, Git version, storage receipt and a minimal reproduction. Remove leases and private repository data from logs. Contributions use the [MIT license](LICENSE).

## Changelog

A pull request that changes anything a user of Graft can notice adds to `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md), in the same pull request, without being asked. Write for someone deciding whether to upgrade, and match the released sections below it.

- Write one bullet per feature or fix, in one or two short sentences and under 40 words. Say what someone can now do, or what no longer goes wrong, not how it works.
- File it under `Added`, `Changed`, `Fixed`, `Removed` or `Safety`.
- Name commands, options and settings in backticks. Leave out error codes, JSON fields, internal files, platform edge cases and design detail: those belong in [docs](docs/operations.md).
- Give before-and-after numbers for a speed or size change, such as "1.0 s instead of 16 s".
- When a change extends or fixes something that is still under `Unreleased`, edit that bullet instead of adding one. Users never saw the earlier version.
- If upgrading can break scripts or needs action, add a bold note at the top of the section, as 0.2.0 does.

## Releasing

Pushing a `v*` tag builds, verifies, attests and publishes a release; a tag with a pre-release suffix such as `v0.2.0-rc.1` publishes a GitHub pre-release only. Running the Release workflow by hand is a dry run that builds, packages and smoke-tests everything without publishing.

Each package manager is updated only when its repository secret exists:

| Secret | Used for |
| --- | --- |
| `HOMEBREW_TAP_DEPLOY_KEY` | Write deploy key on `mprync/homebrew-tap` |
| `SCOOP_BUCKET_DEPLOY_KEY` | Write deploy key on `mprync/scoop-bucket` |
| `CHOCOLATEY_API_KEY` | Chocolatey community repository API key |
| `WINGET_TOKEN` | Classic GitHub token with `public_repo` scope, used to open winget-pkgs pull requests |
| `CARGO_REGISTRY_TOKEN` | crates.io API token with publish rights for `git-graft` |
| `AUR_SSH_KEY` | SSH key registered on the AUR account that owns `git-graft-bin` |

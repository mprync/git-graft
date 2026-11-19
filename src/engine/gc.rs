//! Cleanup for verified package caches and dependency layers.
use crate::{
    api,
    cache::{self, PackageBinding, PurgeFailure},
    layer,
    registry::{CACHE_OWNER, Project, check_owned_tree, exists},
    storage,
    workspace::{
        HISTORY_CACHE_DIR, HISTORY_CACHE_MARKER, history_cache_root,
        verify_history_cache_generation,
    },
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fs, path::Path};

#[derive(PartialEq, Eq)]
enum Verified {
    Package(PackageBinding),
    Layer(Value),
    Base(Vec<[u8; 32]>),
    History([u8; 32]),
}

#[derive(Clone, Copy)]
enum Kind {
    Package,
    Layer,
    RegisteredInstall,
    Base,
    History,
}

pub(crate) fn purge_caches(p: &Project, apply: bool) -> Result<Value> {
    // A legacy workspace may still use these shared caches. Treat its registry
    // as opaque and retain the caches if there are records or ownership is unclear.
    if p.has_legacy_workspace_records()? {
        return Ok(json!({"requested":true, "eligible":false, "purged":false,
            "reason":"legacy workspace registry may still reference project caches"}));
    }
    if apply {
        p.require_private_state()?;
    }
    if crate::workspace::has_live(p)? {
        return Ok(json!({"requested":true, "eligible":false, "purged":false,
            "reason":"managed workspaces are using the project caches"}));
    }
    let mut directories = Vec::new();
    let mut candidates = Vec::new();
    let mut removed = Vec::new();
    let mut blocked = Vec::new();
    let mut retained = Vec::new();
    let mut groups = vec![
        (Kind::Package, p.state.join("cache")),
        (Kind::Layer, p.state.join("layers")),
    ];
    let bases = p.state.join("bases");
    if exists(&bases)? {
        groups.push((Kind::Base, bases));
    }
    match history_cache_root(&p.state) {
        Ok(Some(directory)) => groups.push((Kind::History, directory)),
        Ok(None) => (),
        Err(error) => {
            let path = p.state.join(HISTORY_CACHE_DIR);
            directories.push(path.clone());
            blocked.push(json!({"path":path,"reason":api::error_json(&error)}));
        }
    }
    match layer::registered_install_root(p) {
        Ok(Some(directory)) => groups.push((Kind::RegisteredInstall, directory)),
        Ok(None) => (),
        Err(error) => {
            let path = p.state.join(layer::REGISTERED_INSTALL_DIR);
            directories.push(path.clone());
            blocked.push(json!({"path":path,"reason":api::error_json(&error)}));
        }
    }
    for (kind, directory) in groups {
        directories.push(directory.clone());
        let inspect_container = || {
            match kind {
                Kind::Package | Kind::Layer => p.check_cache_owner(&directory)?,
                Kind::RegisteredInstall => ensure!(
                    layer::registered_install_root(p)?.as_deref() == Some(directory.as_path()),
                    "registered install store changed during purge"
                ),
                Kind::Base => (),
                Kind::History => ensure!(
                    history_cache_root(&p.state)?.as_deref() == Some(directory.as_path()),
                    "Git history cache changed during purge"
                ),
            }
            cache::directory_identity(&directory)
        };
        let container = match inspect_container() {
            Ok(identity) => identity,
            Err(error) => {
                blocked.push(json!({"path":directory,"reason":api::error_json(&error)}));
                continue;
            }
        };
        let check_container = || {
            ensure!(
                inspect_container()? == container,
                "cache container changed during purge"
            );
            Ok(())
        };
        let mut entries = match fs::read_dir(&directory)
            .and_then(|entries| entries.collect::<std::io::Result<Vec<_>>>())
        {
            Ok(entries) => entries,
            Err(error) => {
                blocked.push(json!({"path":directory,"reason":api::error_json(&error.into())}));
                continue;
            }
        };
        entries.sort_by_key(|entry| entry.file_name());
        let marker = match kind {
            Kind::Package | Kind::Layer | Kind::RegisteredInstall => Some(CACHE_OWNER),
            Kind::Base => None,
            Kind::History => Some(HISTORY_CACHE_MARKER),
        };
        for entry in entries {
            let name = entry.file_name();
            if marker.is_some_and(|marker| name == marker) {
                continue;
            }
            let path = entry.path();
            if name
                .to_str()
                .is_some_and(|name| name.starts_with(cache::QUARANTINE_PREFIX))
            {
                let reason = anyhow::anyhow!("earlier cache quarantine retained for inspection");
                retained.push(json!({"path":path,"reason":api::error_json(&reason)}));
                continue;
            }
            let verify = |candidate: &Path| -> Result<Verified> {
                match kind {
                    Kind::Layer => {
                        let digest = name.to_str().context("invalid layer digest")?;
                        Ok(Verified::Layer(layer::verify_layer(candidate, digest)?))
                    }
                    Kind::RegisteredInstall => {
                        let digest = name.to_str().context("invalid registered install digest")?;
                        Ok(Verified::Layer(
                            layer::verify_registered_install_generation(p, candidate, digest)?,
                        ))
                    }
                    Kind::Base => {
                        check_owned_tree(candidate)?;
                        let oid = name.to_str().context("invalid source snapshot commit")?;
                        Ok(Verified::Base(
                            storage::verify_base(candidate, &p.state, oid)
                                .context("source snapshot ownership or contents are unverified")?,
                        ))
                    }
                    Kind::History => {
                        check_owned_tree(candidate)?;
                        Ok(Verified::History(verify_history_cache_generation(
                            candidate, &p.state, &name,
                        )?))
                    }
                    Kind::Package => {
                        let package = name.to_str().context("invalid package cache name")?;
                        let binding = cache::verify_package(p, package, candidate)?;
                        check_owned_tree(candidate)?;
                        Ok(Verified::Package(binding))
                    }
                }
            };
            let result = (|| {
                check_container()?;
                let identity = cache::directory_identity(&path)?;
                let verified = verify(&path)?;
                ensure!(
                    cache::directory_identity(&path)? == identity,
                    "cache directory changed during verification"
                );
                Ok((identity, verified))
            })();
            let (identity, verified) = match result {
                Ok(value) => value,
                Err(error) => {
                    blocked.push(json!({"path":path,"reason":api::error_json(&error)}));
                    continue;
                }
            };
            if apply {
                match cache::purge_directory(&path, &identity, check_container, |moved| {
                    ensure!(
                        verify(moved)? == verified,
                        "cache verification changed during quarantine"
                    );
                    Ok(())
                }) {
                    Ok(()) => removed.push(path.clone()),
                    Err(PurgeFailure::Unmoved(error)) => {
                        blocked.push(json!({"path":path,"reason":api::error_json(&error)}));
                        continue;
                    }
                    Err(PurgeFailure::Retained { path: moved, error }) => {
                        retained.push(json!({"path":moved,"original_path":path,"reason":api::error_json(&error)}));
                        continue;
                    }
                }
            }
            candidates.push(path);
        }
    }
    let eligible = (blocked.is_empty() && retained.is_empty()) || !candidates.is_empty();
    Ok(json!({
        "requested":true, "eligible":eligible, "purged":!removed.is_empty(),
        "directories":directories,"candidates":candidates,"removed":removed,"blocked":blocked,"retained":retained
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn purges_only_original_registered_installs_and_allows_reseeding() -> Result<()> {
        use crate::package_manager::InstallSpec;

        let temporary = tempfile::tempdir()?;
        let repo = temporary.path().join("primary");
        fs::create_dir(&repo)?;
        git::bytes(&repo, &["init", "-q", "--template="])?;
        fs::write(repo.join("fixture.lock"), "locked")?;
        git::bytes(&repo, &["add", "fixture.lock"])?;
        git::bytes(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-qm",
                "base",
                "--no-gpg-sign",
            ],
        )?;
        let (project, _lock) = Project::open(&repo)?;
        let repo = &project.repo;
        let spec = |key: &str| InstallSpec {
            manager: "fixture".into(),
            directory: "vendor/bundle".into(),
            inputs: vec!["fixture.lock".into()],
            key: key.into(),
        };
        let installed = repo.join("vendor/bundle");
        fs::create_dir_all(&installed)?;
        fs::write(installed.join("package"), "installed")?;
        let root = project.state.join(layer::REGISTERED_INSTALL_DIR);
        let first = layer::seed_registered(&project, repo, &spec("valid"))?;
        if first["reason"] == "cow_unavailable" {
            assert_eq!(purge_caches(&project, true)?["purged"], false);
            return Ok(());
        }
        let valid = root.join(
            first["digest"]
                .as_str()
                .context("missing generation digest")?,
        );
        let seed = |key: &str| -> Result<_> {
            let receipt = layer::seed_registered(&project, repo, &spec(key))?;
            Ok(root.join(
                receipt["digest"]
                    .as_str()
                    .context("missing generation digest")?,
            ))
        };

        let altered = seed("altered")?;
        fs::write(altered.join("tree/package"), "keep modified contents")?;
        let extra = seed("extra")?;
        fs::write(extra.join("notes"), "keep extra data")?;
        let replaced = seed("replaced")?;
        let backup = repo.join("original-generation");
        fs::rename(&replaced, &backup)?;
        crate::registry::dir(&replaced)?;
        fs::create_dir(replaced.join("tree"))?;
        fs::copy(backup.join("tree/package"), replaced.join("tree/package"))?;
        fs::copy(
            backup.join("provenance.json"),
            replaced.join("provenance.json"),
        )?;

        // Simulate interruption after persisting the detached binding, before
        // the prepared directory was published at its canonical digest.
        let interrupted = seed("interrupted")?;
        let scratch = root.join(".building-interrupted");
        fs::rename(&interrupted, &scratch)?;
        assert_eq!(seed("interrupted")?, interrupted);
        assert_eq!(
            fs::read_to_string(scratch.join("tree/package"))?,
            "installed"
        );

        let sibling = repo.with_file_name("sibling");
        git::bytes(
            repo,
            &[
                "clone",
                "-q",
                "--no-hardlinks",
                ".",
                git::path_str(&sibling)?,
            ],
        )?;
        assert_eq!(
            layer::attach_registered(&project, &sibling, &spec("replaced"))?["reason"],
            "invalid_generation"
        );
        assert!(!sibling.join("vendor").exists());
        assert!(layer::seed_registered(&project, repo, &spec("replaced")).is_err());

        for apply in [false, true] {
            let result = purge_caches(&project, apply)?;
            for path in [&valid, &interrupted] {
                assert!(
                    result["candidates"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(path)),
                    "{result}"
                );
                assert_eq!(path.exists(), !apply);
            }
            assert_eq!(result["purged"], apply);
            for path in [&altered, &extra, &replaced, &scratch] {
                assert!(
                    result["blocked"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|item| item["path"] == json!(path)),
                    "{result}"
                );
            }
            assert_eq!(
                fs::read_to_string(altered.join("tree/package"))?,
                "keep modified contents"
            );
            assert_eq!(fs::read_to_string(extra.join("notes"))?, "keep extra data");
            assert_eq!(
                fs::read_to_string(replaced.join("tree/package"))?,
                "installed"
            );
            assert_eq!(
                fs::read_to_string(scratch.join("tree/package"))?,
                "installed"
            );
        }
        assert_eq!(fs::read_to_string(installed.join("package"))?, "installed");
        assert_eq!(seed("valid")?, valid);
        assert_eq!(
            layer::attach_registered(&project, &sibling, &spec("valid"))?["status"],
            "attached"
        );
        Ok(())
    }

    #[test]
    fn keeps_foreign_history_cache_namespace_intact() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        git::bytes(temporary.path(), &["init", "-q", "--template="])?;
        let (project, _lock) = Project::open(temporary.path())?;
        let history = project.state.join(HISTORY_CACHE_DIR);
        crate::registry::dir(&history)?;
        let marker = history.join(HISTORY_CACHE_MARKER);
        let unrelated = history.join("saved-data");
        fs::write(&marker, "foreign format")?;
        fs::write(&unrelated, "preserve saved history")?;
        for apply in [false, true] {
            let result = purge_caches(&project, apply)?;
            assert_eq!(result["blocked"][0]["path"], serde_json::json!(history));
            assert_eq!(result["purged"], false);
            assert_eq!(fs::read_to_string(&marker)?, "foreign format");
            assert_eq!(fs::read_to_string(&unrelated)?, "preserve saved history");
        }
        Ok(())
    }

    #[test]
    fn preserves_shared_caches_when_either_legacy_workspace_store_exists() -> Result<()> {
        for legacy in ["records", "independent"] {
            let temporary = tempfile::tempdir()?;
            git::bytes(temporary.path(), &["init", "-q", "--template="])?;
            let (project, _lock) = Project::open(temporary.path())?;
            let old_state = project.state.join(legacy);
            fs::create_dir(&old_state)?;
            let record = old_state.join("retained-data");
            fs::write(&record, "unrecognized workspace state")?;
            let npm = project.state.join("cache/npm");
            fs::create_dir(&npm)?;
            let entry = npm.join("cached");
            fs::write(&entry, "cache in use")?;

            for apply in [false, true] {
                let result = purge_caches(&project, apply)?;
                assert_eq!(result["eligible"], false);
                assert_eq!(result["purged"], false);
                assert_eq!(fs::read_to_string(&entry)?, "cache in use");
                assert_eq!(fs::read_to_string(&record)?, "unrecognized workspace state");
            }
        }
        Ok(())
    }
}

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
    for (kind, directory) in groups {
        directories.push(directory.clone());
        let inspect_container = || {
            match kind {
                Kind::Package | Kind::Layer => p.check_cache_owner(&directory)?,
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
            Kind::Package | Kind::Layer => Some(CACHE_OWNER),
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

//! Garbage collection is opt-in. Time alone cannot make an edit disposable.
use super::retire;
use crate::{
    api, git, layer,
    registry::{CACHE_OWNER, LEASE_SECS, Project, check_owned_tree, now},
    storage,
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs};
use uuid::Uuid;

pub(super) fn forget_lock(p: &Project, name: &str) -> Result<()> {
    let path = p.state.join("locks").join(name);
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn purge_caches(p: &Project, apply: bool) -> Result<Value> {
    let cache = p.state.join("cache");
    let layers = p.state.join("layers");
    let mut candidates = Vec::new();
    let mut removed = Vec::new();
    let mut blocked = Vec::new();
    for (directory, is_layer) in [(&cache, false), (&layers, true)] {
        if let Err(error) = p.check_cache_owner(directory) {
            blocked.push(json!({"path":directory,"reason":api::error_json(&error)}));
            continue;
        }
        let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            if name == CACHE_OWNER {
                continue;
            }
            let path = entry.path();
            let verified = if is_layer {
                layer::verify_layer(&path)
            } else {
                (|| -> Result<()> {
                    // These names match the package cache paths in Runtime::configure.
                    ensure!(
                        matches!(
                            name.to_str(),
                            Some("npm" | "pnpm" | "bun" | "cargo" | "uv" | "sccache")
                        ),
                        "unrecognized package cache root"
                    );
                    check_owned_tree(&path, false)
                })()
            };
            if let Err(error) = verified {
                blocked.push(json!({"path":path,"reason":api::error_json(&error)}));
                continue;
            }
            if apply {
                // Keep the container and its marker through partial purges.
                let result = p
                    .check_cache_owner(directory)
                    .and_then(|()| fs::remove_dir_all(&path).map_err(Into::into));
                if let Err(error) = result {
                    blocked.push(json!({"path":path,"reason":api::error_json(&error)}));
                    continue;
                }
                removed.push(path.clone());
            }
            candidates.push(path);
        }
    }
    let eligible = blocked.is_empty() || !candidates.is_empty();
    Ok(json!({
        "requested":true, "eligible":eligible, "purged":apply && eligible,
        "directories":[cache,layers], "candidates":candidates,"removed":removed,"blocked":blocked
    }))
}

pub(super) fn run(p: &Project, apply: bool, purge_cache: bool) -> Result<Value> {
    let mut candidates = Vec::new();
    let mut removed = Vec::new();
    let mut retained = Vec::new();
    let mut used = BTreeSet::new();
    let records = p.records()?;
    let record_count = records.len();
    for mut r in records {
        if now().saturating_sub(r.heartbeat) < LEASE_SECS {
            used.insert(r.base);
            continue;
        }
        let result = (|| -> Result<()> {
            let lock = p.work_lock(&r.name)?;
            let tip = retire::eligible(p, &r)?;
            candidates.push(r.name.clone());
            if apply {
                retire::remove(p, &mut r, &tip, false, false, None)?;
                drop(lock);
                forget_lock(p, &r.name)?;
                removed.push(r.name.clone());
            }
            Ok(())
        })();
        if let Err(error) = result {
            retained.push(json!({"name":r.name,"reason":api::error_json(&error)}));
            used.insert(r.base);
        }
    }
    // An OID-shaped name alone cannot establish ownership of a directory.
    let mut bases = Vec::new();
    let mut retained_bases = Vec::new();
    for entry in fs::read_dir(p.state.join("bases"))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if git::checked_oid(&name).is_err() || used.contains(&name) {
            continue;
        }
        let path = entry.path();
        let ownership = (|| -> Result<()> {
            ensure!(
                storage::owned_base(&path)?,
                "source snapshot ownership is unverified"
            );
            check_owned_tree(&path, false)
        })();
        if let Err(error) = ownership {
            retained_bases.push(json!({"base":name,"reason":api::error_json(&error)}));
            continue;
        }
        if apply {
            fs::remove_dir_all(path)?;
        }
        bases.push(name);
    }
    bases.sort();
    retained_bases.sort_by(|left, right| left["base"].as_str().cmp(&right["base"].as_str()));
    let mut expired = 0;
    for entry in fs::read_dir(p.state.join("completed"))? {
        let path = entry?.path();
        if path.extension().is_none_or(|s| s != "json") {
            continue;
        }
        let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id) {
            continue;
        }
        if let Some(receipt) = p.receipt(id)? {
            let at = receipt["at"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("invalid receipt date"))?;
            if now().saturating_sub(at) >= crate::registry::RECEIPT_SECS {
                if apply {
                    fs::remove_file(path)?;
                }
                expired += 1;
            }
        }
    }
    let mut cache = json!({"requested":purge_cache,"purged":false});
    if purge_cache {
        // Preview the result after eligible stale workspaces have been removed.
        let remaining = record_count
            - if apply {
                removed.len()
            } else {
                candidates.len()
            };
        if remaining == 0 {
            cache = purge_caches(p, apply)?;
        } else {
            cache["eligible"] = json!(false);
            cache["remaining_workspaces"] = json!(remaining);
        }
    }
    Ok(
        json!({"applied":apply,"candidates":candidates,"removed":removed,"retained":retained,
        "unused_source_snapshots":bases,"retained_source_snapshots":retained_bases,
        "expired_receipts":expired,"cache":cache,"lease_seconds":LEASE_SECS}),
    )
}

//! Garbage collection is opt-in. Time alone cannot make an edit disposable.
use super::retire;
use crate::{
    api,
    registry::{LEASE_SECS, Project, now},
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs};

pub(super) fn forget_lock(p: &Project, name: &str) -> Result<()> {
    let path = p.state.join("locks").join(name);
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
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
    // Only retire full-OID snapshot directories under the owned store. They are
    // safe to unlink after a clone; active records retain theirs for warm spawns.
    let mut bases = Vec::new();
    for entry in fs::read_dir(p.state.join("bases"))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !matches!(name.len(), 40 | 64)
            || !name.bytes().all(|b| b.is_ascii_hexdigit())
            || used.contains(&name)
        {
            continue;
        }
        ensure!(
            entry.file_type()?.is_dir(),
            "unexpected snapshot entry: {}",
            entry.path().display()
        );
        if apply {
            fs::remove_dir_all(entry.path())?;
        }
        bases.push(name);
    }
    bases.sort();
    let mut expired = 0;
    for entry in fs::read_dir(p.state.join("completed"))? {
        let path = entry?.path();
        if path.extension().is_none_or(|s| s != "json") {
            continue;
        }
        let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
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
        cache["eligible"] = json!(remaining == 0);
        if remaining == 0 {
            let directories = [p.state.join("cache"), p.state.join("layers")];
            cache["directories"] = json!(directories);
            if apply {
                for path in directories {
                    fs::remove_dir_all(&path)?;
                    crate::registry::dir(&path)?;
                }
                cache["purged"] = json!(true);
            }
        } else {
            cache["remaining_workspaces"] = json!(remaining);
        }
    }
    Ok(
        json!({"applied":apply,"candidates":candidates,"removed":removed,"retained":retained,
        "unused_source_snapshots":bases,"expired_receipts":expired,"cache":cache,"lease_seconds":LEASE_SECS}),
    )
}

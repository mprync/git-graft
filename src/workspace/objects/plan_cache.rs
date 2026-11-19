//! Restore an exact verified workspace history generation into an empty plan store.

use super::{
    PackLock, graph_count, history, repository_path, sync_directory, sync_pack_file, transfer_git,
};
use crate::recovery::rename_exclusive;
use anyhow::{Result, ensure};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

pub(super) fn restore(
    private: &Path,
    state: &Path,
    format: &str,
    old: &str,
) -> Result<Option<Vec<PackLock>>> {
    let destination = repository_path(private, "objects/pack")?;
    // Existing stores can contain overlapping generations; use Git negotiation
    // there rather than adding a second copy of an existing history.
    if fs::read_dir(&destination)?.next().is_some() {
        return Ok(None);
    }
    let objects = repository_path(private, "objects")?;
    for entry in fs::read_dir(&objects)? {
        let name = entry?.file_name();
        if name != "pack" && name != "info" {
            return Ok(None);
        }
    }
    let stage = tempfile::tempdir_in(&destination)?;
    // Cache failures are misses only before installing anything in the store.
    let Some(packs) = history::restore_exact(state, format, old, stage.path()).unwrap_or(None)
    else {
        return Ok(None);
    };
    ensure!(!packs.is_empty(), "empty cached history");
    let mut locks = Vec::new();
    for (pack, index) in packs {
        let name = pack.file_name().unwrap();
        let target = destination.join(name);
        let keep = target.with_extension("keep");
        let marker = format!("graft plan cache {}\n", std::process::id());
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&keep)?;
        file.write_all(marker.as_bytes())?;
        file.sync_all()?;
        locks.push(PackLock::capture_marker(keep, &marker)?);
        sync_pack_file(&pack)?;
        sync_pack_file(&index)?;
        rename_exclusive(&pack, &target)?;
        rename_exclusive(&index, &target.with_extension("idx"))?;
    }
    sync_directory(&destination)?;
    sync_directory(&objects)?;
    graph_count(transfer_git(private), old)?;
    Ok(Some(locks))
}

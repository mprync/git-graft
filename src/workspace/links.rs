//! Links that follow a shared resource's turn. The configuration names, for
//! each repository the resource serves, a path in its directory; when a
//! `take` is granted, Graft points the link there at the holder's graft of
//! that repository, or at the repository itself when the holder has none.
//! Tools that serve the resource, such as a development stack, name the
//! fixed path and follow the turn without being reconfigured. Archiving a
//! graft points the links that led to it at its repository again, so a link
//! never leads nowhere.
//!
//! A link runs nothing, and the configuration keeps it out of repositories,
//! directories of grafts and `.git`, so it needs no trust.
use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde_json::json;

use crate::{
    api,
    config::{self, Config, Link},
    registry::is_reparse_point,
};

/// Refuse when anything but a link is at the path of one of `links`, before
/// a `take` changes anything: Graft replaces only links.
pub(super) fn require_links(links: &[&Link]) -> Result<()> {
    for link in links {
        match fs::symlink_metadata(&link.path) {
            Ok(meta) if is_reparse_point(&meta) => {}
            Ok(_) => {
                return Err(api::fault(
                    "invalid_config",
                    format!(
                        "{} holds something other than a link, so Graft cannot point it at the holder's \
                         grafts for resource {}. Move it aside, then take the turn again",
                        link.path.display(),
                        link.resource
                    ),
                    json!({"file": link.file, "path": link.path}),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", link.path.display()));
            }
        }
    }
    Ok(())
}

/// The repository `link` serves, as Graft records a graft's primary.
fn repository(link: &Link) -> PathBuf {
    dunce::canonicalize(&link.repository).unwrap_or_else(|_| link.repository.clone())
}

/// Point each of `links` at the first of `grafts`, each a graft's primary
/// and path, whose primary is the link's repository, or else at the
/// repository itself.
pub(super) fn point_all<'a>(
    links: &[&Link],
    grafts: impl Iterator<Item = (&'a Path, &'a Path)> + Clone,
) -> Result<()> {
    for link in links {
        let repository = repository(link);
        let target = grafts
            .clone()
            .find(|(primary, _)| *primary == repository)
            .map_or(repository.clone(), |(_, path)| path.to_owned());
        point(&link.path, &target)?;
    }
    Ok(())
}

/// After the graft at `archived` of the repository at `primary` is archived,
/// point every link that led to it at its repository instead. The links are
/// those of the configuration that keeps the primary's shared turns, and of
/// the primary's own. Best effort: the archive is done whatever happens here.
pub(super) fn point_back(primary: &Path, archived: &Path) {
    let mut dirs = Vec::new();
    if let Ok(Some((dir, _))) = config::shared_turns(primary) {
        dirs.push(dir);
    }
    dirs.push(primary.to_owned());
    for dir in dirs {
        let Ok(config) = Config::read(&dir) else {
            continue;
        };
        for link in &config.links {
            if fs::read_link(&link.path).is_ok_and(|target| target == archived) {
                let _ = point(&link.path, &repository(link));
            }
        }
    }
}

/// Point the link at `path` at `target`, unless it already leads there,
/// creating the directories it is in.
fn point(path: &Path, target: &Path) -> Result<()> {
    if fs::read_link(path).is_ok_and(|current| current == target) {
        return Ok(());
    }
    let parent = path.parent().context("a link needs a directory")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    replace(path, target)
        .with_context(|| format!("point the link {} at {}", path.display(), target.display()))
}

/// Make a link at a temporary name beside `path` and rename it over `path`,
/// so the link is never missing.
#[cfg(unix)]
fn replace(path: &Path, target: &Path) -> Result<()> {
    let name = path.file_name().context("a link needs a name")?;
    let temporary = path.with_file_name(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ));
    std::os::unix::fs::symlink(target, &temporary)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

/// Windows cannot rename a directory link over another, so the old link is
/// removed first.
#[cfg(windows)]
fn replace(path: &Path, target: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if is_reparse_point(&meta) => {
            fs::remove_dir(path).or_else(|_| fs::remove_file(path))?;
        }
        Ok(_) => anyhow::bail!("{} holds something other than a link", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    std::os::windows::fs::symlink_dir(target, path).context(
        "creating a link on Windows needs Developer Mode, or a prompt run as administrator",
    )
}

#[cfg(not(any(unix, windows)))]
fn replace(_: &Path, _: &Path) -> Result<()> {
    anyhow::bail!("links are unsupported on this platform")
}

//! Links that follow a shared resource's turn. For each resource the
//! configuration lists under `links`, Graft keeps a link for each repository
//! at `.graft/links/<resource>/<repository>` beside the configuration. A
//! granted `take` points each at the holder's graft of that repository, or
//! at the repository itself when the holder has none. Tools that serve the
//! resource, such as a development stack, name the fixed path and follow the
//! turn without being reconfigured. Archiving a graft points the links that
//! led to it at its repository again, so a link never leads nowhere.
//!
//! Graft writes only below `.graft/links`, and there replaces only links:
//! anything else on the way refuses the take. The links come with the
//! repository's configuration, so they are written only once trusted, as
//! hooks are.
use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::{
    api,
    config::{self, Config, Link},
    registry::Project,
};

/// Why a link was left unwritten, as the reply's `hint` says.
pub(super) const UNTRUSTED_HINT: &str = "The repository lists this link, so Graft points it only once the user has trusted it. \
     Read it with `git graft hook list`, then run `git graft hook trust` in a terminal";

/// Whether the entry at `path` is a link Graft may replace: a symbolic link,
/// or on Windows a junction, which the standard library counts as one.
/// Other reparse points, such as cloud placeholders, are not links.
fn is_link(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

fn occupied(link: &Link, path: &Path, what: &str) -> anyhow::Error {
    api::fault(
        "invalid_config",
        format!(
            "{} is not {what}, so Graft cannot point the links of resource {} there. \
             Graft keeps only its own links under .graft/links; move it aside, then take the turn again",
            path.display(),
            link.resource
        ),
        json!({"path": path, "resource": link.resource}),
    )
}

/// What is in the way of a link at `path` below `dir`, if anything: the
/// first entry on the way that is not a directory, such as a file or a link,
/// or an entry at `path` that is not a link, with what it should have been.
/// A missing entry is in nobody's way.
fn blocked(dir: &Path, path: &Path) -> Result<Option<(PathBuf, &'static str)>> {
    let below = path.strip_prefix(dir)?;
    let parts: Vec<_> = below.iter().collect();
    let mut at = dir.to_owned();
    for (index, part) in parts.iter().enumerate() {
        at.push(part);
        let last = index + 1 == parts.len();
        let meta = match fs::symlink_metadata(&at) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(anyhow::Error::from(error))
                    .with_context(|| format!("inspect {}", at.display()));
            }
        };
        match (last, meta) {
            (true, meta) if is_link(&meta) => {}
            (false, meta) if meta.file_type().is_dir() => {}
            (true, _) => return Ok(Some((at, "a link"))),
            (false, _) => return Ok(Some((at, "a directory"))),
        }
    }
    Ok(None)
}

/// Refuse, before any link changes, when something Graft did not make is in
/// the way of one of `links`: every directory from the configuration's
/// directory down to the link must be a directory or missing, not a link or
/// a file, and the link itself must be a link or missing.
fn require_links(links: &[&Link]) -> Result<()> {
    for link in links {
        if let Some((at, what)) = blocked(&link.dir, &link.path)? {
            return Err(occupied(link, &at, what));
        }
    }
    Ok(())
}

/// The repository `link` serves, as Graft records a graft's primary.
fn repository(link: &Link) -> PathBuf {
    dunce::canonicalize(&link.repository).unwrap_or_else(|_| link.repository.clone())
}

/// Where each link led before a `take` pointed it, so that a take that
/// fails, or whose `post-take` hook fails, can put every link back.
#[derive(Default)]
pub(super) struct Undo(Vec<(PathBuf, PathBuf, Option<PathBuf>)>);

impl Undo {
    /// Point each link back where it led, and remove one that did not
    /// exist. Best effort: the take is already failing.
    pub(super) fn undo(self) {
        for (dir, path, previous) in self.0.into_iter().rev() {
            let _ = match previous {
                Some(target) => point(&dir, &path, &target),
                None => match blocked(&dir, &path) {
                    Ok(None) => remove_link(&path).map_err(anyhow::Error::from),
                    _ => Ok(()),
                },
            };
        }
    }
}

/// The reply's entry for `link`.
fn entry(link: &Link, status: &str) -> Value {
    json!({"repository": link.key, "path": link.path, "status": status})
}

/// The entries for `links` left unwritten because they need trust.
pub(super) fn skipped(links: &[&Link]) -> Vec<Value> {
    links
        .iter()
        .map(|link| {
            let mut entry = entry(link, "skipped");
            entry["reason"] = json!("untrusted");
            entry["hint"] = json!(UNTRUSTED_HINT);
            entry
        })
        .collect()
}

/// Point each of `links` at the first of `grafts`, each a graft's primary
/// and path, whose primary is the link's repository, or else at the
/// repository itself. Every link is checked first, and if one cannot be
/// pointed, those already pointed are put back, so the links are changed
/// all together or not at all. Returns the reply's entries and what puts
/// them back.
pub(super) fn point_all<'a>(
    links: &[&Link],
    grafts: impl Iterator<Item = (&'a Path, &'a Path)> + Clone,
) -> Result<(Vec<Value>, Undo)> {
    require_links(links)?;
    let mut undo = Undo::default();
    let mut entries = Vec::new();
    for link in links {
        let repository = repository(link);
        let target = grafts
            .clone()
            .find(|(primary, _)| *primary == repository)
            .map_or(repository.clone(), |(_, path)| path.to_owned());
        let previous = fs::read_link(&link.path).ok();
        let status = if previous.as_ref() == Some(&target) {
            "kept"
        } else {
            undo.0.push((link.dir.clone(), link.path.clone(), previous));
            if let Err(error) = point(&link.dir, &link.path, &target) {
                undo.undo();
                return Err(error);
            }
            "pointed"
        };
        let mut entry = entry(link, status);
        entry["target"] = json!(target);
        entries.push(entry);
    }
    Ok((entries, undo))
}

/// After the grafts at `archived` of the repository at `primary` are
/// archived, point every link that led to one of them at its repository
/// instead. The links are those of the project that keeps the primary's
/// turns: the directory above that shares them, or else the primary. That
/// project's lock is held, as a `take` holds it, so the two cannot
/// interleave. The caller must hold no Graft lock, so no lock is ever taken
/// while another is held here. Best effort: the archive is done whatever
/// happens here.
pub(super) fn point_back<'a>(primary: &Path, archived: impl Iterator<Item = &'a PathBuf> + Clone) {
    if archived.clone().next().is_none() {
        return;
    }
    let keeper = match config::shared_turns(primary) {
        Ok(Some((dir, _))) => dir,
        Ok(None) => primary.to_owned(),
        Err(_) => return,
    };
    let Ok(paths) = Project::paths(&keeper) else {
        return;
    };
    if paths.0 != keeper {
        return;
    }
    // A project with no Graft state has never had a turn taken, so Graft
    // has pointed none of its links.
    let Ok(Some((p, _lock))) = Project::open_existing_at(paths) else {
        return;
    };
    let Ok(config) = Config::read(&p.repo) else {
        return;
    };
    for link in &config.links {
        let led = fs::read_link(&link.path)
            .is_ok_and(|target| archived.clone().any(|path| *path == target));
        if led {
            let _ = point(&link.dir, &link.path, &repository(link));
        }
    }
}

/// Point the link at `path` below `dir` at `target`, creating the
/// directories it is in. The way is checked again just before the
/// directories are made and again before the link is written, so a link
/// put on the way since an earlier check is never followed.
fn point(dir: &Path, path: &Path, target: &Path) -> Result<()> {
    let clear = || -> Result<()> {
        match blocked(dir, path)? {
            None => Ok(()),
            Some((at, what)) => Err(api::fault(
                "invalid_config",
                format!(
                    "{} is not {what}, so Graft did not point the link {}. \
                     Graft keeps only its own links under .graft/links; move it aside, then take the turn again",
                    at.display(),
                    path.display()
                ),
                json!({"path": at}),
            )),
        }
    };
    clear()?;
    let parent = path.parent().context("a link needs a directory")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    clear()?;
    replace(path, target)
        .with_context(|| format!("point the link {} at {}", path.display(), target.display()))
}

/// A name beside `path` for a link that is about to replace it.
fn temporary(path: &Path) -> Result<PathBuf> {
    let name = path.file_name().context("a link needs a name")?;
    Ok(path.with_file_name(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    )))
}

fn remove_link(path: &Path) -> io::Result<()> {
    // A directory link on Windows is removed as a directory.
    #[cfg(windows)]
    return fs::remove_dir(path).or_else(|_| fs::remove_file(path));
    #[cfg(not(windows))]
    fs::remove_file(path)
}

/// Make a link at a temporary name beside `path` and rename it over `path`,
/// so the link is never missing.
#[cfg(unix)]
fn replace(path: &Path, target: &Path) -> Result<()> {
    let temporary = temporary(path)?;
    std::os::unix::fs::symlink(target, &temporary)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

/// Windows cannot rename a directory link over another. The new link is
/// made at a temporary name first, so a missing privilege leaves the old
/// one as it was; then the old one is removed and the new one renamed into
/// place, and if that fails the old one is made again.
#[cfg(windows)]
fn replace(path: &Path, target: &Path) -> Result<()> {
    let temporary = temporary(path)?;
    std::os::windows::fs::symlink_dir(target, &temporary).context(
        "creating a link on Windows needs Developer Mode, or a prompt run as administrator",
    )?;
    let previous = fs::read_link(path).ok();
    if fs::symlink_metadata(path).is_ok_and(|meta| is_link(&meta))
        && let Err(error) = remove_link(path)
    {
        let _ = remove_link(&temporary);
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&temporary, path) {
        if let Some(previous) = previous {
            let _ = std::os::windows::fs::symlink_dir(previous, path);
        }
        let _ = remove_link(&temporary);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn replace(_: &Path, _: &Path) -> Result<()> {
    anyhow::bail!("links are unsupported on this platform")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn a_link_on_the_way_is_never_followed_when_a_link_is_written() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let dir = dunce::canonicalize(tmp.path())?;
        let repository = dir.join("app");
        fs::create_dir_all(repository.join("x"))?;
        let path = dir.join(".graft/links/stack/app/x");
        let target = dir.join("elsewhere");
        // Clear, the link and its directories are made.
        point(&dir, &path, &target)?;
        assert_eq!(fs::read_link(&path)?, target);
        // A link put on the way since is refused, and nothing is written
        // where it leads.
        fs::remove_file(&path)?;
        fs::remove_dir(path.parent().context("parent")?)?;
        symlink(&repository, path.parent().context("parent")?)?;
        let error = point(&dir, &path, &target).unwrap_err();
        let json = api::error_json(&error);
        assert_eq!(json["code"], "invalid_config");
        assert_eq!(
            json["details"]["path"],
            json!(dir.join(".graft/links/stack/app"))
        );
        assert!(repository.join("x").is_dir());
        assert!(fs::read_link(repository.join("x")).is_err());
        // Undoing a link through it removes nothing there either.
        Undo(vec![(dir.clone(), path.clone(), None)]).undo();
        assert!(repository.join("x").is_dir());
        // Nor is a link that is not Graft's own replaced by a file.
        fs::remove_file(path.parent().context("parent")?)?;
        fs::create_dir_all(&path)?;
        assert!(point(&dir, &path, &target).is_err());
        assert!(path.is_dir());
        Ok(())
    }
}

//! Project source boundaries onto one history. When landing, verify the
//! fetched commit against the destination's own boundaries before importing it.

use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Seek, Write},
    path::{Path, PathBuf},
    process::Command,
};

use super::{
    PackLock, checked, fetch_objects, graph_count, history, open_regular, output_text,
    repository_path, run, sync_directory, transfer_git,
};
use crate::git;

pub(super) fn is_shallow(repo: &Path) -> Result<bool> {
    match output_text(&run(repo, &["rev-parse", "--is-shallow-repository"])?)? {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => anyhow::bail!("cannot determine Git shallow status"),
    }
}

fn shallow_file(repo: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(repository_path(repo, "shallow")?) {
        Ok(bytes) if !bytes.is_empty() => Ok(Some(bytes)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("read Git shallow boundaries"),
    }
}

pub(super) struct Boundary {
    file: PathBuf,
    cache_key: String,
}

impl Boundary {
    pub(super) fn history(&self) -> history::Boundary<'_> {
        history::Boundary {
            file: &self.file,
            cache_key: &self.cache_key,
        }
    }
}

pub(super) fn initialize(source: &Path, root: &Path, base: &str) -> Result<Option<Boundary>> {
    let Some(original) = shallow_file(source)? else {
        return Ok(None);
    };
    let original = output_text(&original)?;
    let mut boundaries = BTreeSet::new();
    for oid in original.lines() {
        let oid = git::checked_oid(oid)?;
        ensure!(
            oid.len() == base.len(),
            "invalid Git shallow boundary width"
        );
        boundaries.insert(oid.to_ascii_lowercase());
    }
    // Freeze all source boundaries for the commit walk, then keep only the
    // ones encountered from this base. A concurrent deepen cannot change the
    // interpretation of a boundary between selection and the object walk.
    let admin = root.join(".git");
    let mut snapshot = tempfile::NamedTempFile::new_in(&admin)?;
    for oid in &boundaries {
        writeln!(snapshot, "{oid}")?;
    }
    let mut walk = transfer_git(source);
    walk.env("GIT_SHALLOW_FILE", snapshot.path())
        .args(["rev-list", base, "--"]);
    let commits = checked(walk).context("walk the selected shallow Git history")?;
    let mut selected = BTreeSet::new();
    for oid in output_text(&commits)?.lines() {
        if let Some(oid) = boundaries.take(oid) {
            selected.insert(oid);
        }
    }
    if selected.is_empty() {
        return Ok(None);
    }
    let contents = selected.iter().map(|oid| format!("{oid}\n")).collect();
    publish(root, base, contents).map(Some)
}

/// Freeze the boundary used by both the object walk and the new repository.
pub(super) fn publish(root: &Path, base: &str, contents: String) -> Result<Boundary> {
    let admin = root.join(".git");
    let file = admin.join("shallow");
    let mut staged = tempfile::NamedTempFile::new_in(&admin)?;
    staged.write_all(contents.as_bytes())?;
    staged.as_file().sync_all()?;
    staged.persist_noclobber(&file)?;
    sync_directory(&admin)?;
    Ok(Boundary {
        file,
        cache_key: cache_key(base, &contents),
    })
}

pub(super) fn cache_key(base: &str, contents: &str) -> String {
    let digest = blake3::hash(contents.as_bytes());
    format!("{base}-shallow-{digest}")
}

pub(super) struct Quarantine {
    objects: PathBuf,
    alternate: String,
    boundary: Option<PathBuf>,
}

impl Quarantine {
    fn new(primary: &Path, temp: &Path, boundaries: Option<&[u8]>) -> Result<Self> {
        let objects = temp.join("objects");
        fs::create_dir_all(objects.join("pack"))?;
        let primary_objects = repository_path(primary, "objects")?;
        // Git accepts a C-quoted path here, including path-list separators and
        // Windows drive letters. This alternate exists only in child processes.
        let path = git::path_str(&primary_objects)?
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r");
        let boundary = boundaries
            .map(|contents| -> Result<PathBuf> {
                let path = temp.join("shallow");
                fs::write(&path, contents)?;
                Ok(path)
            })
            .transpose()?;
        Ok(Self {
            objects,
            alternate: format!("\"{path}\""),
            boundary,
        })
    }

    pub(super) fn command(&self, primary: &Path) -> Command {
        let mut command = transfer_git(primary);
        command
            .env("GIT_OBJECT_DIRECTORY", &self.objects)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &self.alternate)
            .env(
                "GIT_SHALLOW_FILE",
                self.boundary.as_deref().unwrap_or(Path::new("")),
            );
        command
    }

    pub(super) fn pack_dir(&self) -> PathBuf {
        self.objects.join("pack")
    }
}

pub(super) fn fetch(primary: &Path, workspace: &Path, tip: &str) -> Result<Vec<PackLock>> {
    let temp = tempfile::tempdir().context("stage shallow Git object transfer")?;
    let original = shallow_file(primary)?;
    let quarantine = Quarantine::new(primary, temp.path(), original.as_deref())?;
    let _staged_locks = fetch_objects(primary, workspace, tip, Some(&quarantine))
        .context("fetch workspace history into local staging")?;
    verify(&quarantine, primary, tip)?;
    ensure!(
        shallow_file(primary)? == original,
        "destination Git shallow boundaries changed during import; retry land"
    );

    // Let Git install and protect the verified packs against concurrent GC.
    // No destination objects have been written until connectivity is proven.
    let mut locks = Vec::new();
    for entry in fs::read_dir(quarantine.pack_dir())? {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "pack") {
            continue;
        }
        let marker = format!("git-graft shallow import {}", uuid::Uuid::new_v4());
        let mut install = transfer_git(primary);
        install
            .args(["index-pack", "--stdin"])
            .arg(format!("--keep={marker}"))
            .stdin(open_regular(&path)?);
        let output = checked(install).context("install verified shallow Git pack")?;
        if let Some(hash) = output_text(&output)?.strip_prefix("keep\t") {
            git::checked_oid(hash)?;
            ensure!(hash.len() == tip.len(), "invalid Git pack hash width");
            let path = repository_path(primary, &format!("objects/pack/pack-{hash}.keep"))?;
            if let Ok(lock) = PackLock::capture_marker(path, &format!("{marker}\n")) {
                locks.push(lock);
            }
        }
    }
    Ok(locks)
}

fn verify(quarantine: &Quarantine, primary: &Path, tip: &str) -> Result<u64> {
    graph_count(quarantine.command(primary), tip).context(
        "cannot land shallow workspace because its ancestry cannot be resolved in the destination",
    )
}

pub(super) fn import_plan_inputs(
    private: &Path,
    primary: &Path,
    workspace: &Path,
    old: &str,
    tip: &str,
) -> Result<()> {
    let temp = tempfile::tempdir().context("stage shallow Git planning inputs")?;
    let original = shallow_file(private)?;
    let quarantine = Quarantine::new(primary, temp.path(), original.as_deref())?;
    let _locks = fetch_objects(primary, workspace, tip, Some(&quarantine))
        .context("fetch workspace history into local planning staging")?;
    let old_count = verify(&quarantine, primary, old)?;
    let tip_count = verify(&quarantine, primary, tip)?;
    ensure!(
        shallow_file(primary)? == original,
        "destination Git shallow boundaries changed during import; retry land"
    );

    // Only the two requested closures pass from the combined view into
    // the private plan. The pack stands alone after the temporary alternate
    // disappears, including ancestry the primary supplied for a shallow tip.
    let objects = repository_path(private, "objects")?;
    let packs = objects.join("pack");
    let mut roots = tempfile::tempfile_in(temp.path())?;
    writeln!(roots, "{old}\n{tip}")?;
    roots.rewind()?;
    let mut export = quarantine.command(primary);
    export
        .args(["pack-objects", "--quiet", "--revs", "--no-use-bitmap-index"])
        .arg(packs.join("pack"))
        .stdin(roots);
    let output = checked(export).context("export shallow Git planning inputs")?;
    let hash = git::checked_oid(output_text(&output)?)?;
    ensure!(hash.len() == tip.len(), "invalid Git pack hash width");
    let pack = packs.join(format!("pack-{hash}.pack"));
    let mut index = transfer_git(private);
    index.args(["index-pack", "--verify"]).arg(&pack);
    checked(index).context("verify shallow Git planning pack")?;
    ensure!(
        graph_count(transfer_git(private), old)? == old_count
            && graph_count(transfer_git(private), tip)? == tip_count,
        "private Git plan is missing part of the requested history"
    );
    sync_directory(&packs)?;
    sync_directory(&objects)?;
    Ok(())
}

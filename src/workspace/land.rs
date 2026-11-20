//! Land immutable objects from a private repository into the primary repository.
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use crate::{
    api::Land,
    git,
    landing::{self, Plan},
    recovery,
    registry::{self, Project},
    undo,
};

use super::objects;
mod scratch;

const STORE_OWNER: &str = "graft-object-store";

fn namespace(id: &str) -> String {
    format!("refs/graft/workspaces/{}", blake3::hash(id.as_bytes()))
}

fn plan_key(plan: &Plan) -> blake3::Hash {
    let mut key = blake3::Hasher::new();
    for field in [&plan.target, &plan.old, &plan.new, &plan.tip] {
        key.update(field.as_bytes());
        key.update(b"\0");
    }
    key.finalize()
}

fn plan_pins(plan: &Plan) -> [(String, &str); 3] {
    let prefix = format!("refs/graft/land/{}", plan_key(plan));
    [
        (format!("{prefix}/old"), &plan.old),
        (format!("{prefix}/new"), &plan.new),
        (format!("{prefix}/tip"), &plan.tip),
    ]
}

fn stores(p: &Project, create: bool) -> Result<PathBuf> {
    p.require_private_state()?;
    let root = p.state.join("land-plans");
    super::registry::marked_dir(&root, p.id(), create)?;
    Ok(root)
}

fn shallow_boundary(admin: &Path) -> Result<Vec<u8>> {
    let path = admin.join("shallow");
    if registry::exists(&path)? {
        return registry::regular(&path);
    }
    Ok(Vec::new())
}

fn store_name(object_format: &str, shallow: &[u8]) -> String {
    if shallow.is_empty() {
        format!("{object_format}-full")
    } else {
        format!("{object_format}-shallow-{}", blake3::hash(shallow))
    }
}

fn verified_store(p: &Project, root: &Path, name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "invalid private landing object store name"
    );
    let path = root.join(name);
    registry::owned_directory(&path)?;
    registry::owned_directory(&path.join("objects"))?;
    registry::owned_directory(&path.join("objects/info"))?;
    ensure!(
        registry::regular(&path.join(STORE_OWNER))? == p.id().as_bytes()
            && !registry::exists(&path.join("commondir"))?
            && !registry::exists(&path.join("objects/info/alternates"))?
            && git::absolute_git_dir(&path)? == path
            && git::text(&path, &["rev-parse", "--is-bare-repository"])? == "true",
        "private landing object store ownership is unverified"
    );
    let format = git::text(&path, &["rev-parse", "--show-object-format"])?;
    ensure!(
        store_name(&format, &shallow_boundary(&path)?) == name,
        "private landing object store history boundary changed"
    );
    Ok(path)
}

fn open_store(p: &Project, root: &Path) -> Result<(String, PathBuf, bool)> {
    let format = git::text(&p.repo, &["rev-parse", "--show-object-format"])?;
    ensure!(
        matches!(format.as_str(), "sha1" | "sha256"),
        "unsupported Git object format"
    );
    let shallow = shallow_boundary(&p.common)?;
    // Sharing one object database per boundary allows normal Git negotiation
    // and packing to reuse objects between independent plans.
    let name = store_name(&format, &shallow);
    let path = root.join(&name);
    if !registry::exists(&path)? {
        let stage = tempfile::Builder::new()
            .prefix(".pending-")
            .tempdir_in(root)?;
        scratch::mark(p, stage.path())?;
        let stage = stage.keep();
        git::bytes(
            &stage,
            &[
                "-c",
                "init.defaultRefFormat=files",
                "init",
                "--bare",
                "--quiet",
                "--template=",
                &format!("--object-format={format}"),
            ],
        )?;
        registry::atomic(&stage.join(STORE_OWNER), p.id().as_bytes())?;
        if !shallow.is_empty() {
            registry::atomic(&stage.join("shallow"), &shallow)?;
        }
        for file in ["HEAD", "config"] {
            OpenOptions::new()
                .write(true)
                .open(stage.join(file))?
                .sync_all()?;
        }
        super::registry::sync_directory(&stage)?;
        scratch::checkpoint(p, &stage)?;
        return Ok((name, stage, true));
    }
    let path = verified_store(p, root, &name)?;
    Ok((name, path, false))
}

fn sync_pin_directories(store: &Path, reference: &str) -> Result<()> {
    let path = store.join(reference);
    for parent in path
        .ancestors()
        .skip(1)
        .take_while(|parent| parent.starts_with(store))
    {
        // The loose ref may already be packed by native Git GC.
        if registry::exists(parent)? {
            super::registry::sync_directory(parent)?;
        }
    }
    Ok(())
}

fn sync_objects(store: &Path) -> Result<()> {
    let object_dir = store.join("objects");
    for child in fs::read_dir(&object_dir)? {
        let child = child?;
        if child.file_type()?.is_dir() {
            super::registry::sync_directory(&child.path())?;
        }
    }
    super::registry::sync_directory(&object_dir)
}

fn create_plan_pins(store: &Path, plan: &Plan) -> Result<()> {
    sync_objects(store)?;
    for (reference, oid) in plan_pins(plan) {
        match recovery::direct_ref(store, &reference)? {
            Some(existing) => ensure!(existing == oid, "private landing pin disagrees with plan"),
            None => git::create_private_ref(store, &reference, oid)?,
        }
        sync_pin_directories(store, &reference)?;
    }
    Ok(())
}

fn plan_receipt(root: &Path, plan: &Plan) -> PathBuf {
    root.join(format!("{}.plan", plan_key(plan)))
}

fn store_for_plan(p: &Project, plan: &Plan) -> Result<PathBuf> {
    let root = stores(p, false)?;
    let name = String::from_utf8(registry::regular(&plan_receipt(&root, plan))?)?;
    let store = verified_store(p, &root, &name)?;
    for (reference, oid) in plan_pins(plan) {
        ensure!(
            recovery::direct_ref(&store, &reference)?.as_deref() == Some(oid),
            "private landing pin is missing or changed at {reference}"
        );
    }
    Ok(store)
}

/// Plan and durably pin both inputs and the result inside the owned bare store.
pub(crate) fn prepare(
    p: &Project,
    workspace: &Path,
    options: &Land,
    installations: &[&str],
) -> Result<Plan> {
    let root = stores(p, true)?;
    scratch::recover(p, &root)?;
    let (name, store, stage) = open_store(p, &root)?;
    // Merge-tree can write conflict objects even when planning fails. Keep
    // those writes separate from the checkpointed first store.
    let planning = if stage {
        let temp = tempfile::Builder::new()
            .prefix(".planning-")
            .tempdir_in(&root)?;
        let format = git::text(&store, &["rev-parse", "--show-object-format"])?;
        git::bytes(
            temp.path(),
            &[
                "init",
                "--bare",
                "--quiet",
                "--template=",
                &format!("--object-format={format}"),
            ],
        )?;
        // A C-quoted entry keeps this single absolute path from naming another alternate.
        let mut alternate = String::from("\"");
        for ch in git::path_str(&store.join("objects"))?.chars() {
            match ch {
                '\\' => alternate.push_str(r"\\"),
                '"' => alternate.push_str(r#"\""#),
                ch if ch.is_ascii_control() => alternate.push_str(&format!("\\{:03o}", ch as u32)),
                ch => alternate.push(ch),
            }
        }
        alternate.push_str("\"\n");
        registry::atomic(
            &temp.path().join("objects/info/alternates"),
            alternate.as_bytes(),
        )?;
        if registry::exists(&store.join("shallow"))? {
            registry::atomic(
                &temp.path().join("shallow"),
                &registry::regular(&store.join("shallow"))?,
            )?;
        }
        Some(temp)
    } else {
        None
    };
    let planning_store = planning
        .as_ref()
        .map_or(store.as_path(), |temp| temp.path());
    let prefix = format!("refs/graft/import/{}", uuid::Uuid::new_v4());
    let mut imported = None;
    let mut base = None;
    let mut tip = None;
    let result = landing::prepare_with_import(
        &p.repo,
        workspace,
        &p.state,
        options,
        planning_store,
        installations,
        |old, workspace_tip| {
            base = Some(old.to_owned());
            tip = Some(workspace_tip.to_owned());
            imported = Some(objects::import_plan_inputs(
                &store,
                &p.repo,
                workspace,
                &p.state,
                stage.then_some(prefix.as_str()),
                old,
                workspace_tip,
            )?);
            if stage {
                scratch::checkpoint(p, &store)?;
            }
            Ok(())
        },
    );
    let mut result_locks = None;
    let result = result.and_then(|plan| {
        if stage {
            result_locks = Some(objects::transfer_result(&store, planning_store, &plan.new)?);
        }
        create_plan_pins(&store, &plan)?;
        Ok(plan)
    });
    let plan = match result {
        Ok(plan) => plan,
        Err(error) => {
            if stage {
                // The scratch must match its checkpoint, so keep the `.keep`
                // files but close them: Windows cannot move a directory while a
                // file inside it is open.
                for lock in imported.iter_mut().chain(result_locks.iter_mut()).flatten() {
                    lock.close();
                }
                scratch::purge(p, &root, &store)
                    .context("retain unverified private landing scratch")?;
            }
            return Err(error);
        }
    };
    if stage && let Some(old) = &base {
        objects::remove_import_pin(&store, &format!("{prefix}/base"), old)?;
    }
    if stage && let Some(tip) = &tip {
        objects::remove_import_pin(&store, &format!("{prefix}/tip"), tip)?;
    }
    drop(imported);
    drop(result_locks);
    if stage {
        recovery::rename_exclusive(&store, &root.join(&name))?;
        super::registry::sync_directory(&root)?;
        verified_store(p, &root, &name)?;
    }
    let receipt = plan_receipt(&root, &plan);
    if registry::exists(&receipt)? {
        store_for_plan(p, &plan)?;
    } else {
        let mut file = tempfile::NamedTempFile::new_in(&root)?;
        file.write_all(name.as_bytes())?;
        file.as_file().sync_all()?;
        let staged = file.into_temp_path().keep()?;
        recovery::rename_exclusive(&staged, &receipt)?;
        super::registry::sync_directory(&root)?;
    }
    Ok(plan)
}

fn head_points_at(repo: &Path, head: &str, reference: &str) -> Result<bool> {
    let output = git::raw(repo, &["symbolic-ref", "--quiet", head])?;
    match output.status.code() {
        Some(0) => {
            let branch = std::str::from_utf8(&output.stdout)?.trim_end_matches(['\n', '\r']);
            ensure!(
                branch.starts_with("refs/heads/"),
                "cannot verify Git checkout branch for {head}"
            );
            Ok(branch.eq_ignore_ascii_case(reference))
        }
        Some(1) => {
            // A missing or malformed HEAD must not be mistaken for a detached commit.
            git::oid(repo, head).with_context(|| format!("cannot resolve detached Git {head}"))?;
            Ok(false)
        }
        _ => bail!(
            "cannot inspect Git {head}: {}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        ),
    }
}

fn destination(p: &Project, reference: &str, owns_head_lock: bool) -> Result<Option<PathBuf>> {
    let admin = git::absolute_git_dir(&p.repo)?;
    ensure!(admin == p.common, "primary Git directory changed");
    landing::check_admin_branch_operations(&admin, reference)?;
    let bare = git::text(&p.repo, &["rev-parse", "--is-bare-repository"])?;
    let primary_checkout = match bare.as_str() {
        "true" => false,
        "false" => {
            ensure!(
                owns_head_lock || !registry::exists(&admin.join("HEAD.lock"))?,
                "primary checkout HEAD is locked"
            );
            head_points_at(&p.repo, "HEAD", reference)?
        }
        _ => bail!("cannot determine whether the primary Git repository is bare"),
    };

    // Native worktrees share the primary refs even though Graft workspaces
    // do not. Inspect their per-worktree HEADs without invoking git worktree.
    let worktrees = admin.join("worktrees");
    let mut linked_destination = None;
    if registry::exists(&worktrees)? {
        registry::owned_directory(&worktrees)?;
        for entry in fs::read_dir(worktrees)? {
            let entry = entry?;
            let path = entry.path();
            registry::owned_directory(&path)
                .with_context(|| format!("cannot verify linked Git admin at {}", path.display()))?;
            let id = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF-8 linked Git admin at {}", path.display()))?;
            ensure!(
                !registry::exists(&path.join("HEAD.lock"))?,
                "linked worktree HEAD is locked: {id}"
            );
            landing::check_admin_branch_operations(&path, reference)?;
            if head_points_at(&p.repo, &format!("worktrees/{id}/HEAD"), reference)? {
                ensure!(
                    !primary_checkout && linked_destination.is_none(),
                    "destination is checked out in multiple worktrees: {reference}"
                );
                linked_destination = Some((id, registry::exists(&path.join("locked"))?));
            }
        }
    }
    if let Some((id, locked)) = linked_destination {
        ensure!(!locked, "destination worktree is locked: {id}");
        bail!("destination is checked out in an external linked worktree: {id}");
    }
    Ok(primary_checkout.then(|| p.repo.clone()))
}

/// Advance the primary checkout or conditionally update an unchecked ref.
pub(crate) fn apply(p: &Project, plan: &Plan) -> Result<()> {
    apply_result(p, plan, &store_for_plan(p, plan)?)
}

fn apply_result(p: &Project, plan: &Plan, store: &Path) -> Result<()> {
    let mut keep = None;
    let result = landing::apply_with_destination(
        &p.repo,
        plan,
        store,
        |reference, owns_head_lock| destination(p, reference, owns_head_lock),
        || {
            keep = Some(objects::transfer_result(&p.repo, store, &plan.new)?);
            Ok(())
        },
    );
    drop(keep);
    result
}

/// Append and pin the inverse of one completed landing for deterministic retry.
pub(crate) fn undo(p: &Project, plan: &Plan, id: &str) -> Result<Value> {
    let store = store_for_plan(p, plan)?;
    let reference = format!("{}/undo/{}", namespace(id), plan_key(plan));
    undo::reverse(&p.repo, &store, plan, &reference, |plan| {
        sync_objects(&store)?;
        sync_pin_directories(&store, &reference)?;
        apply_result(p, plan, &store)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Fault;
    use serde_json::json;
    use std::fs::File;

    fn config(repo: &Path) -> Result<()> {
        for (key, value) in [
            ("user.name", "Independent Landing Fixture"),
            ("user.email", "test@example.invalid"),
            ("core.autocrlf", "false"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "test-hooks"),
        ] {
            git::bytes(repo, &["config", key, value])?;
        }
        Ok(())
    }

    struct Fixture {
        root: tempfile::TempDir,
        p: Project,
        _lock: File,
        workspace: PathBuf,
        base: String,
        options: Land,
    }

    impl Fixture {
        fn options() -> Land {
            Land {
                name: "task".into(),
                lease: "test".into(),
                into: "main".into(),
                squash: false,
                commit: None,
                discard_ignored: false,
                reconcile: false,
            }
        }

        fn new() -> Result<Self> {
            let root = tempfile::tempdir()?;
            let primary = root.path().join("primary");
            let workspace = root.path().join("private checkout");
            fs::create_dir(&primary)?;
            git::bytes(&primary, &["init", "-b", "main"])?;
            config(&primary)?;
            fs::write(primary.join("base.txt"), "base\n")?;
            git::bytes(&primary, &["add", "base.txt"])?;
            git::bytes(&primary, &["commit", "-m", "base"])?;
            let base = git::oid(&primary, "HEAD")?;
            git::bytes(
                root.path(),
                &[
                    "clone",
                    "--no-local",
                    "--template=",
                    "-c",
                    "core.autocrlf=false",
                    "-c",
                    "core.hooksPath=test-hooks",
                    git::path_str(&primary)?,
                    git::path_str(&workspace)?,
                ],
            )?;
            config(&workspace)?;
            git::bytes(
                &primary,
                &[
                    "config",
                    "core.hooksPath",
                    git::path_str(&root.path().join("hooks"))?,
                ],
            )?;
            let (p, lock) = Project::open(&primary)?;
            Ok(Self {
                root,
                p,
                _lock: lock,
                workspace,
                base,
                options: Self::options(),
            })
        }

        fn plan(&self) -> Result<Plan> {
            prepare(&self.p, &self.workspace, &self.options, &[])
        }

        fn plan_squash(&self, p: &Project, contents: &str) -> Result<Plan> {
            fs::write(self.workspace.join("changes"), contents)?;
            let options = Land {
                squash: true,
                commit: Some("Commit private changes".into()),
                ..Self::options()
            };
            prepare(p, &self.workspace, &options, &[])
        }

        fn sync_base(&self, oid: &str) {
            run(
                &self.workspace,
                &[
                    "fetch",
                    "--no-tags",
                    "--no-write-fetch-head",
                    git::path_str(&self.p.repo).unwrap(),
                    oid,
                ],
            );
            run(&self.workspace, &["merge", "--ff-only", oid]);
        }
    }

    #[test]
    fn incomplete_first_store_is_not_published_or_adopted() -> Result<()> {
        let fixture = Fixture::new()?;
        let root = stores(&fixture.p, true)?;
        let (name, staged, stage) = open_store(&fixture.p, &root)?;
        assert!(stage);
        assert!(!root.join(&name).exists());
        let plan = fixture.plan()?;
        assert_eq!(plan.old, fixture.base);
        assert!(!staged.exists());
        assert!(root.join(name).exists());
        Ok(())
    }

    #[test]
    fn interrupted_store_with_foreign_file_is_preserved() -> Result<()> {
        let fixture = Fixture::new()?;
        let root = stores(&fixture.p, true)?;
        let (_, staged, _) = open_store(&fixture.p, &root)?;
        fs::write(staged.join("foreign"), b"preserve")?;
        fixture.plan()?;
        assert_eq!(fs::read(staged.join("foreign"))?, b"preserve");
        Ok(())
    }

    #[test]
    fn replaced_scratch_file_is_preserved_while_landing_continues() -> Result<()> {
        let fixture = Fixture::new()?;
        let root = stores(&fixture.p, true)?;
        let (_, staged, _) = open_store(&fixture.p, &root)?;
        let config = staged.join("config");
        let original = fs::read(&config)?;
        recovery::rename_exclusive(&config, &root.join("saved-config"))?;
        fs::write(&config, &original)?;
        fixture.plan()?;
        assert_eq!(fs::read(&config)?, original);
        Ok(())
    }

    #[test]
    fn interrupted_pack_install_is_retained_and_does_not_block_retry() -> Result<()> {
        let fixture = Fixture::new()?;
        let root = stores(&fixture.p, true)?;
        let (_, staged, _) = open_store(&fixture.p, &root)?;
        let partial =
            staged.join("objects/pack/pack-1111111111111111111111111111111111111111.pack");
        fs::write(&partial, b"partial pack")?;
        fixture.plan()?;
        assert_eq!(fs::read(partial)?, b"partial pack");
        Ok(())
    }

    #[test]
    fn torn_marker_and_foreign_planning_scratch_do_not_block_landing() -> Result<()> {
        let fixture = Fixture::new()?;
        let root = stores(&fixture.p, true)?;
        let (_, staged, _) = open_store(&fixture.p, &root)?;
        fs::write(staged.join(".graft-land-pending"), b"torn")?;
        let foreign = root.join(".planning-foreign");
        fs::create_dir(&foreign)?;
        fs::write(foreign.join("data"), b"preserve")?;
        fixture.plan()?;
        assert_eq!(fs::read(staged.join(".graft-land-pending"))?, b"torn");
        assert_eq!(fs::read(foreign.join("data"))?, b"preserve");
        Ok(())
    }

    #[test]
    fn keeps_multiple_plans_pinned_through_gc_and_undoes_same_named_branch() -> Result<()> {
        let fixture = Fixture::new()?;
        write(
            &fixture.workspace,
            "transient.txt",
            "private intermediate content",
        );
        let transient = commit(&fixture.workspace, "private intermediate commit");
        let secret = run(&fixture.workspace, &["rev-parse", "HEAD:transient.txt"]);
        fs::remove_file(fixture.workspace.join("transient.txt"))?;
        let first = fixture.plan_squash(&fixture.p, "first change")?;
        let second = fixture.plan_squash(&fixture.p, "second change")?;
        let primary = &fixture.p.repo;
        let private = store_for_plan(&fixture.p, &first)?;
        assert_eq!(private, store_for_plan(&fixture.p, &second)?);
        assert_eq!(git::oid(primary, "HEAD")?, fixture.base);
        assert_ne!(first.new, first.tip);
        assert_ne!(first.new, second.new);
        assert_eq!(git::oid(&fixture.workspace, "HEAD")?, second.tip);
        git::bytes(primary, &["gc", "--prune=now"])?;
        git::bytes(&private, &["gc", "--prune=now"])?;
        let pinned = git::text(
            &private,
            &["for-each-ref", "--format=%(objectname)", "refs/graft/land"],
        )?;
        assert_eq!(pinned.lines().count(), 6);
        for oid in [&first.old, &first.new, &first.tip, &second.new, &second.tip] {
            assert!(pinned.lines().any(|entry| entry == oid));
            assert_eq!(git::oid(&private, oid)?, *oid);
        }
        assert!(!has_object(primary, &first.new)?);

        apply(&fixture.p, &first)?;
        assert_eq!(git::oid(primary, "HEAD")?, first.new);
        assert_eq!(fs::read_to_string(primary.join("changes"))?, "first change");
        assert!(apply(&fixture.p, &second).is_err());
        for oid in [&transient, &secret, &first.tip, &second.new, &second.tip] {
            assert!(
                !has_object(primary, oid)?,
                "unexpected primary object {oid}"
            );
        }
        let advertised = git::text(primary, &["ls-remote", git::path_str(primary)?])?;
        assert_eq!(
            advertised,
            format!("{}\tHEAD\n{}\trefs/heads/main", first.new, first.new)
        );
        assert_eq!(
            git::text(primary, &["for-each-ref", "--format=%(refname)", "refs/"])?,
            "refs/heads/main"
        );
        let mirror = fixture.root.path().join("mirror.git");
        fs::create_dir(&mirror)?;
        git::bytes(&mirror, &["init", "--bare", "--template="])?;
        git::bytes(primary, &["push", "--mirror", git::path_str(&mirror)?])?;
        assert_eq!(
            git::text(&mirror, &["for-each-ref", "--format=%(refname)"])?,
            "refs/heads/main"
        );
        assert!(!has_object(&mirror, &secret)?);

        git::bytes(primary, &["gc", "--prune=now"])?;
        let receipt = undo(&fixture.p, &first, "same-opaque-id")?;
        let undone = git::oid(primary, "HEAD")?;
        assert_eq!(receipt["commit"], undone);
        assert_eq!(git::oid(primary, "HEAD^")?, first.new);
        assert!(!primary.join("changes").exists());
        git::bytes(primary, &["gc", "--prune=now"])?;
        git::bytes(&private, &["gc", "--prune=now"])?;
        assert_eq!(undo(&fixture.p, &first, "same-opaque-id")?, receipt);
        assert!(!has_object(primary, &first.tip)?);
        assert!(!has_object(primary, &secret)?);
        assert_eq!(git::oid(&fixture.workspace, "HEAD")?, second.tip);
        assert_eq!(
            fs::read_to_string(fixture.workspace.join("changes"))?,
            "second change"
        );
        Ok(())
    }

    fn has_object(repo: &Path, oid: &str) -> Result<bool> {
        Ok(git::raw(repo, &["cat-file", "-e", oid])?.status.success())
    }

    #[test]
    fn protects_native_worktree_state_even_for_a_bare_primary() -> Result<()> {
        let fixture = Fixture::new()?;
        let bare = fixture.root.path().join("bare.git");
        let external = fixture.root.path().join("external checkout");
        let duplicate = fixture.root.path().join("other external checkout");
        git::bytes(
            fixture.root.path(),
            &[
                "clone",
                "--bare",
                git::path_str(&fixture.p.repo)?,
                git::path_str(&bare)?,
            ],
        )?;
        config(&bare)?;
        let (p, _lock) = Project::open(&bare)?;
        let plan = fixture.plan_squash(&p, "bare primary landing")?;
        git::bytes(
            &bare,
            &["worktree", "add", git::path_str(&external)?, "main"],
        )?;
        let error = apply(&p, &plan).unwrap_err();
        assert!(error.to_string().contains("external linked worktree"));

        git::bytes(
            &bare,
            &[
                "worktree",
                "add",
                "--force",
                git::path_str(&duplicate)?,
                "main",
            ],
        )?;
        let error = apply(&p, &plan).unwrap_err();
        assert!(error.to_string().contains("multiple worktrees"));
        git::bytes(&bare, &["worktree", "remove", git::path_str(&duplicate)?])?;

        git::bytes(&bare, &["worktree", "lock", git::path_str(&external)?])?;
        let error = apply(&p, &plan).unwrap_err();
        assert!(error.to_string().contains("locked"));
        git::bytes(&bare, &["worktree", "unlock", git::path_str(&external)?])?;

        git::bytes(&external, &["checkout", "--detach"])?;
        let admin = PathBuf::from(git::text(&external, &["rev-parse", "--absolute-git-dir"])?);
        let rebase = admin.join("rebase-merge");
        fs::create_dir(&rebase)?;
        fs::write(rebase.join("head-name"), "refs/heads/main\n")?;
        let error = apply(&p, &plan).unwrap_err();
        assert!(error.to_string().contains("unfinished Git operation"));
        assert_eq!(git::oid(&bare, "refs/heads/main")?, plan.old);

        fs::remove_file(rebase.join("head-name"))?;
        fs::remove_dir(rebase)?;
        apply(&p, &plan)?;
        assert_eq!(git::oid(&bare, "refs/heads/main")?, plan.new);
        assert_eq!(git::oid(&external, "HEAD")?, plan.old);
        assert!(!external.join("changes").exists());
        Ok(())
    }

    fn run(path: &Path, args: &[&str]) -> String {
        git::text(path, args).unwrap_or_else(|error| panic!("git {args:?}: {error:#}"))
    }

    fn commit(path: &Path, message: &str) -> String {
        run(path, &["add", "-A"]);
        run(path, &["commit", "-m", message]);
        run(path, &["rev-parse", "HEAD"])
    }

    fn write(path: &Path, file: &str, value: &str) {
        fs::write(path.join(file), value).unwrap();
    }

    #[test]
    fn applies_fast_forward_and_retries_as_noop() {
        let fixture = Fixture::new().unwrap();
        let no_change = fixture.plan().unwrap();
        assert_eq!(no_change.old, no_change.new);
        apply(&fixture.p, &no_change).unwrap();

        write(&fixture.workspace, "feature.txt", "feature\n");
        let tip = commit(&fixture.workspace, "feature");
        let plan = fixture.plan().unwrap();
        assert_eq!(plan.old, fixture.base);
        assert_eq!(plan.new, tip);
        assert_eq!(plan.tip, tip);
        assert!(!fixture.p.repo.join("feature.txt").exists());
        apply(&fixture.p, &plan).unwrap();
        assert_eq!(
            fs::read_to_string(fixture.p.repo.join("feature.txt")).unwrap(),
            "feature\n"
        );
        assert_eq!(run(&fixture.p.repo, &["rev-parse", "HEAD"]), tip);
        apply(&fixture.p, &plan).unwrap();

        write(&fixture.p.repo, "later.txt", "later\n");
        let later = commit(&fixture.p.repo, "later");
        apply(&fixture.p, &plan).unwrap();
        assert_eq!(run(&fixture.p.repo, &["rev-parse", "HEAD"]), later);
    }

    #[test]
    fn creates_real_merge_or_squash_and_resumes_persisted_plan() {
        for squash in [false, true] {
            let mut fixture = Fixture::new().unwrap();
            fixture.options.squash = squash;
            write(&fixture.p.repo, "ours.txt", "ours\n");
            let old = commit(&fixture.p.repo, "ours");
            write(&fixture.workspace, "theirs.txt", "theirs\n");
            let tip = commit(&fixture.workspace, "theirs");
            let plan = fixture.plan().unwrap();
            assert_eq!(plan.old, old);
            assert_eq!(plan.tip, tip);
            let private = store_for_plan(&fixture.p, &plan).unwrap();
            let parents = run(&private, &["show", "-s", "--format=%P", &plan.new]);
            assert_eq!(
                parents,
                if squash {
                    old.clone()
                } else {
                    format!("{old} {tip}")
                }
            );
            let persisted = serde_json::to_string(&plan).unwrap();
            let resumed: Plan = serde_json::from_str(&persisted).unwrap();
            apply(&fixture.p, &resumed).unwrap();
            apply(&fixture.p, &resumed).unwrap();
            assert_eq!(run(&fixture.p.repo, &["rev-parse", "HEAD"]), plan.new);
            assert_eq!(has_object(&fixture.p.repo, &plan.tip).unwrap(), !squash);
            for file in ["ours.txt", "theirs.txt"] {
                assert!(fixture.p.repo.join(file).is_file());
            }
            assert_eq!(run(&fixture.p.repo, &["status", "--porcelain"]), "");
        }
    }

    #[test]
    fn refuses_dirty_and_ignored_collisions_even_with_merge_config() {
        let fixture = Fixture::new().unwrap();
        write(&fixture.workspace, "collision.txt", "landed\n");
        commit(&fixture.workspace, "feature");
        let plan = fixture.plan().unwrap();
        run(&fixture.p.repo, &["config", "merge.autostash", "true"]);
        run(&fixture.p.repo, &["config", "merge.ff", "false"]);
        run(
            &fixture.p.repo,
            &["config", "branch.main.mergeOptions", "--abort"],
        );

        write(&fixture.p.repo, "base.txt", "keep tracked edit\n");
        assert!(
            apply(&fixture.p, &plan)
                .unwrap_err()
                .to_string()
                .contains("local changes")
        );
        assert_eq!(
            fs::read_to_string(fixture.p.repo.join("base.txt")).unwrap(),
            "keep tracked edit\n"
        );
        write(&fixture.p.repo, "base.txt", "base\n");

        write(&fixture.p.repo, "collision.txt", "keep untracked file\n");
        assert!(
            apply(&fixture.p, &plan)
                .unwrap_err()
                .to_string()
                .contains("local changes")
        );
        write(
            &fixture.p.repo,
            ".git/info/exclude",
            "collision.txt\nnoncollision.txt\n",
        );
        let error = format!("{:#}", apply(&fixture.p, &plan).unwrap_err());
        assert!(error.contains("collision.txt"), "{error}");
        assert_eq!(run(&fixture.p.repo, &["rev-parse", "HEAD"]), fixture.base);
        assert_eq!(
            fs::read_to_string(fixture.p.repo.join("collision.txt")).unwrap(),
            "keep untracked file\n"
        );

        fs::rename(
            fixture.p.repo.join("collision.txt"),
            fixture.p.repo.join("noncollision.txt"),
        )
        .unwrap();
        apply(&fixture.p, &plan).unwrap();
        assert_eq!(
            fs::read_to_string(fixture.p.repo.join("noncollision.txt")).unwrap(),
            "keep untracked file\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.p.repo.join("collision.txt")).unwrap(),
            "landed\n"
        );
    }

    #[test]
    fn refuses_sparse_and_busy_primary_checkout() {
        let fixture = Fixture::new().unwrap();
        write(&fixture.workspace, "feature.txt", "feature\n");
        commit(&fixture.workspace, "feature");
        let plan = fixture.plan().unwrap();
        run(&fixture.p.repo, &["sparse-checkout", "set", "subdirectory"]);
        assert!(
            apply(&fixture.p, &plan)
                .unwrap_err()
                .to_string()
                .contains("sparse")
        );

        run(&fixture.p.repo, &["sparse-checkout", "disable"]);
        let admin = &fixture.p.common;
        write(admin, "MERGE_HEAD", &plan.tip);
        let error = apply(&fixture.p, &plan).unwrap_err();
        assert_eq!(error.downcast_ref::<Fault>().unwrap().code, "git_busy");
        fs::remove_file(admin.join("MERGE_HEAD")).unwrap();

        apply(&fixture.p, &plan).unwrap();
        assert_eq!(run(&fixture.p.repo, &["rev-parse", "HEAD"]), plan.new);
        assert!(fixture.p.repo.join("feature.txt").is_file());
    }

    #[test]
    fn refuses_unexpected_ref_for_unchecked_branch_and_retries_cas() {
        let mut fixture = Fixture::new().unwrap();
        fixture.options.into = "target".into();
        run(&fixture.p.repo, &["branch", "target"]);
        write(&fixture.workspace, "feature.txt", "feature\n");
        commit(&fixture.workspace, "feature");
        let plan = fixture.plan().unwrap();

        write(&fixture.p.repo, "other.txt", "other\n");
        let other = commit(&fixture.p.repo, "other");
        run(
            &fixture.p.repo,
            &["update-ref", "refs/heads/target", &other, &plan.old],
        );
        assert!(
            apply(&fixture.p, &plan)
                .unwrap_err()
                .to_string()
                .contains("tip changed")
        );
        assert_eq!(
            run(&fixture.p.repo, &["rev-parse", "refs/heads/target"]),
            other
        );

        let revised = fixture.plan().unwrap();
        apply(&fixture.p, &revised).unwrap();
        apply(&fixture.p, &revised).unwrap();
        assert_eq!(
            run(&fixture.p.repo, &["rev-parse", "refs/heads/target"]),
            revised.new
        );
        assert_eq!(run(&fixture.p.repo, &["rev-parse", "HEAD"]), other);
        assert!(!fixture.p.repo.join("feature.txt").exists());
    }

    #[test]
    fn refuses_ref_updates_while_destination_is_detached_for_rebase() {
        let fixture = Fixture::new().unwrap();
        write(&fixture.workspace, "feature.txt", "feature\n");
        commit(&fixture.workspace, "feature");
        let plan = fixture.plan().unwrap();
        let admin = PathBuf::from(run(&fixture.p.repo, &["rev-parse", "--absolute-git-dir"]));
        run(&fixture.p.repo, &["switch", "--detach"]);
        fs::create_dir(admin.join("rebase-merge")).unwrap();
        write(&admin, "rebase-merge/head-name", "refs/heads/main\n");
        assert!(
            apply(&fixture.p, &plan)
                .unwrap_err()
                .to_string()
                .contains("unfinished Git operation")
        );
        assert_eq!(
            run(&fixture.p.repo, &["rev-parse", "refs/heads/main"]),
            fixture.base
        );
        assert!(!fixture.p.repo.join("feature.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn prepared_hook_rejection_preserves_checkout_and_keeps_tip_private() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new().unwrap();
        write(&fixture.workspace, "feature.txt", "feature\n");
        let tip = commit(&fixture.workspace, "feature");
        let plan = fixture.plan().unwrap();
        let hooks = fixture.root.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        let hook = hooks.join("reference-transaction");
        fs::write(&hook, "#!/bin/sh\nif [ \"$1\" = prepared ]; then\n  while read -r old new ref; do\n    if [ \"$ref\" = refs/heads/main ]; then\n      echo 'landing fixture rejected the ref transaction' >&2\n      exit 1\n    fi\n  done\nfi\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let error = format!("{:#}", apply(&fixture.p, &plan).unwrap_err());
        assert!(error.contains("landing fixture rejected the ref transaction"));
        assert_eq!(run(&fixture.p.repo, &["rev-parse", "HEAD"]), fixture.base);
        assert_eq!(run(&fixture.workspace, &["rev-parse", "HEAD"]), tip);
        assert!(!fixture.p.repo.join("feature.txt").exists());
        assert!(!has_object(&fixture.p.repo, &tip).unwrap());
    }

    #[test]
    fn reports_conflict_paths_and_preserves_checkout_and_refs() {
        let fixture = Fixture::new().unwrap();
        let file = "file with spaces.txt";
        write(&fixture.p.repo, file, "ours\n");
        let old = commit(&fixture.p.repo, "ours");
        write(&fixture.workspace, file, "theirs\n");
        let tip = commit(&fixture.workspace, "theirs");
        let error = fixture.plan().unwrap_err();
        let fault = error
            .downcast_ref::<Fault>()
            .unwrap_or_else(|| panic!("{error:#}"));
        assert_eq!(fault.code, "merge_conflict");
        let root = stores(&fixture.p, false).unwrap();
        let has_pending = || {
            fs::read_dir(&root).unwrap().any(|entry| {
                let name = entry.unwrap().file_name();
                name.to_string_lossy().starts_with(".pending-")
            })
        };
        assert!(!has_pending());
        assert_eq!(
            fixture
                .plan()
                .unwrap_err()
                .downcast_ref::<Fault>()
                .unwrap()
                .code,
            "merge_conflict"
        );
        assert!(!has_pending());
        assert_eq!(fault.details["paths"], json!([file]));
        let conflicts = fault.details["conflicts"].as_array().unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0]["type"], "CONFLICT (contents)");
        assert_eq!(conflicts[0]["paths"], json!([file]));
        assert_eq!(run(&fixture.p.repo, &["rev-parse", "HEAD"]), old);
        assert_eq!(run(&fixture.workspace, &["rev-parse", "HEAD"]), tip);
        let blob = run(&fixture.workspace, &["rev-parse", &format!("HEAD:{file}")]);
        assert!(!has_object(&fixture.p.repo, &tip).unwrap());
        assert!(!has_object(&fixture.p.repo, &blob).unwrap());
        assert_eq!(
            fs::read_to_string(fixture.p.repo.join(file)).unwrap(),
            "ours\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.workspace.join(file)).unwrap(),
            "theirs\n"
        );
    }

    #[test]
    fn commits_workspace_changes_only_on_explicit_request() {
        let mut fixture = Fixture::new().unwrap();
        fs::remove_file(fixture.workspace.join("base.txt")).unwrap();
        write(&fixture.workspace, ".gitignore", "ignored.txt\n");
        write(&fixture.workspace, "ignored.txt", "local cache\n");
        write(&fixture.workspace, "new.txt", "new workspace file\n");
        assert!(
            fixture
                .plan()
                .unwrap_err()
                .to_string()
                .contains("Commit them first")
        );
        assert_eq!(
            run(&fixture.workspace, &["rev-parse", "HEAD"]),
            fixture.base
        );
        assert_eq!(
            run(&fixture.workspace, &["diff", "--cached", "--name-only"]),
            ""
        );

        fixture.options.commit = Some("Workspace changes".into());
        let plan = fixture.plan().unwrap();
        assert_eq!(
            run(
                &fixture.workspace,
                &["show", "-s", "--format=%s", &plan.tip]
            ),
            "Workspace changes"
        );
        assert_eq!(
            run(&fixture.workspace, &["ls-tree", "--name-only", "HEAD"]),
            ".gitignore\nnew.txt"
        );
        assert_eq!(
            fs::read_to_string(fixture.workspace.join("ignored.txt")).unwrap(),
            "local cache\n"
        );
        assert_eq!(fixture.plan().unwrap().tip, plan.tip);
    }

    #[test]
    fn reports_modify_delete_and_rename_delete_without_reconciliation() {
        for rename in [false, true] {
            let mut fixture = Fixture::new().unwrap();
            fixture.options.reconcile = true;
            let file = "package.json";
            let renamed = "renamed.json";
            write(
                &fixture.p.repo,
                file,
                "{\"name\":\"fixture\",\"version\":\"1.0.0\"}\n",
            );
            let base = commit(&fixture.p.repo, "add manifest");
            fixture.sync_base(&base);
            fs::remove_file(fixture.p.repo.join(file)).unwrap();
            let old = commit(&fixture.p.repo, "remove manifest");
            let (kind, paths) = if rename {
                fs::rename(
                    fixture.workspace.join(file),
                    fixture.workspace.join(renamed),
                )
                .unwrap();
                ("CONFLICT (rename/delete)", json!([file, renamed]))
            } else {
                write(
                    &fixture.workspace,
                    file,
                    "{\"name\":\"fixture\",\"version\":\"2.0.0\"}\n",
                );
                ("CONFLICT (modify/delete)", json!([file]))
            };
            let tip = commit(&fixture.workspace, "change manifest");

            let error = fixture.plan().unwrap_err();
            let fault = error.downcast_ref::<Fault>().unwrap();
            assert_eq!(fault.code, "merge_conflict");
            assert_eq!(fault.details["paths"], paths);
            let conflicts = fault.details["conflicts"].as_array().unwrap();
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0]["type"], kind);
            assert!(conflicts[0]["message"].as_str().unwrap().contains(file));
            assert!(fault.details.get("reconcile_error").is_none());
            assert!(!fixture.p.state.join("cache/npm").exists());
            assert_eq!(run(&fixture.p.repo, &["rev-parse", "HEAD"]), old);
            assert_eq!(run(&fixture.workspace, &["rev-parse", "HEAD"]), tip);
        }
    }

    #[test]
    fn reconcile_only_resolves_eligible_conflicts_when_requested() {
        fn write_packages(path: &Path, package: Option<&str>) {
            let mut manifest = json!({"name": "fixture", "version": "1.0.0"});
            let mut lock = json!({
                "name": "fixture", "version": "1.0.0", "lockfileVersion": 3, "requires": true,
                "packages": {"": {"name": "fixture", "version": "1.0.0"}}
            });
            if let Some(package) = package {
                let dependencies = json!({package: "1.0.0"});
                manifest["dependencies"] = dependencies.clone();
                lock["packages"][""]["dependencies"] = dependencies;
                lock["packages"][format!("node_modules/{package}")] = json!({
                    "version": "1.0.0", "resolved": format!("https://registry.npmjs.org/{package}/-/{package}-1.0.0.tgz"),
                    "integrity": "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="
                });
            }
            write(
                path,
                "package.json",
                &serde_json::to_string_pretty(&manifest).unwrap(),
            );
            write(
                path,
                "package-lock.json",
                &serde_json::to_string_pretty(&lock).unwrap(),
            );
        }

        let mut fixture = Fixture::new().unwrap();
        write_packages(&fixture.p.repo, None);
        let base = commit(&fixture.p.repo, "npm base");
        fixture.sync_base(&base);
        write_packages(&fixture.p.repo, Some("alpha"));
        write(&fixture.p.repo, "ours.txt", "ours\n");
        commit(&fixture.p.repo, "ours");
        write_packages(&fixture.workspace, Some("beta"));
        write(&fixture.workspace, "theirs.txt", "theirs\n");
        commit(&fixture.workspace, "theirs");

        let error = fixture.plan().unwrap_err();
        let fault = error.downcast_ref::<Fault>().unwrap();
        assert_eq!(fault.code, "merge_conflict");
        assert_eq!(
            fault.details["paths"],
            json!(["package-lock.json", "package.json"])
        );
        assert!(!fixture.p.state.join("cache/npm").exists());

        fixture.options.reconcile = true;
        let plan = fixture.plan().unwrap_or_else(|error| {
            panic!(
                "{error:#} {}",
                error
                    .downcast_ref::<Fault>()
                    .map(|fault| fault.details.to_string())
                    .unwrap_or_default()
            )
        });
        apply(&fixture.p, &plan).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.p.repo.join("package.json")).unwrap())
                .unwrap();
        assert_eq!(
            manifest["dependencies"],
            json!({"alpha": "1.0.0", "beta": "1.0.0"})
        );
        for file in ["ours.txt", "theirs.txt"] {
            assert!(fixture.p.repo.join(file).is_file());
        }
    }
}

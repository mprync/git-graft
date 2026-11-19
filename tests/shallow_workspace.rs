use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

struct Fixture {
    temp: tempfile::TempDir,
    upstream: PathBuf,
    primary: PathBuf,
}

impl Fixture {
    fn new(format: &str, depth: usize) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let upstream = temp.path().join("upstream");
        let primary = temp.path().join("primary with spaces");
        let fixture = Self {
            temp,
            upstream,
            primary,
        };
        fs::create_dir(&fixture.upstream)?;
        fixture.git(
            &fixture.upstream,
            &[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--object-format={format}"),
            ],
        )?;
        fixture.identity(&fixture.upstream)?;
        for step in 1..=5 {
            fixture.commit(&fixture.upstream, "base.txt", &format!("base {step}"))?;
        }
        fixture.git(
            fixture.temp.path(),
            &[
                "clone",
                "--quiet",
                &format!("--depth={depth}"),
                &fixture.upstream_url()?,
                fixture.primary.to_str().context("primary path")?,
            ],
        )?;
        fixture.identity(&fixture.primary)?;
        Ok(fixture)
    }

    fn upstream_url(&self) -> Result<String> {
        let root = dunce::canonicalize(&self.upstream)?;
        let path = root.to_str().context("upstream path")?.replace('\\', "/");
        let slash = if path.starts_with('/') { "" } else { "/" };
        Ok(format!("file://{slash}{path}"))
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy().to_ascii_uppercase();
            if name.starts_with("GIT_") || name.starts_with("GRAFT_") {
                command.env_remove(key);
            }
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                self.temp.path().join("no-global-config"),
            )
            .stdin(Stdio::null());
        command
    }

    fn git_output(&self, repo: &Path, args: &[&str]) -> Result<Output> {
        Ok(self
            .command("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()?)
    }

    fn git(&self, repo: &Path, args: &[&str]) -> Result<String> {
        let out = self.git_output(repo, args)?;
        ensure!(
            out.status.success(),
            "git {args:?} in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8(out.stdout)?.trim_end().into())
    }

    fn identity(&self, repo: &Path) -> Result<()> {
        self.git(repo, &["config", "user.name", "Shallow fixture"])?;
        self.git(repo, &["config", "user.email", "fixture@example.invalid"])?;
        Ok(())
    }

    fn commit(&self, repo: &Path, file: &str, message: &str) -> Result<String> {
        fs::write(repo.join(file), format!("{message}\n"))?;
        self.git(repo, &["add", "--", file])?;
        self.git(repo, &["commit", "-qm", message])?;
        self.git(repo, &["rev-parse", "HEAD"])
    }

    fn graft_output(&self, args: &[&str]) -> Result<Output> {
        Ok(self
            .command(env!("CARGO_BIN_EXE_git-graft"))
            .arg("--repo")
            .arg(&self.primary)
            .arg("--json")
            .args(args)
            .output()?)
    }

    fn graft(&self, args: &[&str]) -> Result<Value> {
        let out = self.graft_output(args)?;
        ensure!(
            out.status.success(),
            "git graft {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(serde_json::from_slice(&out.stdout)?)
    }

    fn assert_standalone(&self, repo: &Path, expected_boundary: Option<&str>) -> Result<()> {
        let shallow = self.git(repo, &["rev-parse", "--is-shallow-repository"])?;
        assert_eq!(
            shallow,
            if expected_boundary.is_some() {
                "true"
            } else {
                "false"
            }
        );
        if let Some(boundary) = expected_boundary {
            assert_eq!(
                fs::read_to_string(repo.join(".git/shallow"))?.trim(),
                boundary
            );
        }
        assert!(!repo.join(".git/objects/info/alternates").exists());
        assert_eq!(self.git(repo, &["remote"])?, "");
        self.git(repo, &["fsck", "--full", "--strict"])?;
        Ok(())
    }
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("missing {key} in {value}"))
}

#[test]
fn shallow_workspace_lands_undoes_and_restores_native_commits() -> Result<()> {
    for (format, depth) in [("sha1", 1), ("sha256", 3)] {
        let f = Fixture::new(format, depth)?;
        let boundary = fs::read_to_string(f.primary.join(".git/shallow"))?;
        let primary_log = f.git(&f.primary, &["log", "--format=%H %s"])?;
        assert_eq!(primary_log.lines().count(), depth);
        let spawn = f.graft(&["spawn", "native", "--base", "main", "--no-deps"])?;
        let workspace = Path::new(field(&spawn, "path")?);
        assert_eq!(f.git(workspace, &["log", "--format=%H %s"])?, primary_log);
        assert_eq!(
            f.git(workspace, &["rev-parse", "--show-object-format"])?,
            format
        );
        f.assert_standalone(workspace, Some(boundary.trim()))?;
        let tip = f.commit(workspace, "native.txt", "native private change")?;
        let land = f.graft(&["land", "native", "--lease", field(&spawn, "lease")?])?;
        assert_eq!(field(&land, "commit")?, tip);
        assert_eq!(f.git(&f.primary, &["rev-parse", "HEAD"])?, tip);
        assert_eq!(
            fs::read_to_string(f.primary.join("native.txt"))?,
            "native private change\n"
        );
        assert_eq!(
            fs::read_to_string(f.primary.join(".git/shallow"))?,
            boundary
        );
        for entry in fs::read_dir(f.primary.join(".git/objects/pack"))? {
            assert!(entry?.path().extension().is_none_or(|ext| ext != "keep"));
        }
        f.git(&f.primary, &["fsck", "--full", "--strict"])?;
        let lease = field(&land, "recovery_lease")?;
        f.graft(&["undo-land", "native", "--lease", lease])?;
        assert_eq!(
            f.git(&f.primary, &["rev-parse", "HEAD^{tree}"])?,
            f.git(
                &f.primary,
                &[
                    "rev-parse",
                    &format!("{}^{{tree}}", field(&land, "previous")?)
                ]
            )?
        );
        assert!(!f.primary.join("native.txt").exists());
        f.git(&f.primary, &["fsck", "--full", "--strict"])?;
        let restored = f.graft(&["restore", "native", "--lease", lease])?;
        let restored = Path::new(field(&restored, "path")?);
        assert_eq!(f.git(restored, &["rev-parse", "HEAD"])?, tip);
        f.assert_standalone(restored, Some(boundary.trim()))?;
    }
    Ok(())
}

#[test]
fn shallow_object_cache_tracks_deepen_and_excludes_disconnected_objects() -> Result<()> {
    for format in ["sha1", "sha256"] {
        let f = Fixture::new(format, 1)?;
        let base = f.git(&f.primary, &["rev-parse", "HEAD"])?;
        f.git(&f.upstream, &["switch", "--orphan", "island"])?;
        f.commit(&f.upstream, "private.txt", "disconnected root")?;
        let island = f.commit(&f.upstream, "private.txt", "disconnected private commit")?;
        let island_blob = f.git(&f.upstream, &["rev-parse", "HEAD:private.txt"])?;
        f.git(&f.upstream, &["switch", "main"])?;
        f.git(
            &f.primary,
            &["fetch", "--depth=1", "origin", "island:refs/heads/island"],
        )?;
        let primary_boundary = fs::read_to_string(f.primary.join(".git/shallow"))?;
        assert!(primary_boundary.lines().any(|line| line == island));
        assert!(primary_boundary.lines().any(|line| line == base));
        let first = f.graft(&["spawn", "initial", "--no-deps"])?;
        let root = Path::new(field(&first, "path")?);
        f.assert_standalone(root, Some(&base))?;
        for oid in [&island, &island_blob] {
            assert!(
                !f.git_output(root, &["cat-file", "-e", oid])?
                    .status
                    .success()
            );
        }
        let repeat = f.graft(&["spawn", "original-again", "--no-deps"])?;
        if first["storage"]["objects"]["cache_bytes"]
            .as_u64()
            .unwrap_or_default()
            > 0
        {
            assert_eq!(repeat["storage"]["objects"]["source"], "cache");
        }

        f.git(&f.primary, &["fetch", "--deepen=2", "origin", "main"])?;
        let deeper = f.graft(&["spawn", "deeper", "--no-deps"])?;
        assert_eq!(field(&deeper, "base")?, base);
        assert_ne!(deeper["storage"]["objects"]["source"], "cache");
        let deeper = Path::new(field(&deeper, "path")?);
        let expected = f.git(&f.primary, &["rev-list", "HEAD"])?;
        assert_eq!(expected.lines().count(), 3);
        assert_eq!(f.git(deeper, &["rev-list", "HEAD"])?, expected);
        f.assert_standalone(deeper, expected.lines().last())?;
        assert_eq!(f.git(root, &["rev-list", "HEAD"])?, base);

        f.git(&f.primary, &["fetch", "--unshallow", "origin", "main"])?;
        let full = f.graft(&["spawn", "complete", "--no-deps"])?;
        assert_eq!(field(&full, "base")?, base);
        let full = Path::new(field(&full, "path")?);
        assert_eq!(f.git(full, &["rev-list", "--count", "HEAD"])?, "5");
        f.assert_standalone(full, None)?;
        assert!(
            !f.git_output(full, &["cat-file", "-e", &island])?
                .status
                .success()
        );
    }
    Ok(())
}

#[test]
fn shallow_merge_keeps_all_reachable_boundaries() -> Result<()> {
    let f = Fixture::new("sha1", 1)?;
    f.git(&f.upstream, &["switch", "-c", "side", "HEAD~2"])?;
    let side = f.commit(&f.upstream, "side.txt", "side parent")?;
    f.git(&f.upstream, &["switch", "main"])?;
    let main = f.commit(&f.upstream, "main.txt", "main parent")?;
    f.git(&f.upstream, &["merge", "--no-ff", "--no-edit", "side"])?;
    f.git(
        &f.primary,
        &[
            "fetch",
            "--depth=2",
            "origin",
            "main:refs/heads/shallow-merge",
        ],
    )?;
    let spawn = f.graft(&[
        "spawn",
        "two-parents",
        "--base",
        "shallow-merge",
        "--no-deps",
    ])?;
    let workspace = Path::new(field(&spawn, "path")?);
    let expected = f.git(&f.primary, &["rev-list", "--parents", "shallow-merge"])?;
    assert_eq!(expected.lines().count(), 3);
    assert_eq!(
        f.git(workspace, &["rev-list", "--parents", "HEAD"])?,
        expected
    );
    let mut boundaries = [main, side];
    boundaries.sort();
    f.assert_standalone(workspace, Some(&boundaries.join("\n")))?;
    Ok(())
}

#[test]
fn shallow_land_requires_destination_owned_ancestry_before_publication() -> Result<()> {
    let f = Fixture::new("sha1", 2)?;
    let spawn = f.graft(&["spawn", "ancestry", "--no-deps"])?;
    let workspace = Path::new(field(&spawn, "path")?);
    let base = f.git(&f.primary, &["rev-parse", "HEAD"])?;
    f.git(&f.primary, &["fetch", "--unshallow", "origin"])?;
    f.git(&f.upstream, &["switch", "-c", "ancestry"])?;
    let parent = f.commit(&f.upstream, "ancestor.txt", "required destination ancestor")?;
    f.git(&f.upstream, &["switch", "-c", "feature"])?;
    let boundary = f.commit(&f.upstream, "feature.txt", "workspace shallow boundary")?;
    f.git(
        workspace,
        &[
            "fetch",
            "--depth=1",
            &f.upstream_url()?,
            "feature:refs/heads/feature",
        ],
    )?;
    f.git(workspace, &["switch", "feature"])?;
    let tip = f.commit(
        workspace,
        "private.txt",
        "commit above a workspace-only boundary",
    )?;
    assert!(
        !f.git_output(workspace, &["cat-file", "-e", &parent])?
            .status
            .success()
    );
    assert!(
        fs::read_to_string(workspace.join(".git/shallow"))?
            .lines()
            .any(|line| line == boundary)
    );

    let land_args = ["land", "ancestry", "--lease", field(&spawn, "lease")?];
    let objects_before = f.git(
        &f.primary,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    )?;
    let index = fs::read(f.primary.join(".git/index"))?;
    let failure = f.graft_output(&land_args)?;
    assert!(!failure.status.success());
    let error = String::from_utf8(failure.stderr)?;
    assert!(error.contains("ancestry cannot be resolved"), "{error}");
    assert_eq!(f.git(&f.primary, &["rev-parse", "HEAD"])?, base);
    assert_eq!(fs::read(f.primary.join(".git/index"))?, index);
    assert!(!f.primary.join("private.txt").exists());
    assert!(!f.primary.join(".git/shallow").exists());
    assert_eq!(
        f.git(&f.primary, &["for-each-ref", "refs/graft/workspaces/"])?,
        ""
    );
    assert_eq!(
        f.git(
            &f.primary,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)"
            ]
        )?,
        objects_before
    );

    f.git(
        &f.primary,
        &["fetch", "origin", "ancestry:refs/heads/acquired-ancestry"],
    )?;
    assert_eq!(
        f.git(&f.primary, &["rev-parse", "acquired-ancestry"])?,
        parent
    );
    let landed = f.graft(&land_args)?;
    assert_eq!(field(&landed, "commit")?, tip);
    assert_eq!(f.git(&f.primary, &["rev-parse", "HEAD"])?, tip);
    assert_eq!(f.git(&f.primary, &["rev-list", "--count", "HEAD"])?, "8");
    assert!(!f.primary.join(".git/shallow").exists());
    f.git(&f.primary, &["fsck", "--full", "--strict"])?;
    Ok(())
}

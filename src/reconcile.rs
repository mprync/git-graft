//! Conservative, opt-in offline reconciliation for root npm lockfiles.
//!
//! This module only constructs Git objects. The caller is responsible for
//! requiring explicit trust and execution permission before calling it.
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
    process::Stdio,
};

const SECTIONS: [&str; 3] = ["dependencies", "devDependencies", "optionalDependencies"];

fn git(repo: &Path, args: &[&str], input: Option<&[u8]>, index: Option<&Path>) -> Result<String> {
    let mut command = crate::git::command(repo);
    command.args(args);
    // Also discard other Git-specific variables, such as inherited config
    // overrides, before setting our deliberate private index below.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start git")?;
    if let Some(data) = input {
        child.stdin.take().context("git stdin")?.write_all(data)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("git output is not UTF-8")
}

fn oid(s: &str) -> Result<()> {
    if !(s.len() == 40 || s.len() == 64) || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("expected a full object ID");
    }
    Ok(())
}

fn read_json(repo: &Path, commit: &str, path: &str) -> Result<Value> {
    let entry = git(repo, &["ls-tree", commit, "--", path], None, None)?;
    if !entry.starts_with("100644 blob ") || !entry.trim_end().ends_with(&format!("\t{path}")) {
        bail!("{path} must be a normal root file in every commit");
    }
    let text = git(repo, &["show", &format!("{commit}:{path}")], None, None)?;
    serde_json::from_str(&text).with_context(|| format!("invalid {path}"))
}

fn object(v: &Value) -> Result<&Map<String, Value>> {
    v.as_object().context("expected JSON object")
}
fn registry_spec(name: &str, spec: &Value) -> Result<()> {
    let s = spec
        .as_str()
        .context("dependency version must be a string")?;
    if s.is_empty()
        || !s.bytes().any(|c| c.is_ascii_digit())
        || !s
            .bytes()
            .all(|c| c.is_ascii_digit() || b".^~*xX<>=| -".contains(&c))
    {
        bail!("unsupported non-semver dependency {name}");
    }
    Ok(())
}
fn deps(v: &Value) -> Result<BTreeMap<String, (String, Value)>> {
    let obj = object(v)?;
    let mut result = BTreeMap::new();
    for section in SECTIONS {
        if let Some(entries) = obj.get(section) {
            for (name, spec) in object(entries)? {
                // Registry semver ranges only; exclude paths, URLs, tags and aliases.
                registry_spec(name, spec)?;
                if result
                    .insert(name.clone(), (section.to_owned(), spec.clone()))
                    .is_some()
                {
                    bail!("dependency {name} appears in multiple sections");
                }
            }
        }
    }
    Ok(result)
}

fn without_deps(v: &Value) -> Result<Value> {
    let mut copy = object(v)?.clone();
    for key in SECTIONS {
        copy.remove(key);
    }
    Ok(Value::Object(copy))
}

fn merge_additions(base: &Value, ours: &Value, theirs: &Value) -> Result<Value> {
    if [base, ours, theirs].iter().any(|v| {
        v.get("workspaces").is_some()
            || v.get("peerDependencies").is_some()
            || v.get("bundleDependencies").is_some()
            || v.get("bundledDependencies").is_some()
            || v.get("overrides").is_some()
    }) {
        bail!("workspaces, peers, bundled dependencies and overrides are unsupported");
    }
    if without_deps(base)? != without_deps(ours)? || without_deps(base)? != without_deps(theirs)? {
        bail!("non-dependency manifest changes are unsupported");
    }
    let base_deps = deps(base)?;
    let left = deps(ours)?;
    let right = deps(theirs)?;
    for (name, value) in &base_deps {
        if left.get(name) != Some(value) || right.get(name) != Some(value) {
            bail!("existing dependency {name} changed or was removed");
        }
    }
    let mut all = base_deps;
    for (name, value) in left.into_iter().chain(right) {
        if let Some(previous) = all.get(&name) {
            if previous != &value {
                bail!("conflicting dependency {name}");
            }
        } else {
            all.insert(name, value);
        }
    }
    let mut merged = object(base)?.clone();
    for section in SECTIONS {
        let entries: Map<String, Value> = all
            .iter()
            .filter(|(_, (s, _))| s == section)
            .map(|(n, (_, v))| (n.clone(), v.clone()))
            .collect();
        if !entries.is_empty() || merged.contains_key(section) {
            merged.insert(section.to_owned(), Value::Object(entries));
        }
    }
    Ok(Value::Object(merged))
}

fn validate_lock(lock: &Value, manifest: &Value) -> Result<()> {
    let obj = object(lock)?;
    if obj.get("lockfileVersion").and_then(Value::as_u64) != Some(3)
        || obj.contains_key("dependencies")
    {
        bail!("only npm lockfile version 3 is supported");
    }
    let packages = object(obj.get("packages").context("missing lock packages")?)?;
    let root = packages.get("").context("missing lock root")?;
    if deps(root)? != deps(manifest)? {
        bail!("manifest and lockfile dependencies differ");
    }
    for (path, entry) in packages {
        if path.is_empty() {
            continue;
        }
        if !path.starts_with("node_modules/")
            || path.contains('\\')
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || !entry.is_object()
            || entry.get("link").is_some()
            || entry.get("inBundle").is_some()
        {
            bail!("unsupported lock package {path}");
        }
        for section in ["dependencies", "optionalDependencies", "peerDependencies"] {
            if let Some(entries) = entry.get(section) {
                for (name, spec) in object(entries)? {
                    registry_spec(name, spec)?;
                }
            }
        }
        let resolved = entry
            .get("resolved")
            .and_then(Value::as_str)
            .context("package lacks registry resolution")?;
        if !resolved.starts_with("https://")
            || entry.get("integrity").and_then(Value::as_str).is_none()
        {
            bail!("unsupported lock resolution for {path}");
        }
    }
    for name in deps(manifest)?.keys() {
        if !packages.contains_key(&format!("node_modules/{name}")) {
            bail!("missing direct lock resolution for {name}");
        }
    }
    Ok(())
}

fn merge_lock(
    base: &Value,
    ours: &Value,
    theirs: &Value,
    manifests: [&Value; 3],
    merged_manifest: &Value,
) -> Result<Value> {
    for (lock, manifest) in [base, ours, theirs].into_iter().zip(manifests) {
        validate_lock(lock, manifest)?;
    }
    let mut result = object(base)?.clone();
    let mut result_packages = object(base.get("packages").context("missing packages")?)?.clone();
    let root = merge_additions(
        &result_packages[""],
        &ours["packages"][""],
        &theirs["packages"][""],
    )?;
    for side in [ours, theirs] {
        let mut meta = object(side)?.clone();
        meta.remove("packages");
        let mut base_meta = object(base)?.clone();
        base_meta.remove("packages");
        if meta != base_meta {
            bail!("lockfile metadata changed");
        }
        let packages = object(&side["packages"])?;
        for (path, entry) in &result_packages {
            if !path.is_empty()
                && packages.get(path) != Some(entry)
                && object(&base["packages"])?.contains_key(path)
            {
                bail!("existing lock resolution changed: {path}");
            }
        }
        for (path, entry) in packages {
            if path.is_empty() {
                continue;
            }
            if let Some(existing) = result_packages.get(path) {
                if existing != entry {
                    bail!("incompatible lock resolutions: {path}");
                }
            } else {
                result_packages.insert(path.clone(), entry.clone());
            }
        }
    }
    result_packages.insert("".into(), root);
    result.insert("packages".into(), Value::Object(result_packages));
    let result = Value::Object(result);
    validate_lock(&result, merged_manifest)?;
    Ok(result)
}

/// Return a new tree containing a validated offline npm resolution.
/// Never modifies the live index, branches, or checkout.
pub fn resolve(
    repo: &Path,
    state: &Path,
    ours: &str,
    theirs: &str,
    conflict_tree: &str,
    paths: &[String],
) -> Result<String> {
    for id in [ours, theirs, conflict_tree] {
        oid(id)?;
    }
    let unique: BTreeSet<_> = paths.iter().map(String::as_str).collect();
    if unique.is_empty()
        || unique
            .iter()
            .any(|p| !["package.json", "package-lock.json"].contains(p))
    {
        bail!("only root npm manifest/lockfile conflicts are supported");
    }
    let bases = git(repo, &["merge-base", "--all", ours, theirs], None, None)?;
    let bases: Vec<_> = bases.lines().collect();
    if bases.len() != 1 {
        bail!("exactly one merge base is required");
    }
    let manifests: Vec<Value> = [bases[0], ours, theirs]
        .iter()
        .map(|c| read_json(repo, c, "package.json"))
        .collect::<Result<_>>()?;
    let locks: Vec<Value> = [bases[0], ours, theirs]
        .iter()
        .map(|c| read_json(repo, c, "package-lock.json"))
        .collect::<Result<_>>()?;
    let manifest = merge_additions(&manifests[0], &manifests[1], &manifests[2])?;
    let lock = merge_lock(
        &locks[0],
        &locks[1],
        &locks[2],
        [&manifests[0], &manifests[1], &manifests[2]],
        &manifest,
    )?;
    let temp = tempfile::tempdir().context("create private reconciliation directory")?;
    let cache = state.join("cache").join("npm");
    // Reject symlinked cache directories before handing their path to npm.
    for dir in [state.to_path_buf(), state.join("cache"), cache.clone()] {
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                bail!("unsafe npm cache directory: {}", dir.display())
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?
            }
            Err(e) => return Err(e.into()),
        }
    }
    let cache = cache.canonicalize()?;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    std::fs::write(temp.path().join("package.json"), &manifest_bytes)?;
    std::fs::write(
        temp.path().join("package-lock.json"),
        serde_json::to_vec_pretty(&lock)?,
    )?;
    let mut npm = crate::runtime::npm_command()?;
    npm.current_dir(temp.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", temp.path())
        .env("TEMP", temp.path())
        .env("TMP", temp.path())
        .env("npm_config_cache", &cache)
        .env("npm_config_userconfig", temp.path().join("no-user-config"))
        .env(
            "npm_config_globalconfig",
            temp.path().join("no-global-config"),
        )
        .args([
            "install",
            "--package-lock-only",
            "--offline",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
            "--no-update-notifier",
        ])
        .stdin(Stdio::null());
    // Windows requires SystemRoot to start normal native processes after
    // clearing the inherited npm and Git configuration environment.
    if let Some(root) = std::env::var_os("SystemRoot") {
        npm.env("SystemRoot", root);
    }
    let output = npm
        .output()
        .context("start npm for offline reconciliation")?;
    if !output.status.success() {
        bail!(
            "offline npm reconciliation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let regenerated: Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("package-lock.json"))?)
            .context("invalid regenerated lockfile")?;
    if regenerated != lock {
        bail!("npm produced unexpected lock resolutions or metadata");
    }
    if serde_json::from_slice::<Value>(&std::fs::read(temp.path().join("package.json"))?)?
        != manifest
    {
        bail!("npm changed the manifest");
    }
    let index = temp.path().join("index");
    git(repo, &["read-tree", conflict_tree], None, Some(&index))?;
    for (path, bytes) in [
        ("package.json", manifest_bytes),
        (
            "package-lock.json",
            serde_json::to_vec_pretty(&regenerated)?,
        ),
    ] {
        let blob = git(repo, &["hash-object", "-w", "--stdin"], Some(&bytes), None)?;
        git(
            repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "100644",
                blob.trim(),
                path,
            ],
            None,
            Some(&index),
        )?;
    }
    Ok(git(repo, &["write-tree"], None, Some(&index))?
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{path::PathBuf, process::Command};

    fn run(repo: &Path, args: &[&str]) -> String {
        git(repo, args, None, None).unwrap()
    }
    fn write(repo: &Path, name: &str, value: &Value) {
        std::fs::write(repo.join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }
    fn commit(repo: &Path, manifest: &Value, lock: &Value, message: &str) -> String {
        write(repo, "package.json", manifest);
        write(repo, "package-lock.json", lock);
        run(repo, &["add", "package.json", "package-lock.json"]);
        run(repo, &["commit", "-qm", message]);
        run(repo, &["rev-parse", "HEAD"]).trim().into()
    }
    fn pair(name: Option<(&str, &str, &str)>) -> (Value, Value) {
        let mut manifest = json!({"name":"fixture","version":"1.0.0"});
        let mut lock = json!({"name":"fixture","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"fixture","version":"1.0.0"}}});
        if let Some((name, section, version)) = name {
            manifest[section] = json!({name: version});
            lock["packages"][""][section] = json!({name: version});
            lock["packages"][format!("node_modules/{name}")] = json!({"version":version,"resolved":format!("https://registry.npmjs.org/{name}/-/{name}-{version}.tgz"),"integrity":"sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="});
            if section == "devDependencies" {
                lock["packages"][format!("node_modules/{name}")]["dev"] = json!(true);
            }
        }
        (manifest, lock)
    }
    struct Fixture {
        _tmp: tempfile::TempDir,
        repo: PathBuf,
        state: PathBuf,
        ours: String,
        theirs: String,
        tree: String,
    }
    fn fixture(left: (&str, &str, &str), right: (&str, &str, &str)) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let state = tmp.path().join("state");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&state).unwrap();
        run(&repo, &["init", "-q"]);
        run(&repo, &["config", "user.email", "test@example.invalid"]);
        run(&repo, &["config", "user.name", "Test"]);
        let (m, l) = pair(None);
        std::fs::write(repo.join("other.txt"), "base\n").unwrap();
        run(&repo, &["add", "other.txt"]);
        let base = commit(&repo, &m, &l, "base");
        run(&repo, &["switch", "-qc", "left"]);
        std::fs::write(repo.join("other.txt"), "merged change\n").unwrap();
        run(&repo, &["add", "other.txt"]);
        let (m, l) = pair(Some(left));
        let ours = commit(&repo, &m, &l, "left");
        run(&repo, &["switch", "-qc", "right", &base]);
        let (m, l) = pair(Some(right));
        let theirs = commit(&repo, &m, &l, "right");
        // The real merge-tree output includes conflict markers; retain other merged files.
        let out = crate::git::command(&repo)
            .args(["merge-tree", "--write-tree", &ours, &theirs])
            .output()
            .unwrap();
        let tree = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned();
        Fixture {
            _tmp: tmp,
            repo,
            state,
            ours,
            theirs,
            tree,
        }
    }
    fn paths() -> Vec<String> {
        vec!["package.json".into(), "package-lock.json".into()]
    }
    #[test]
    fn offline_additions_preserve_live_checkout_and_index() {
        let f = fixture(
            ("alpha", "dependencies", "1.0.0"),
            ("beta", "dependencies", "2.0.0"),
        );
        std::fs::write(f.repo.join("untracked"), "keep").unwrap();
        std::fs::write(f.repo.join("package.json"), "dirty worktree\n").unwrap();
        let before_index = std::fs::read(f.repo.join(".git/index")).unwrap();
        let before_status = run(&f.repo, &["status", "--porcelain=v1"]);
        let before_head = run(&f.repo, &["rev-parse", "HEAD"]);
        let tree = resolve(&f.repo, &f.state, &f.ours, &f.theirs, &f.tree, &paths()).unwrap();
        let manifest: Value =
            serde_json::from_str(&run(&f.repo, &["show", &format!("{tree}:package.json")]))
                .unwrap();
        assert_eq!(
            manifest["dependencies"],
            json!({"alpha":"1.0.0", "beta":"2.0.0"})
        );
        let lock: Value = serde_json::from_str(&run(
            &f.repo,
            &["show", &format!("{tree}:package-lock.json")],
        ))
        .unwrap();
        assert!(lock["packages"].get("node_modules/alpha").is_some());
        assert!(lock["packages"].get("node_modules/beta").is_some());
        assert_eq!(
            run(&f.repo, &["show", &format!("{tree}:other.txt")]),
            "merged change\n"
        );
        assert_eq!(
            before_index,
            std::fs::read(f.repo.join(".git/index")).unwrap()
        );
        assert_eq!(before_status, run(&f.repo, &["status", "--porcelain=v1"]));
        assert_eq!(before_head, run(&f.repo, &["rev-parse", "HEAD"]));
        assert_eq!(
            std::fs::read_to_string(f.repo.join("package.json")).unwrap(),
            "dirty worktree\n"
        );
    }

    #[test]
    fn ignores_inherited_hook_repository_and_index() {
        if std::env::var_os("GRAFT_HOOK_TEST_CHILD").is_some() {
            offline_additions_preserve_live_checkout_and_index();
            return;
        }
        let hook = tempfile::tempdir().unwrap();
        run(hook.path(), &["init", "-q"]);
        run(
            hook.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        run(hook.path(), &["config", "user.name", "Test"]);
        std::fs::write(hook.path().join("hook-file"), "keep\n").unwrap();
        run(hook.path(), &["add", "hook-file"]);
        run(hook.path(), &["commit", "-qm", "hook base"]);
        let index = hook.path().join(".git/index");
        let before_index = std::fs::read(&index).unwrap();
        let before_head = run(hook.path(), &["rev-parse", "HEAD"]);
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "reconcile::tests::ignores_inherited_hook_repository_and_index",
                "--nocapture",
            ])
            .env("GRAFT_HOOK_TEST_CHILD", "1")
            .env("GIT_DIR", hook.path().join(".git"))
            .env("GIT_WORK_TREE", hook.path())
            .env("GIT_INDEX_FILE", &index)
            .env("GIT_OBJECT_DIRECTORY", hook.path().join(".git/objects"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "child failed:\n{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(before_index, std::fs::read(index).unwrap());
        assert_eq!(before_head, run(hook.path(), &["rev-parse", "HEAD"]));
        assert_eq!(
            std::fs::read_to_string(hook.path().join("hook-file")).unwrap(),
            "keep\n"
        );
        assert!(run(hook.path(), &["status", "--porcelain"]).is_empty());
    }
    #[test]
    fn rejects_same_name_version_and_section_conflicts() {
        for right in [
            ("alpha", "dependencies", "2.0.0"),
            ("alpha", "devDependencies", "1.0.0"),
        ] {
            let f = fixture(("alpha", "dependencies", "1.0.0"), right);
            assert!(resolve(&f.repo, &f.state, &f.ours, &f.theirs, &f.tree, &paths()).is_err());
        }
    }
    #[test]
    fn rejects_unexpected_lock_and_unsupported_changes() {
        let (base, base_lock) = pair(None);
        let (a, mut al) = pair(Some(("alpha", "dependencies", "1.0.0")));
        let (b, mut bl) = pair(Some(("beta", "dependencies", "1.0.0")));
        al["packages"]["node_modules/shared"] = json!({"version":"1.0.0","resolved":"https://example.invalid/shared","integrity":"sha512-a"});
        bl["packages"]["node_modules/shared"] = json!({"version":"2.0.0","resolved":"https://example.invalid/shared","integrity":"sha512-b"});
        let merged = merge_additions(&base, &a, &b).unwrap();
        assert!(merge_lock(&base_lock, &al, &bl, [&base, &a, &b], &merged).is_err());
        let mut local = a.clone();
        local["dependencies"]["alpha"] = json!("file:../alpha");
        assert!(merge_additions(&base, &local, &b).is_err());
        let mut workspace = a.clone();
        workspace["workspaces"] = json!(["packages/*"]);
        assert!(merge_additions(&base, &workspace, &b).is_err());
        al["packages"]["node_modules/alpha"]["dependencies"] =
            json!({"local": "file:../../outside"});
        assert!(validate_lock(&al, &a).is_err());
    }
}

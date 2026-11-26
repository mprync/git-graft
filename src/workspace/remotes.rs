//! Remotes of the primary that the project's configuration gives every graft
//! (`remotes` in `.graftconfig`), so ordinary `git fetch`, and `git push`
//! where the caller may push, work in it. A graft has no remotes otherwise.
//! Only a remote's URLs and fetch refspecs are copied, never credentials: a
//! remote whose URL carries a user name or password is left out. Remotes that
//! a file the repository carries names are set up only once trusted, as its
//! hooks are (see `hook::trust`).
use std::{io::Write, path::Path, process::Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::git;

/// Whether the caller may push through the remotes a graft gets. The command
/// line always may. An MCP host may only when it was started with
/// `--allow-push`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushing {
    Allowed,
    Disabled,
}

/// The transport of the push URL a remote gets when the caller may not push.
/// Git pushes to `url` when a remote has no `pushurl`, so leaving it out
/// would not stop a push. The graft's configuration also forbids this
/// transport, so `git push` fails with `transport 'graft-push-disabled' not
/// allowed` before Git looks for a program to run for it, while `git fetch`,
/// which uses `url`, works.
const NO_PUSH: &str = "graft-push-disabled";

/// The values of `key` in the configuration of the repository at `dir`.
fn values(dir: &Path, key: &str) -> Result<Vec<String>> {
    let out = git::raw(dir, &["config", "--get-all", key])?;
    // Exit status 1 means the key is not set.
    if out.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    anyhow::ensure!(out.status.success(), "git config --get-all {key} failed");
    Ok(String::from_utf8(out.stdout)?
        .lines()
        .map(str::to_owned)
        .collect())
}

/// Whether `url` names a user or password before its host, as in
/// `https://user:token@host/repo`. An scp-like `git@host:repo` names only the
/// SSH user, which is no credential.
fn carries_credentials(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let Some((userinfo, _)) = authority.rsplit_once('@') else {
        return false;
    };
    // An SSH user is how SSH logs in, not a secret; a password is.
    let ssh = scheme.eq_ignore_ascii_case("ssh") || scheme.eq_ignore_ascii_case("git+ssh");
    !ssh || userinfo.contains(':')
}

/// What the primary configures for a remote the graft is to get.
struct Source {
    urls: Vec<String>,
    pushurls: Vec<String>,
    fetch: Vec<String>,
}

/// Whether the graft gets the remote `name`: its configuration in the
/// primary, the graft's own remote of that name, or the fields of the entry
/// that reports why not.
enum Found {
    Copy(Source),
    Keep,
    Leave(Value),
}

/// Why a remote that needs trust was left out, and what lets it in.
const UNTRUSTED_HINT: &str = "The repository lists this remote, so a graft gets it only once the user has trusted it. \
     Read it with `git graft hook list`, then run `git graft hook trust` in a terminal";

/// Read the remote `name` of the primary at `primary`, for the graft at
/// `graft`. A remote the graft already has is kept; otherwise one that needs
/// trust it lacks, `untrusted`, is left out. Changes nothing.
fn find(primary: &Path, graft: &Path, name: &str, untrusted: bool) -> Result<Found> {
    if !values(graft, &format!("remote.{name}.url"))?.is_empty() {
        return Ok(Found::Keep);
    }
    if untrusted {
        return Ok(Found::Leave(
            json!({"status": "skipped", "reason": "untrusted", "hint": UNTRUSTED_HINT}),
        ));
    }
    let urls = values(primary, &format!("remote.{name}.url"))?;
    if urls.is_empty() {
        return Ok(Found::Leave(
            json!({"status": "skipped", "reason": "not_configured"}),
        ));
    }
    let pushurls = values(primary, &format!("remote.{name}.pushurl"))?;
    if urls
        .iter()
        .chain(&pushurls)
        .any(|url| carries_credentials(url))
    {
        return Ok(Found::Leave(
            json!({"status": "skipped", "reason": "url_has_credentials"}),
        ));
    }
    let mut fetch = values(primary, &format!("remote.{name}.fetch"))?;
    if fetch.is_empty() {
        fetch.push(format!("+refs/heads/*:refs/remotes/{name}/*"));
    }
    Ok(Found::Copy(Source {
        urls,
        pushurls,
        fetch,
    }))
}

/// Give the graft the remote `name` as `source` describes it, and, when
/// `copy_refs`, the primary's remote-tracking refs of it. Returns the fields
/// of the entry that reports it.
fn give(
    primary: &Path,
    graft: &Path,
    name: &str,
    source: &Source,
    copy_refs: bool,
    pushing: Pushing,
) -> Result<Value> {
    let set = |key: String, values: &[String]| -> Result<()> {
        for value in values {
            git::bytes(graft, &["config", "--add", &key, value])?;
        }
        Ok(())
    };
    set(format!("remote.{name}.url"), &source.urls)?;
    match pushing {
        Pushing::Allowed => set(format!("remote.{name}.pushurl"), &source.pushurls)?,
        Pushing::Disabled => disable_push(graft, name)?,
    }
    set(format!("remote.{name}.fetch"), &source.fetch)?;
    let mut refs = 0;
    if copy_refs {
        // With local history the graft already holds their objects. The
        // primary's `HEAD` of the remote is a symbolic ref, which a fetch
        // would copy as a plain one that no later fetch moves, so it is
        // left out and made again below.
        let head = format!("refs/remotes/{name}/HEAD");
        git::bytes(
            graft,
            &[
                "fetch",
                "-q",
                "--no-tags",
                "--no-write-fetch-head",
                git::path_str(primary)?,
                &format!("+refs/remotes/{name}/*:refs/remotes/{name}/*"),
                &format!("^{head}"),
            ],
        )?;
        let prefix = format!("refs/remotes/{name}/");
        if let Some(target) = git::symbolic_ref(primary, &head)?
            && target.starts_with(&prefix)
            && git::raw(graft, &["rev-parse", "--verify", "--quiet", &target])?
                .status
                .success()
        {
            git::bytes(graft, &["symbolic-ref", &head, &target])?;
        }
        refs = list_refs(graft, &prefix)?.len();
    }
    Ok(json!({"status": "configured", "refs": refs, "push": pushing == Pushing::Allowed}))
}

/// Make Git refuse to push through the remote `name` of the graft at
/// `graft`: forbid the transport [`NO_PUSH`], then make it the remote's only
/// push URL. Each is one write of the graft's configuration, so a failure
/// leaves the remote's push URLs as they were.
fn disable_push(graft: &Path, name: &str) -> Result<()> {
    git::bytes(
        graft,
        &["config", &format!("protocol.{NO_PUSH}.allow"), "never"],
    )?;
    git::bytes(
        graft,
        &[
            "config",
            "--replace-all",
            &format!("remote.{name}.pushurl"),
            &format!("{NO_PUSH}::"),
        ],
    )?;
    Ok(())
}

/// Whether Git pushes anywhere through the remote `name` of the graft at
/// `graft`: to `url` when it has no push URL, or to a push URL that is not
/// [`NO_PUSH`]'s.
fn can_push(graft: &Path, name: &str) -> Result<bool> {
    let pushurls = values(graft, &format!("remote.{name}.pushurl"))?;
    let disabled = format!("{NO_PUSH}::");
    Ok(pushurls.is_empty() || pushurls.iter().any(|url| *url != disabled))
}

/// The fields of the entry for the remote `name` the graft at `graft`
/// already has. Its URLs are left as they are, except that a caller that may
/// not push makes Git refuse to push through it, as through a remote it is
/// given. `push` says whether Git can push through it now.
fn keep(graft: &Path, name: &str, pushing: Pushing) -> Value {
    let disabled = match pushing {
        Pushing::Allowed => Ok(()),
        Pushing::Disabled => disable_push(graft, name),
    };
    let mut fields = match can_push(graft, name) {
        Ok(push) => json!({"status": "kept", "push": push}),
        Err(error) => json!({"status": "kept", "reason": format!("{error:#}")}),
    };
    if let Err(error) = disabled {
        fields["reason"] = json!(format!("{error:#}"));
    }
    fields
}

/// The names of the refs under `prefix` in the repository at `dir`.
fn list_refs(dir: &Path, prefix: &str) -> Result<Vec<String>> {
    Ok(
        git::text(dir, &["for-each-ref", "--format=%(refname)", prefix])?
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// Take out of the graft at `graft` what [`give`] set up for the remote
/// `name`: its configuration and its remote-tracking refs. Only called for a
/// remote the graft did not have before.
fn undo(graft: &Path, name: &str) -> Result<()> {
    let section = git::raw(
        graft,
        &["config", "--remove-section", &format!("remote.{name}")],
    )?;
    // A failure before the first key was written leaves no section.
    anyhow::ensure!(
        section.status.success() || values(graft, &format!("remote.{name}.url"))?.is_empty(),
        "git config --remove-section remote.{name}: {}",
        String::from_utf8_lossy(&section.stderr).trim_end()
    );
    let refs = list_refs(graft, &format!("refs/remotes/{name}/"))?;
    if refs.is_empty() {
        return Ok(());
    }
    // `no-deref` deletes a symbolic `HEAD` itself, not the ref it names.
    let script: String = refs
        .iter()
        .map(|reference| format!("option no-deref\ndelete {reference}\n"))
        .collect();
    let mut child = git::command(graft)
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("start Git")?;
    let written = child
        .stdin
        .take()
        .context("open the input of git update-ref")?
        .write_all(script.as_bytes());
    let out = child
        .wait_with_output()
        .context("wait for git update-ref")?;
    written.context("write to git update-ref")?;
    anyhow::ensure!(
        out.status.success(),
        "git update-ref: {}",
        String::from_utf8_lossy(&out.stderr).trim_end()
    );
    Ok(())
}

/// Give the graft at `graft` each remote `names` lists, as the primary at
/// `primary` configures it, and, when `copy_refs`, the primary's
/// remote-tracking refs of it. When the caller may not push, each remote's
/// push URL is one that Git refuses. A remote the graft already has keeps its
/// URLs, except that a caller that may not push makes Git refuse to push
/// through it too. A remote `untrusted` names, which needs trust it lacks, is
/// left out. Returns one entry per name: `configured`, with the `refs` copied
/// and whether it can `push`; `kept`, with whether it can `push`; why it was
/// `skipped`; or why it `failed`. A remote that fails is taken out again
/// whole, and the others are still set up.
pub(crate) fn configure(
    primary: &Path,
    graft: &Path,
    names: &[String],
    untrusted: &[String],
    copy_refs: bool,
    pushing: Pushing,
) -> Vec<Value> {
    names
        .iter()
        .map(|name| {
            let fields = match find(primary, graft, name, untrusted.contains(name)) {
                Err(error) => json!({"status": "failed", "reason": format!("{error:#}")}),
                Ok(Found::Leave(fields)) => fields,
                Ok(Found::Keep) => keep(graft, name, pushing),
                Ok(Found::Copy(source)) => {
                    match give(primary, graft, name, &source, copy_refs, pushing) {
                        Ok(fields) => fields,
                        Err(error) => {
                            let mut reason = format!("{error:#}");
                            if let Err(undone) = undo(graft, name) {
                                reason.push_str(&format!(
                                    "; taking the remote out again also failed: {undone:#}"
                                ));
                            }
                            json!({"status": "failed", "reason": reason})
                        }
                    }
                }
            };
            let mut entry = json!({"name": name});
            if let (Value::Object(entry), Value::Object(fields)) = (&mut entry, fields) {
                entry.extend(fields);
            }
            entry
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn credentials_in_a_url_are_recognised() {
        for plain in [
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
            "https://github.com/owner/repo.git",
            "/srv/git/repo.git",
            "file:///srv/git/repo.git",
        ] {
            assert!(!carries_credentials(plain), "{plain}");
        }
        for secret in [
            "https://token@github.com/owner/repo.git",
            "https://user:pass@example.com/repo.git",
        ] {
            assert!(carries_credentials(secret), "{secret}");
        }
    }

    fn repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git::bytes(dir, &["init", "-q", "--template=", "-b", "main"]).unwrap();
        git::bytes(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "one",
            ],
        )
        .unwrap();
    }

    #[test]
    fn named_remotes_are_configured_with_their_refs_and_others_are_reported() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (primary, graft) = (tmp.path().join("primary"), tmp.path().join("graft"));
        repo(&primary);
        repo(&graft);
        let head = git::text(&primary, &["rev-parse", "HEAD"])?;
        git::bytes(
            &primary,
            &["remote", "add", "origin", "git@example.com:owner/repo.git"],
        )?;
        git::bytes(
            &primary,
            &[
                "config",
                "remote.origin.pushurl",
                "git@example.com:owner/push.git",
            ],
        )?;
        git::bytes(&primary, &["update-ref", "refs/remotes/origin/main", &head])?;
        git::bytes(
            &primary,
            &[
                "remote",
                "add",
                "secret",
                "https://token@example.com/repo.git",
            ],
        )?;
        let names: Vec<String> = ["origin", "secret", "missing"].map(str::to_owned).into();
        let entries = configure(&primary, &graft, &names, &[], false, Pushing::Allowed);
        assert_eq!(
            entries,
            [
                json!({"name": "origin", "status": "configured", "refs": 0, "push": true}),
                json!({"name": "secret", "status": "skipped", "reason": "url_has_credentials"}),
                json!({"name": "missing", "status": "skipped", "reason": "not_configured"}),
            ]
        );
        assert_eq!(
            git::text(&graft, &["config", "remote.origin.url"])?,
            "git@example.com:owner/repo.git"
        );
        assert_eq!(
            git::text(&graft, &["config", "remote.origin.pushurl"])?,
            "git@example.com:owner/push.git"
        );
        assert_eq!(
            git::text(&graft, &["config", "remote.origin.fetch"])?,
            "+refs/heads/*:refs/remotes/origin/*"
        );
        assert!(values(&graft, "remote.secret.url")?.is_empty());
        // A remote the graft has is kept as it is.
        let again = configure(&primary, &graft, &names[..1], &[], true, Pushing::Disabled);
        // A caller that may not push makes Git refuse to push through it.
        assert_eq!(
            again,
            [json!({"name": "origin", "status": "kept", "push": false})]
        );
        assert_eq!(
            values(&graft, "remote.origin.pushurl")?,
            [format!("{NO_PUSH}::")]
        );

        // With refs copied, the graft has the primary's remote-tracking refs.
        let fresh = tmp.path().join("fresh");
        repo(&fresh);
        git::bytes(&fresh, &["fetch", "-q", git::path_str(&primary)?, "HEAD"])?;
        let copied = configure(&primary, &fresh, &names[..1], &[], true, Pushing::Allowed);
        assert_eq!(
            copied,
            [json!({"name": "origin", "status": "configured", "refs": 1, "push": true})]
        );
        assert_eq!(git::text(&fresh, &["rev-parse", "origin/main"])?, head);
        Ok(())
    }

    /// A primary with a bare upstream `origin` it has fetched, whose
    /// `origin/HEAD` names `origin/main`, and a graft that holds its objects.
    fn cloned(tmp: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
        let (upstream, primary, graft) = (
            tmp.join("upstream.git"),
            tmp.join("primary"),
            tmp.join("graft"),
        );
        repo(&primary);
        git::bytes(
            tmp,
            &[
                "init",
                "-q",
                "--bare",
                "--template=",
                git::path_str(&upstream)?,
            ],
        )?;
        git::bytes(
            &primary,
            &["remote", "add", "origin", git::path_str(&upstream)?],
        )?;
        git::bytes(&primary, &["push", "-q", "origin", "main"])?;
        git::bytes(&primary, &["fetch", "-q", "origin"])?;
        git::bytes(&primary, &["remote", "set-head", "origin", "main"])?;
        repo(&graft);
        git::bytes(&graft, &["fetch", "-q", git::path_str(&primary)?, "main"])?;
        Ok((upstream, primary, graft))
    }

    #[test]
    fn origin_head_is_made_again_as_a_symbolic_ref_and_follows_a_fetch() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (_upstream, primary, graft) = cloned(tmp.path())?;
        let names = ["origin".to_owned()];
        let entries = configure(&primary, &graft, &names, &[], true, Pushing::Allowed);
        assert_eq!(
            entries,
            [json!({"name": "origin", "status": "configured", "refs": 2, "push": true})]
        );
        assert_eq!(
            git::symbolic_ref(&graft, "refs/remotes/origin/HEAD")?.as_deref(),
            Some("refs/remotes/origin/main")
        );
        // The upstream moves on; a fetch in the graft moves `origin` with it.
        let before = git::text(&graft, &["rev-parse", "origin/main"])?;
        git::bytes(
            &primary,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "two",
            ],
        )?;
        git::bytes(&primary, &["push", "-q", "origin", "main"])?;
        let moved = git::text(&primary, &["rev-parse", "HEAD"])?;
        assert_ne!(moved, before);
        git::bytes(&graft, &["fetch", "-q", "origin"])?;
        assert_eq!(git::text(&graft, &["rev-parse", "origin/main"])?, moved);
        assert_eq!(git::text(&graft, &["rev-parse", "origin"])?, moved);
        Ok(())
    }

    #[test]
    fn without_push_a_remote_fetches_but_git_refuses_to_push_through_it() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (upstream, primary, graft) = cloned(tmp.path())?;
        git::bytes(
            &primary,
            &["config", "remote.origin.pushurl", git::path_str(&upstream)?],
        )?;
        let names = ["origin".to_owned()];
        let entries = configure(&primary, &graft, &names, &[], true, Pushing::Disabled);
        assert_eq!(entries[0]["status"], "configured");
        assert_eq!(entries[0]["push"], false);
        assert_eq!(
            values(&graft, "remote.origin.pushurl")?,
            [format!("{NO_PUSH}::")]
        );
        git::bytes(&graft, &["fetch", "-q", "origin"])?;
        let pushed = git::raw(&graft, &["push", "origin", "main:refs/heads/pushed"])?;
        assert!(!pushed.status.success());
        assert!(
            String::from_utf8_lossy(&pushed.stderr)
                .contains(&format!("transport '{NO_PUSH}' not allowed")),
            "{}",
            String::from_utf8_lossy(&pushed.stderr)
        );
        let upstream_refs = git::text(&upstream, &["for-each-ref", "--format=%(refname)"])?;
        assert_eq!(upstream_refs, "refs/heads/main");
        Ok(())
    }

    #[test]
    fn a_remote_that_fails_is_taken_out_whole_and_the_others_are_still_set_up() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (_upstream, primary, graft) = cloned(tmp.path())?;
        git::bytes(
            &primary,
            &[
                "remote",
                "add",
                "upstream",
                "git@example.com:owner/repo.git",
            ],
        )?;
        let head = git::text(&primary, &["rev-parse", "HEAD"])?;
        git::bytes(
            &primary,
            &["update-ref", "refs/remotes/upstream/main", &head],
        )?;
        // Git cannot make `origin/HEAD` while its lock file exists, which
        // fails `origin` after its keys and refs are in place.
        let lock = graft.join(".git/refs/remotes/origin/HEAD.lock");
        std::fs::create_dir_all(lock.parent().context("no parent")?)?;
        std::fs::write(&lock, "")?;
        let names = ["origin".to_owned(), "upstream".to_owned()];
        let entries = configure(&primary, &graft, &names, &[], true, Pushing::Allowed);
        assert_eq!(entries[0]["name"], "origin");
        assert_eq!(entries[0]["status"], "failed");
        assert!(
            entries[0]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("symbolic-ref"))
        );
        assert_eq!(
            entries[1],
            json!({"name": "upstream", "status": "configured", "refs": 1, "push": true})
        );
        let keys = git::raw(&graft, &["config", "--get-regexp", r"^remote\.origin\."])?;
        assert_eq!(
            keys.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&keys.stdout)
        );
        assert!(list_refs(&graft, "refs/remotes/origin/")?.is_empty());
        // With the cause gone, a later restore sets it up rather than keeping half.
        std::fs::remove_file(&lock)?;
        let again = configure(&primary, &graft, &names, &[], true, Pushing::Allowed);
        assert_eq!(again[0]["status"], "configured");
        assert_eq!(
            again[1],
            json!({"name": "upstream", "status": "kept", "push": true})
        );
        Ok(())
    }

    #[test]
    fn a_remote_that_needs_trust_is_skipped_unless_the_graft_has_it() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (_upstream, primary, graft) = cloned(tmp.path())?;
        let names = ["origin".to_owned()];
        let entries = configure(&primary, &graft, &names, &names, true, Pushing::Allowed);
        assert_eq!(entries[0]["status"], "skipped");
        assert_eq!(entries[0]["reason"], "untrusted");
        assert!(
            entries[0]["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("git graft hook trust"))
        );
        assert!(values(&graft, "remote.origin.url")?.is_empty());
        assert!(list_refs(&graft, "refs/remotes/origin/")?.is_empty());
        // One the graft already has is kept as it is.
        configure(&primary, &graft, &names, &[], true, Pushing::Allowed);
        let kept = configure(&primary, &graft, &names, &names, true, Pushing::Allowed);
        assert_eq!(
            kept,
            [json!({"name": "origin", "status": "kept", "push": true})]
        );
        Ok(())
    }

    #[test]
    fn a_kept_remote_stops_pushing_for_a_caller_that_may_not_push() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (upstream, primary, graft) = cloned(tmp.path())?;
        git::bytes(
            &primary,
            &["config", "remote.origin.pushurl", git::path_str(&upstream)?],
        )?;
        let names = ["origin".to_owned()];
        // Made where pushing is allowed, the graft can push through it.
        let made = configure(&primary, &graft, &names, &[], true, Pushing::Allowed);
        assert_eq!(made[0]["push"], true);
        git::bytes(&graft, &["push", "-q", "origin", "main:refs/heads/first"])?;

        // Kept for a caller that may not push, it can fetch but not push.
        let kept = configure(&primary, &graft, &names, &[], true, Pushing::Disabled);
        assert_eq!(
            kept,
            [json!({"name": "origin", "status": "kept", "push": false})]
        );
        git::bytes(&graft, &["fetch", "-q", "origin"])?;
        let pushed = git::raw(&graft, &["push", "origin", "main:refs/heads/second"])?;
        assert!(!pushed.status.success());
        assert!(
            String::from_utf8_lossy(&pushed.stderr)
                .contains(&format!("transport '{NO_PUSH}' not allowed"))
        );
        // A caller that may push later does not undo that: the entry says so.
        let again = configure(&primary, &graft, &names, &[], true, Pushing::Allowed);
        assert_eq!(
            again,
            [json!({"name": "origin", "status": "kept", "push": false})]
        );
        assert_eq!(
            git::text(&upstream, &["for-each-ref", "--format=%(refname)"])?,
            "refs/heads/first\nrefs/heads/main"
        );
        Ok(())
    }
}

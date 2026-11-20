//! Append absent checkout settings under Git's own config lock.
use std::{
    fs::{self, Metadata},
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};

use anyhow::{Context, Result, ensure};

use super::open_regular;

fn identity(m: &Metadata) -> (u64, u64, i64, i64) {
    (m.dev(), m.ino(), m.ctime(), m.ctime_nsec())
}

fn initial_config(data: &[u8]) -> bool {
    let Ok(contents) = std::str::from_utf8(data) else {
        return false;
    };
    let mut lines = contents.lines();
    contents.ends_with('\n')
        && lines.next() == Some("[core]")
        && lines.all(|line| {
            let Some((key, value)) = line.trim_start_matches('\t').split_once(" = ") else {
                return false;
            };
            matches!(
                key,
                "repositoryformatversion"
                    | "filemode"
                    | "bare"
                    | "logallrefupdates"
                    | "ignorecase"
                    | "precomposeunicode"
                    | "symlinks"
            ) && matches!(value, "true" | "false" | "0")
        })
}

pub(super) fn append(root: &Path, settings: &[(String, String)]) -> Result<bool> {
    if settings.len() < 4
        || !settings.iter().all(|(key, value)| {
            matches!(
                key.as_str(),
                "user.name"
                    | "user.email"
                    | "author.name"
                    | "author.email"
                    | "committer.name"
                    | "committer.email"
                    | "core.autocrlf"
                    | "core.eol"
                    | "core.safecrlf"
                    | "core.bigfilethreshold"
            ) && !value.chars().any(char::is_control)
        })
    {
        return Ok(false);
    }
    let admin = root.join(".git");
    let config = admin.join("config");
    let mut lock = tempfile::Builder::new()
        .prefix("config.lock")
        .rand_bytes(0)
        .tempfile_in(&admin)
        .context("lock private Git configuration")?;
    let Ok(original) = open_regular(&config) else {
        return Ok(false);
    };
    let meta = original.metadata()?;
    if meta.len() > 1024 || meta.nlink() != 1 || !crate::recovery::no_extended_metadata(&config) {
        return Ok(false);
    }
    let mut data = Vec::new();
    (&original).take(1025).read_to_end(&mut data)?;
    // Only the simple core section emitted by git init can precede these new keys.
    if data.len() > 1024 || !initial_config(&data) {
        return Ok(false);
    }
    let mut section = "";
    for (key, value) in settings {
        let (group, name) = key
            .split_once('.')
            .context("missing Git configuration section")?;
        if group != section {
            writeln!(data, "[{group}]")?;
            section = group;
        }
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        writeln!(data, "\t{name} = \"{escaped}\"")?;
    }
    lock.as_file().set_permissions(meta.permissions())?;
    lock.write_all(&data)?;
    lock.as_file().sync_all()?;
    ensure!(
        identity(&fs::symlink_metadata(&config)?) == identity(&meta),
        "private Git configuration changed while applying settings"
    );
    lock.persist(&config)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn publishes_with_a_searchable_writable_git_directory() -> Result<()> {
        let root = tempfile::tempdir()?;
        let admin = root.path().join(".git");
        fs::create_dir(&admin)?;
        fs::write(admin.join("config"), "[core]\n\tbare = false\n")?;
        let settings = ["user.name", "user.email", "author.name", "committer.name"]
            .map(|key| (key.to_owned(), "fixture".to_owned()));
        fs::set_permissions(&admin, fs::Permissions::from_mode(0o300))?;
        let result = append(root.path(), &settings);
        fs::set_permissions(&admin, fs::Permissions::from_mode(0o700))?;
        assert!(result?);
        assert!(!admin.join("config.lock").exists());
        for (key, value) in settings {
            let out = super::super::run(
                root.path(),
                &["config", "--file", ".git/config", "--get-all", &key],
            )?;
            assert_eq!(super::super::output_text(&out)?, value);
        }
        Ok(())
    }
}

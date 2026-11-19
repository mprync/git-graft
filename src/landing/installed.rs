//! Paths which automatic staging must keep out of a workspace commit.
use std::{collections::BTreeSet, fs, io, path::Path};

use anyhow::{Result, ensure};
use serde_json::json;

use crate::{api::fault, git, registry};

pub(super) fn exposed(directories: &[&str]) -> anyhow::Error {
    fault(
        "registered_install_exposed",
        "Automatic staging cannot safely include installed dependencies. Restore their ignore rules or explicitly commit your intended files with Git.",
        json!({"directories": directories}),
    )
}

pub(super) fn spellings(workspace: &Path, directories: &[&str]) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    if directories.is_empty() {
        return Ok(paths);
    }
    let root = dunce::canonicalize(workspace)?;
    for &directory in directories {
        paths.insert(directory.to_owned());
        // Git's icase uses ASCII folding. The volume can also equate Unicode
        // spellings, including in the parents of a missing installation.
        let mut actual = root.clone();
        for component in directory.split('/') {
            actual.push(component);
            match fs::symlink_metadata(&actual) {
                Ok(metadata) => {
                    if metadata.is_symlink() || registry::is_reparse_point(&metadata) {
                        return Err(exposed(directories));
                    }
                    actual = dunce::canonicalize(&actual)?;
                    ensure!(actual.starts_with(&root), "install path left the workspace");
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        let relative = git::path_str(actual.strip_prefix(&root)?)?;
        paths.insert(relative.replace(std::path::MAIN_SEPARATOR, "/"));
    }
    Ok(paths)
}

pub(super) fn exclusions(path: &str) -> [String; 2] {
    let mut pattern = String::from(":(top,exclude,glob,icase)");
    for (i, c) in path.char_indices() {
        // Git add treats an ignored directory explicitly named in a pathspec
        // as an error, even in an exclusion. A leading glob escape keeps the
        // match exact without giving Git an explicit directory prefix.
        if i == 0 || matches!(c, '\\' | '*' | '?' | '[' | ']') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    let descendants = format!("{pattern}/**");
    [pattern, descendants]
}

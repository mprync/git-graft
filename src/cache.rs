//! Bind package cache roots to the registry and quarantine verified cache entries.

use crate::{
    recovery::rename_exclusive,
    registry::{self, Project, check_unmounted_directory, exists, is_reparse_point},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};
use uuid::Uuid;

pub(crate) const PACKAGES: &[&str] = &["npm", "pnpm", "bun", "cargo", "uv", "sccache"];
const OWNER: &str = ".git-graft-cache-id";
pub(crate) const QUARANTINE_PREFIX: &str = ".purge-";
const OWNER_LIMIT: u64 = 1024;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectoryIdentity {
    device: u64,
    file: u64,
    created: Option<SystemTime>,
}

pub(crate) fn directory_identity(path: &Path) -> Result<DirectoryIdentity> {
    let metadata = check_unmounted_directory(path, false)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(DirectoryIdentity {
            device: metadata.dev(),
            file: metadata.ino(),
            created: metadata.created().ok(),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle,
        };
        let directory = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // The directory handle and output storage are live through this call.
        if unsafe { GetFileInformationByHandle(directory.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        ensure!(
            info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
                == FILE_ATTRIBUTE_DIRECTORY,
            "expected an ordinary cache directory at {}",
            path.display()
        );
        let _ = metadata;
        Ok(DirectoryIdentity {
            device: u64::from(info.dwVolumeSerialNumber),
            file: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            created: None,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        anyhow::bail!("package cache directory identity is unavailable");
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackageBinding {
    project: String,
    package: String,
    nonce: String,
    directory: DirectoryIdentity,
}

fn private_directory(path: &Path) -> Result<()> {
    let metadata = check_unmounted_directory(path, false)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "cache directory is not private: {}",
            path.display()
        );
    }
    #[cfg(not(unix))]
    let _ = metadata;
    Ok(())
}

fn private_stage(parent: &Path, prefix: &str) -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    Ok(builder.tempdir_in(parent)?)
}

fn owner_record(project: &Project, package: &str) -> PathBuf {
    project
        .state
        .join(format!("package-cache-{package}-v1.json"))
}

pub(crate) fn read_private_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && !is_reparse_point(&metadata) && metadata.len() <= limit,
        "invalid private record at {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "Graft state record is not private: {}",
            path.display()
        );
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "private record is too large");
    Ok(bytes)
}

fn read_binding(project: &Project, package: &str) -> Result<PackageBinding> {
    ensure!(
        PACKAGES.contains(&package)
            || package
                .strip_prefix("registered-")
                .is_some_and(|id| id.len() == 64
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
        "unrecognized package cache root"
    );
    let binding: PackageBinding = serde_json::from_slice(&read_private_file(
        &owner_record(project, package),
        OWNER_LIMIT,
    )?)?;
    ensure!(
        binding.project == project.id()
            && binding.package == package
            && Uuid::parse_str(&binding.nonce).is_ok_and(
                |nonce| nonce.get_version_num() == 4 && nonce.to_string() == binding.nonce
            ),
        "invalid package cache registry binding"
    );
    Ok(binding)
}

pub(crate) fn verify_package(
    project: &Project,
    package: &str,
    path: &Path,
) -> Result<PackageBinding> {
    (|| {
        private_directory(path)?;
        let expected = read_binding(project, package)?;
        ensure!(
            read_private_file(&path.join(OWNER), OWNER_LIMIT)? == expected.nonce.as_bytes()
                && directory_identity(path)? == expected.directory,
            "package cache binding does not match this directory"
        );
        Ok(expected)
    })()
    .with_context(|| {
        format!(
            "package cache ownership is unverified at {}",
            path.display()
        )
    })
}

/// Pin every package root exported by the runtime. The project lock must be held.
/// Existing directories are accepted only with their original registry binding.
pub(crate) fn prepare_package_roots(project: &Project) -> Result<()> {
    prepare_roots(project, PACKAGES)
}

pub(crate) fn registered_cache_path(project: &Project, manager: &str) -> PathBuf {
    project
        .state
        .join("cache")
        .join(format!("registered-{}", blake3::hash(manager.as_bytes())))
}

pub(crate) fn prepare_registered_cache(project: &Project, manager: &str) -> Result<PathBuf> {
    let path = registered_cache_path(project, manager);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid cache identifier")?;
    prepare_roots(project, &[name])?;
    Ok(path)
}

fn prepare_roots(project: &Project, packages: &[&str]) -> Result<()> {
    project.require_private_state()?;
    let cache = project.state.join("cache");
    project.check_cache_owner(&cache)?;
    private_directory(&cache)?;
    let parent = directory_identity(&cache)?;
    let mut absent = Vec::new();
    for &package in packages {
        let path = cache.join(package);
        if exists(&path)? {
            verify_package(project, package, &path)?;
        } else {
            let record = owner_record(project, package);
            if exists(&record)? {
                // Agreement is required even for a retired binding. Inconclusive
                // registry state is left untouched.
                read_binding(project, package)?;
            }
            absent.push((package, path, record));
        }
    }
    for (package, path, record) in absent {
        let stage = private_stage(&cache, ".package-")?.keep();
        let binding = PackageBinding {
            project: project.id().to_owned(),
            package: package.to_owned(),
            nonce: Uuid::new_v4().to_string(),
            directory: directory_identity(&stage)?,
        };
        registry::atomic(&stage.join(OWNER), binding.nonce.as_bytes())?;
        ensure!(
            directory_identity(&cache)? == parent && !exists(&path)?,
            "package cache path changed during preparation"
        );
        project.check_cache_owner(&cache)?;
        // Commit the binding first. An interrupted publication leaves a vacant
        // slot that a later invocation can provision, without adopting any tree.
        registry::atomic(&record, &serde_json::to_vec(&binding)?)?;
        rename_exclusive(&stage, &path)
            .with_context(|| format!("publish package cache {}", path.display()))?;
        verify_package(project, package, &path)?;
    }
    Ok(())
}

pub(crate) enum PurgeFailure {
    Unmoved(anyhow::Error),
    Retained { path: PathBuf, error: anyhow::Error },
}

/// Move the exact inspected directory into private quarantine before deletion.
/// Keep anything whose identity or contents cannot be re-established there.
pub(crate) fn purge_directory(
    path: &Path,
    identity: &DirectoryIdentity,
    check_container: impl Fn() -> Result<()>,
    verify: impl FnOnce(&Path) -> Result<()>,
) -> std::result::Result<(), PurgeFailure> {
    let stage = (|| {
        let parent = path.parent().context("cache has no parent")?;
        check_container()?;
        let stage = private_stage(parent, QUARANTINE_PREFIX)?.keep();
        let stage_id = directory_identity(&stage)?;
        let name = path.file_name().context("cache has no name")?;
        let staged_path = stage.join(name);
        let check_stage = || -> Result<()> {
            check_container()?;
            private_directory(&stage)?;
            ensure!(
                directory_identity(&stage)? == stage_id,
                "cache quarantine directory changed"
            );
            Ok(())
        };
        let move_result = (|| {
            check_stage()?;
            ensure!(
                directory_identity(path)? == *identity,
                "cache directory changed before quarantine"
            );
            rename_exclusive(path, &staged_path)?;
            Ok(())
        })();
        if let Err(error) = move_result {
            if check_stage().is_ok() {
                let _ = fs::remove_dir(&stage);
            }
            return Err(error);
        }
        Ok((stage, stage_id, staged_path))
    })()
    .map_err(PurgeFailure::Unmoved)?;
    let (directory, quarantine_id, moved) = stage;
    let check = || -> Result<()> {
        check_container()?;
        private_directory(&directory)?;
        ensure!(
            directory_identity(&directory)? == quarantine_id
                && directory_identity(&moved)? == *identity,
            "cache directory changed during quarantine"
        );
        ensure!(
            !exists(path)?,
            "cache path was repopulated during quarantine"
        );
        Ok(())
    };
    let removal = (|| {
        check()?;
        verify(&moved)?;
        check()?;
        fs::remove_dir_all(&moved).context("remove the verified quarantined cache")?;
        Ok(())
    })();
    match removal {
        Ok(()) => {
            // A leftover entry in the quarantine container is never removed here.
            if check_container().is_ok()
                && directory_identity(&directory).is_ok_and(|id| id == quarantine_id)
            {
                let _ = fs::remove_dir(directory);
            }
            Ok(())
        }
        Err(error) => Err(PurgeFailure::Retained { path: moved, error }),
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod tests {
    use super::*;
    use crate::{engine::gc::purge_caches, git};

    #[test]
    fn existing_root_cannot_be_adopted_even_with_a_copied_marker() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        git::bytes(tmp.path(), &["init", "-q", "--template="])?;
        let (project, _lock) = Project::open(tmp.path())?;
        let cache = project.state.join("cache");
        let npm = cache.join("npm");
        prepare_package_roots(&project)?;
        let saved = cache.join("displaced-npm");
        fs::write(npm.join("old"), "old install")?;
        rename_exclusive(&npm, &saved)?;
        registry::dir(&npm)?;
        fs::copy(saved.join(OWNER), npm.join(OWNER))?;
        fs::write(npm.join("private"), "user data")?;
        assert!(prepare_package_roots(&project).is_err());

        let result = purge_caches(&project, true)?;
        assert!(result["blocked"].as_array().unwrap().iter().any(|entry| {
            entry["path"] == serde_json::json!(npm)
                && entry["reason"]["message"].as_str().is_some_and(|message| {
                    message.contains("package cache ownership is unverified")
                })
        }));
        assert!(
            !result["removed"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(npm))
        );
        assert_eq!(fs::read_to_string(npm.join("private"))?, "user data");
        assert_eq!(fs::read_to_string(saved.join("old"))?, "old install");
        verify_package(&project, "npm", &saved)?;
        Ok(())
    }

    #[test]
    fn purge_reports_a_previously_retained_quarantine_without_collecting_it() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        git::bytes(tmp.path(), &["init", "-q", "--template="])?;
        let (project, _lock) = Project::open(tmp.path())?;
        prepare_package_roots(&project)?;
        let path = project.state.join("cache/npm");
        fs::write(path.join("entry"), "captured contents")?;
        let identity = directory_identity(&path)?;
        let outcome = purge_directory(
            &path,
            &identity,
            || Ok(()),
            |moved| {
                fs::write(moved.join("entry"), "edited during purge")?;
                registry::dir(&path)?;
                fs::write(path.join("private"), "replacement")?;
                anyhow::bail!("payload changed")
            },
        );
        let Err(PurgeFailure::Retained { path: moved, .. }) = outcome else {
            panic!("changed cache must be retained after quarantine");
        };
        assert_eq!(
            fs::read_to_string(moved.join("entry"))?,
            "edited during purge"
        );
        assert_eq!(fs::read_to_string(path.join("private"))?, "replacement");
        let result = purge_caches(&project, true)?;
        let stage = moved.parent().unwrap();
        assert_eq!(result["retained"][0]["path"], serde_json::json!(stage));
        assert!(moved.exists());
        assert!(path.exists());
        assert!(
            !result["removed"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(path))
        );
        Ok(())
    }

    #[test]
    fn purge_preserves_replacements_before_and_after_quarantine() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("managed");
        let stored = tmp.path().join("stored");
        registry::dir(&path)?;
        fs::write(path.join("entry"), "verified")?;
        let identity = directory_identity(&path)?;
        rename_exclusive(&path, &stored)?;
        registry::dir(&path)?;
        fs::write(path.join("entry"), "different directory")?;
        assert!(matches!(
            purge_directory(&path, &identity, || Ok(()), |_| panic!("wrong identity")),
            Err(PurgeFailure::Unmoved(_))
        ));
        let result = purge_directory(
            &stored,
            &identity,
            || Ok(()),
            |moved| {
                ensure!(fs::read_to_string(moved.join("entry"))? == "verified");
                registry::dir(&stored)?;
                fs::write(stored.join("entry"), "new arrival")?;
                Ok(())
            },
        );
        let Err(PurgeFailure::Retained { path: moved, .. }) = result else {
            panic!("repopulated cache path must retain the quarantined directory");
        };
        assert_eq!(fs::read_to_string(moved.join("entry"))?, "verified");
        assert_eq!(fs::read_to_string(stored.join("entry"))?, "new arrival");
        assert_eq!(
            fs::read_to_string(path.join("entry"))?,
            "different directory"
        );
        Ok(())
    }

    #[test]
    fn quarantine_never_replaces_an_entry_that_appears_before_rename() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("owned");
        registry::dir(&path)?;
        fs::write(path.join("entry"), "original")?;
        let identity = directory_identity(&path)?;
        let check_container = || -> Result<()> {
            for entry in fs::read_dir(tmp.path())? {
                let entry = entry?;
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(QUARANTINE_PREFIX)
                {
                    let occupied = entry.path().join("owned");
                    if !exists(&occupied)? {
                        registry::dir(&occupied)?;
                    }
                }
            }
            Ok(())
        };
        assert!(matches!(
            purge_directory(&path, &identity, check_container, |_| panic!(
                "unexpected move"
            )),
            Err(PurgeFailure::Unmoved(_))
        ));
        assert_eq!(fs::read_to_string(path.join("entry"))?, "original");
        let quarantine = fs::read_dir(tmp.path())?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(QUARANTINE_PREFIX)
            })
            .context("collision was not retained")?;
        assert!(
            fs::read_dir(quarantine.path().join("owned"))?
                .next()
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn successful_purge_can_recreate_fresh_package_bindings() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        git::bytes(tmp.path(), &["init", "-q", "--template="])?;
        let (project, _lock) = Project::open(tmp.path())?;
        prepare_package_roots(&project)?;
        let npm = project.state.join("cache/npm");
        let original = verify_package(&project, "npm", &npm)?;
        assert_eq!(purge_caches(&project, true)?["purged"], true);
        assert!(!npm.exists());
        prepare_package_roots(&project)?;
        assert_ne!(verify_package(&project, "npm", &npm)?, original);
        Ok(())
    }
}

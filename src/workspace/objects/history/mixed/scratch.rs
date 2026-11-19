//! Keep raw donor bytes out of named transfer artifacts.

use anyhow::Result;
use std::{fs::File, path::Path};

#[cfg(target_os = "linux")]
pub(super) fn create(source: &File, _donor: &Path, scratch: &Path) -> Result<File> {
    use anyhow::ensure;
    use std::{io, os::fd::AsRawFd, os::unix::fs::MetadataExt};

    let file = tempfile::tempfile_in(scratch)?;
    // tempfile can fall back from O_TMPFILE to opening and unlinking a name.
    ensure!(file.metadata()?.nlink() == 0, "Git scratch is still linked");
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(file)
}

#[cfg(target_os = "macos")]
pub(super) fn create(source: &File, donor: &Path, _scratch: &Path) -> Result<File> {
    use super::{clone_file, open_regular};
    use anyhow::{Context, ensure};
    use std::{
        fs::{self, OpenOptions, Permissions},
        os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    };

    let parent = donor.parent().context("missing donor pack directory")?;
    let directory = fs::symlink_metadata(parent)?;
    ensure!(
        directory.is_dir()
            && directory.uid() == unsafe { libc::geteuid() }
            && directory.mode() & 0o022 == 0,
        "donor pack directory is not privately writable"
    );
    let directory = temporary_pack_dir(parent)?.keep();
    let path = directory.join("donor.pack");
    let result = (|| -> Result<File> {
        clone_file(source, &path)?;
        let pinned = open_regular(&path)?;
        let identity = pinned.metadata()?;
        let original = source.metadata()?;
        ensure!(
            identity.nlink() == 1
                && (identity.dev(), identity.ino()) != (original.dev(), original.ino()),
            "expected an independent donor clone"
        );
        pinned.set_permissions(Permissions::from_mode(0o600))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        let opened = file.metadata()?;
        ensure!(
            (opened.dev(), opened.ino()) == (identity.dev(), identity.ino()),
            "donor clone changed before opening"
        );
        fs::remove_file(&path)?;
        ensure!(file.metadata()?.nlink() == 0, "Git scratch is still linked");
        Ok(file)
    })();
    // Only remove an empty directory here. Git prunes interrupted scratch.
    let _ = fs::remove_dir(directory);
    result
}

#[cfg(target_os = "macos")]
fn temporary_pack_dir(parent: &Path) -> Result<tempfile::TempDir> {
    use std::{fs::Permissions, os::unix::fs::PermissionsExt};

    // Git's prune removes stale tmp_* files and directories in objects/pack.
    Ok(tempfile::Builder::new()
        .prefix("tmp_pack_graft_")
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(parent)?)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn create(_source: &File, _donor: &Path, _scratch: &Path) -> Result<File> {
    anyhow::bail!("anonymous copy-on-write scratch is unsupported")
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use crate::workspace::objects::{clone_file, open_regular};
    use std::{fs, os::unix::fs::MetadataExt};

    #[test]
    fn donor_clone_has_no_name_until_sanitized() -> Result<()> {
        let root = tempfile::tempdir()?;
        let packs = root.path().join("packs");
        let staging = root.path().join("staging");
        fs::create_dir(&packs)?;
        fs::create_dir(&staging)?;
        let donor = packs.join("donor.pack");
        fs::write(&donor, b"retained bytes and other history")?;
        let source = open_regular(&donor)?;
        let probe = root.path().join("probe");
        if let Err(error) = clone_file(&source, &probe) {
            match error.raw_os_error() {
                Some(libc::ENOTSUP | libc::EINVAL | libc::ENOSYS | libc::EXDEV | libc::ENOTTY) => {
                    return Ok(());
                }
                _ => return Err(error.into()),
            }
        }

        let file = create(&source, &donor, &staging)?;
        assert_eq!(file.metadata()?.nlink(), 0);
        assert_eq!(fs::read_dir(&packs)?.count(), 1);
        assert_eq!(fs::read_dir(&staging)?.count(), 0);
        file.set_len(14)?;
        let output = staging.join("filtered.pack");
        clone_file(&file, &output)?;
        assert_eq!(fs::read(output)?, b"retained bytes");
        assert_eq!(fs::read(donor)?, b"retained bytes and other history");
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn git_prunes_expired_scratch_directories() -> Result<()> {
        use crate::workspace::objects::run;
        use std::{fs::FileTimes, time::UNIX_EPOCH};

        let root = tempfile::tempdir()?;
        run(root.path(), &["init", "-q", "--template="])?;
        let packs = root.path().join(".git/objects/pack");
        let old = temporary_pack_dir(&packs)?.keep();
        fs::write(old.join("donor.pack"), b"interrupted scratch")?;
        File::open(&old)?.set_times(FileTimes::new().set_modified(UNIX_EPOCH))?;
        let fresh = temporary_pack_dir(&packs)?;

        run(root.path(), &["prune", "--expire=2000-01-01"])?;
        assert!(!old.exists());
        assert!(fresh.path().is_dir());
        Ok(())
    }
}

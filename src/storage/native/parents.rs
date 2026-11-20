use super::{invalid, open_at, stat_at};
use std::{
    ffi::{CString, OsString},
    fs::{self, File},
    io,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path},
};

const MAX_DEPTH: usize = 6;

pub(in crate::storage) fn available() -> bool {
    // Four workers each traverse two roots. Leave space for Git and normal I/O.
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    limit >= 128 && fs::read_dir("/dev/fd").is_ok_and(|fds| fds.count() + 96 < limit as usize)
}

struct Ancestor {
    name: OsString,
    directory: File,
    inode: u64,
}

#[derive(Default)]
pub(super) struct Ancestors(Vec<Ancestor>);

impl Ancestors {
    pub(super) fn prefix<'a>(
        &mut self,
        root: &File,
        device: u64,
        path: &'a Path,
    ) -> io::Result<Option<(File, &'a Path)>> {
        let mut parent = root;
        let mut remaining = path.components();
        let mut depth = 0;
        while depth < MAX_DEPTH {
            let mut next = remaining.clone();
            let Some(Component::Normal(part)) = next.next() else {
                break;
            };
            if next.as_path().as_os_str().is_empty() {
                break;
            }
            // Each cached edge must still name the same pinned directory.
            let cached = self.0.get(depth).filter(|entry| entry.name == part);
            let valid = if let Some(entry) = cached {
                let name = CString::new(part.as_bytes())?;
                stat_at(parent, &name)
                    .is_ok_and(|s| s.st_dev as u64 == device && s.st_ino == entry.inode)
            } else {
                false
            };
            if !valid {
                let child = match open_at(parent, part, libc::O_RDONLY | libc::O_DIRECTORY) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                    Err(error) if error.raw_os_error() == Some(libc::EMFILE) => {
                        self.0.clear();
                        return Ok(None);
                    }
                    Err(error) => return Err(error),
                };
                let metadata = child.metadata()?;
                if metadata.dev() != device {
                    return Err(invalid("mounted checkout directory"));
                }
                self.0.truncate(depth);
                self.0.push(Ancestor {
                    name: part.to_owned(),
                    directory: child,
                    inode: metadata.ino(),
                });
            }
            parent = &self.0[depth].directory;
            remaining = next;
            depth += 1;
        }
        if depth == 0 {
            return Ok(None);
        }
        match parent.try_clone() {
            Ok(fd) => Ok(Some((fd, remaining.as_path()))),
            Err(error) if error.raw_os_error() == Some(libc::EMFILE) => {
                self.0.clear();
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Root;
    use std::{fs, io, os::unix::fs::symlink, path::Path};

    #[test]
    fn new_sibling_uses_the_current_ancestors() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().canonicalize()?;
        fs::create_dir_all(path.join("tree/parent/one"))?;
        let worker = Root::open(&path)?.checkout_worker(true)?;
        worker.create_parent(Path::new("tree/parent/one/file"))?;

        fs::rename(path.join("tree/parent"), path.join("old-parent"))?;
        worker.create_parent(Path::new("tree/parent/two/file"))?;
        assert!(path.join("tree/parent/two").is_dir());
        assert!(!path.join("old-parent/two").exists());

        fs::rename(path.join("tree"), path.join("old-tree"))?;
        symlink(path.join("old-tree"), path.join("tree"))?;
        assert!(
            worker
                .create_parent(Path::new("tree/parent/three/file"))
                .is_err()
        );
        assert!(!path.join("old-tree/parent/three").exists());
        Ok(())
    }
}

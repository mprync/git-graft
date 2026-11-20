//! Hash source files during workspace reservation. Checkout still decides which paths may be cloned.
use super::{donor_entries, donor_listing, donor_prefix, unambiguous_paths, verified_blob};
use crate::storage::{Entry, cache::Stamp, native::Root};
use std::{
    fs::{self, File, Metadata},
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{Builder, JoinHandle},
};

const MAX_PATHS: usize = 4096;
const READERS: usize = 2;
const MAX_FILE_BYTES: u64 = 4 << 20;
const READ_BUDGET: u64 = 64 << 20;

pub(super) struct Verified {
    file: File,
    metadata: Metadata,
    stamp: Stamp,
}

impl Verified {
    pub(super) fn reopen(&self, donor: &Root, path: &Path) -> Option<(File, Metadata)> {
        // Holding the original descriptor prevents inode reuse between verification and lookup.
        if !self.stamp.matches_stat(&donor.regular_stat(path).ok()?) {
            return None;
        }
        Some((self.file.try_clone().ok()?, self.metadata.clone()))
    }
}

struct Pending {
    slots: Vec<OnceLock<Verified>>,
    cancelled: AtomicBool,
}

pub(in crate::storage) struct EarlyDonors {
    pending: Arc<Pending>,
    task: Option<JoinHandle<()>>,
}

impl EarlyDonors {
    pub(in crate::storage) fn start(primary: &Path, entries: &[Entry]) -> Option<Self> {
        if !(128..=MAX_PATHS).contains(&entries.len()) || !unambiguous_paths(entries) {
            return None;
        }
        // Reserve descriptors for Git, the clone workers and registry operations.
        let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
        let open = fs::read_dir("/dev/fd").ok()?.count();
        if limit < (open + entries.len() + 160) as _ {
            return None;
        }
        let primary = dunce::canonicalize(primary).ok()?;
        let entries = entries.to_vec();
        let pending = Arc::new(Pending {
            slots: (0..entries.len()).map(|_| OnceLock::new()).collect(),
            cancelled: AtomicBool::new(false),
        });
        let worker = Arc::clone(&pending);
        let task = Builder::new()
            .spawn(move || {
                if crate::git::optional::collect(&mut donor_prefix(&primary), 2, &worker.cancelled)
                    .as_deref()
                    != Some(b"\n")
                {
                    return;
                }
                let Some(index) = crate::git::optional::collect(
                    &mut donor_listing(&primary),
                    4 << 20,
                    &worker.cancelled,
                ) else {
                    return;
                };
                let tracked = donor_entries(&index);
                std::thread::scope(|scope| {
                    let batch = entries.len().div_ceil(READERS);
                    for (entries, slots) in entries.chunks(batch).zip(worker.slots.chunks(batch)) {
                        let (primary, tracked, cancelled) = (&primary, &tracked, &worker.cancelled);
                        let _ = Builder::new().spawn_scoped(scope, move || {
                            let Ok(donor) =
                                Root::open(primary).and_then(|root| root.checkout_worker(true))
                            else {
                                return;
                            };
                            let mut budget = READ_BUDGET / READERS as u64;
                            let mut buffer = [0; 64 * 1024];
                            for (entry, slot) in entries.iter().zip(slots) {
                                if cancelled.load(Ordering::Relaxed) {
                                    break;
                                }
                                let mode: &[u8] = if entry.mode == 0o100755 {
                                    b"100755"
                                } else {
                                    b"100644"
                                };
                                if !entry.regular()
                                    || tracked.get(entry.name.as_slice())
                                        != Some(&(mode, entry.blob.as_bytes()))
                                {
                                    continue;
                                }
                                let Ok(path) = entry.path() else { continue };
                                let Ok((mut file, metadata)) = donor.regular_metadata(path) else {
                                    continue;
                                };
                                if metadata.len() > MAX_FILE_BYTES || metadata.len() > budget {
                                    continue;
                                }
                                budget -= metadata.len();
                                let stamp = Stamp::from(&metadata);
                                if verified_blob(
                                    &mut file,
                                    &metadata,
                                    &mut buffer,
                                    &entry.blob,
                                    entry.blob.len() == 64,
                                ) && file.metadata().is_ok_and(|now| Stamp::from(&now) == stamp)
                                {
                                    let _ = slot.set(Verified {
                                        file,
                                        metadata,
                                        stamp,
                                    });
                                }
                            }
                        });
                    }
                });
            })
            .ok()?;
        Some(Self {
            pending,
            task: Some(task),
        })
    }

    pub(super) fn batch(&self, start: usize, len: usize) -> &[OnceLock<Verified>] {
        &self.pending.slots[start..start + len]
    }
}

impl Drop for EarlyDonors {
    fn drop(&mut self) {
        self.pending.cancelled.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn pinned_file_requires_the_same_path_and_stamp() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("file");
        fs::write(&path, b"before")?;
        let root = Root::open(&temp.path().canonicalize()?)?;
        let name = Path::new("file");
        let (file, metadata) = root.regular_metadata(name)?;
        let pinned = Verified {
            file,
            stamp: Stamp::from(&metadata),
            metadata,
        };
        assert!(pinned.reopen(&root, name).is_some());
        fs::write(&path, b"edited")?;
        assert!(pinned.reopen(&root, name).is_none());
        fs::rename(&path, temp.path().join("moved"))?;
        fs::write(&path, b"before")?;
        assert!(pinned.reopen(&root, name).is_none());
        fs::remove_file(&path)?;
        symlink("moved", &path)?;
        assert!(pinned.reopen(&root, name).is_none());
        Ok(())
    }
}

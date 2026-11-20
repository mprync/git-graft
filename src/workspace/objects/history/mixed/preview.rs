//! Verify a tentative v2 index with native Git while traversing the same anonymous pack.
use super::{ObjectId, Pack, Result, cache, ensure, transfer_git};
use anyhow::Context;
use std::{
    fs::File,
    io::{Seek, Write},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
};
mod scratch;
mod view;

pub(in super::super) struct Preview {
    name: String,
    input: File,
    index: Vec<u8>,
}

impl Preview {
    pub(super) fn new(
        input: File,
        ids: &[ObjectId],
        offsets: Vec<u32>,
        bytes: &[u8],
        trailer: &[u8],
    ) -> Result<Self> {
        let mut rows: Vec<_> = (0..offsets.len()).collect();
        rows.sort_unstable_by_key(|&row| offsets[row]);
        let mut crc = vec![0u32; rows.len()];
        let mut end = bytes.len();
        for row in rows.into_iter().rev() {
            let start = offsets[row] as usize;
            ensure!(start < end, "invalid preview offsets");
            let mut digest = flate2::Crc::new();
            digest.update(&bytes[start..end]);
            crc[row] = digest.sum();
            end = start;
        }
        ensure!(end == 12, "unexpected first pack offset");
        let mut index =
            Vec::with_capacity(1032 + ids.len() * (trailer.len() + 8) + trailer.len() * 2);
        index.extend_from_slice(b"\xfftOc\x00\x00\x00\x02");
        let mut fan = [0u32; 256];
        for id in ids {
            fan[usize::from(id.0[0])] += 1;
        }
        let mut total = 0;
        for count in fan {
            total += count;
            index.extend_from_slice(&total.to_be_bytes());
        }
        for id in ids {
            index.extend_from_slice(&id.0[..trailer.len()]);
        }
        for n in crc.into_iter().chain(offsets) {
            index.extend_from_slice(&n.to_be_bytes());
        }
        index.extend_from_slice(trailer);
        // The pack trailer from `mixed::prepare` is always a supported width.
        let digest = super::trailer_digest(trailer.len(), &index)?;
        index.extend_from_slice(&digest);
        let name: String = trailer.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Self { name, input, index })
    }

    fn anonymous_index(&self, parent: &Path) -> Result<File> {
        let mut index = tempfile::tempfile_in(parent)?;
        ensure!(index.metadata()?.nlink() == 0, "expected anonymous index");
        index.write_all(&self.index)?;
        let mut permissions = index.metadata()?.permissions();
        permissions.set_readonly(true);
        index.set_permissions(permissions)?;
        index.rewind()?;
        Ok(index)
    }

    fn pair(&self, index: &File) -> [(PathBuf, i32); 2] {
        [
            (format!("pack-{}.pack", self.name).into(), 0),
            (format!("pack-{}.idx", self.name).into(), index.as_raw_fd()),
        ]
    }

    pub(in super::super) fn verify_mixed(
        &self,
        root: &Path,
        stage: &Path,
        width: usize,
        mut prepared: super::Prepared,
    ) -> Result<(Pack, u64)> {
        let directory = prepared.scratch.path();
        let index = self.anonymous_index(directory)?;
        let links = self.pair(&index);
        prepared.file.rewind()?;
        view::with_files(root, width, &prepared.file, &links, |view| {
            view.verify(&links[0].0, |git| {
                let threads = prepared.index_threads(root, git);
                git.args(["index-pack", "--verify", "--index-version=2", threads]);
            })
        })?;
        let pack = Pack::at(stage, &self.name, width)?;
        super::clone_file(&prepared.file, &pack.pack)?;
        super::clone_file(&index, &pack.index)?;
        Ok((pack, prepared.unchanged_bytes))
    }

    pub(in super::super) fn walk(
        &self,
        root: &Path,
        stage: &Path,
        packs: &[&Pack],
        oid: &str,
    ) -> Result<Count> {
        ensure!(packs.len() <= 16, "too many companion packs");
        let parent = stage
            .parent()
            .and_then(Path::parent)
            .context("missing private staging")?;
        let index = self.anonymous_index(parent)?;
        let mut links = self.pair(&index).to_vec();
        let mut companions = Vec::new();
        for path in packs.iter().flat_map(|pair| [&pair.pack, &pair.index]) {
            let file = super::open_regular(path)?;
            let stamp = cache::Stamp::read(&file)?;
            links.push((
                path.file_name().context("missing pack name")?.into(),
                file.as_raw_fd(),
            ));
            companions.push((path.clone(), file, stamp));
        }
        let count = view::with_files(root, oid.len() / 2, &self.input, &links, |view| {
            for pack in packs {
                view.verify(&pack.pack, |git| {
                    git.args(["index-pack", "--verify"]);
                })?;
            }
            for (_, file, _) in &mut companions {
                file.rewind()?;
            }
            (&index).rewind()?;
            (&self.input).rewind()?;
            super::super::graph_count(view.command(true)?, oid)
        })?;
        Ok(Count {
            count,
            companions,
            index,
        })
    }
}

pub(in super::super) struct Count {
    count: u64,
    companions: Vec<(PathBuf, File, cache::Stamp)>,
    index: File,
}

impl Count {
    pub(in super::super) fn finish(
        self,
        pack: &Pack,
        view: &Preview,
    ) -> Result<(u64, super::super::VerifiedInputs)> {
        let mut inputs = super::super::VerifiedInputs::default();
        let mut total = 0u64;
        for (path, file, before) in self.companions {
            ensure!(
                before == cache::Stamp::read(&file)?,
                "verified companion changed"
            );
            if path.extension().is_some_and(|e| e == "idx") {
                (&file).rewind()?;
                total += super::index_count(&mut &file)? as u64;
            }
            inputs.0.push((path, file, Some(before)));
        }
        (&self.index).rewind()?;
        total += super::index_count(&mut &self.index)? as u64;
        ensure!(total == self.count, "preview has extra or missing objects");
        ensure!(
            view.input.metadata()?.nlink() == 0 && self.index.metadata()?.nlink() == 0,
            "anonymous pack acquired a name"
        );
        inputs
            .0
            .push((pack.pack.clone(), view.input.try_clone()?, None));
        inputs.0.push((pack.index.clone(), self.index, None));
        Ok((self.count, inputs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::objects::run;

    #[test]
    fn git_uses_the_private_context_when_its_parent_is_moved() -> Result<()> {
        use std::fs;
        let root = tempfile::tempdir()?;
        run(
            root.path(),
            &["init", "-q", "--template=", "--object-format=sha1"],
        )?;
        let admin = root.path().join(".git");
        let moved = root.path().join("moved");
        let input = tempfile::tempfile()?;
        let replacement = view::with_files(root.path(), 32, &input, &[], |view| {
            let mut command = view.command(true)?;
            let pinned = PathBuf::from(
                command
                    .get_envs()
                    .find_map(|(key, value)| (key == "GIT_DIR").then_some(value).flatten())
                    .context("missing private Git directory")?,
            );
            let entry = fs::read_dir(&admin)?
                .collect::<std::io::Result<Vec<_>>>()?
                .into_iter()
                .find(|entry| {
                    entry.metadata().is_ok_and(|m| {
                        fs::metadata(&pinned)
                            .is_ok_and(|p| (m.dev(), m.ino()) == (p.dev(), p.ino()))
                    })
                })
                .context("missing owned scratch")?;
            fs::rename(&admin, &moved)?;
            let replacement = admin.join(entry.file_name());
            fs::create_dir_all(&replacement)?;
            fs::write(replacement.join("user-note"), b"keep")?;
            command.args(["rev-parse", "--show-object-format"]);
            ensure!(
                super::super::checked(command)?.trim_ascii_end() == b"sha256",
                "Git used the workspace repository context"
            );
            Ok(replacement)
        })?;
        assert_eq!(fs::read(replacement.join("user-note"))?, b"keep");
        assert!(!moved.join(replacement.file_name().unwrap()).exists());
        Ok(())
    }

    #[test]
    fn native_verifier_checks_the_proposed_index_before_naming_objects() -> Result<()> {
        for (format, width) in [("sha1", 20), ("sha256", 32)] {
            let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize()?)?;
            run(
                root.path(),
                &[
                    "init",
                    "-q",
                    "--template=",
                    &format!("--object-format={format}"),
                ],
            )?;
            std::fs::write(root.path().join("tracked"), b"selected contents")?;
            let raw = run(root.path(), &["hash-object", "-w", "tracked"])?;
            let id = ObjectId::from_hex(raw.trim_ascii_end(), width)?;
            let mut list = tempfile::tempfile_in(root.path())?;
            list.write_all(&raw)?;
            list.rewind()?;
            let mut git = transfer_git(root.path());
            git.args(["pack-objects", "--stdout"]).stdin(list);
            let bytes = super::super::checked(git)?;
            let (payload, trailer) = bytes.split_at(bytes.len() - width);
            for proposed in [id, ObjectId([0; 32])] {
                let stage = tempfile::tempdir_in(root.path())?;
                let mut input = tempfile::tempfile_in(root.path())?;
                input.write_all(&bytes)?;
                input.rewind()?;
                let preview =
                    Preview::new(input.try_clone()?, &[proposed], vec![12], payload, trailer)?;
                let candidate = super::super::Prepared {
                    file: input,
                    scratch: tempfile::tempdir_in(root.path())?,
                    ids: vec![proposed],
                    unchanged_bytes: 0,
                    preview: None,
                };
                let result = preview.verify_mixed(root.path(), stage.path(), width, candidate);
                if proposed == id {
                    let (pack, _) = result?;
                    assert_eq!(std::fs::read(pack.pack)?, bytes);
                } else {
                    assert!(result.is_err());
                    assert_eq!(std::fs::read_dir(stage.path())?.count(), 0);
                }
            }
        }
        Ok(())
    }
}

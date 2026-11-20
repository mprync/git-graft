//! Run Git with a private repository context and descriptor-backed packs.
use super::{scratch::Scratch, transfer_git};
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    os::{fd::RawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(super) struct View<'a> {
    scratch: Scratch,
    inherited: Vec<RawFd>,
    input: &'a File,
}

pub(super) fn with_files<T>(
    root: &Path,
    width: usize,
    input: &File,
    links: &[(PathBuf, RawFd)],
    operation: impl FnOnce(&View<'_>) -> Result<T>,
) -> Result<T> {
    let config: &[u8] = match width {
        20 => b"[core]\n bare = true\n repositoryFormatVersion = 0\n",
        32 => b"[core]\n bare = true\n repositoryFormatVersion = 1\n[extensions]\n objectFormat = sha256\n",
        _ => anyhow::bail!("unsupported Git object format"),
    };
    let mut scratch = Scratch::new(&root.join(".git"))?;
    scratch.file(Path::new("config"), config)?;
    scratch.file(Path::new("HEAD"), b"ref: refs/heads/unborn\n")?;
    scratch.directory(Path::new("refs"))?;
    scratch.directory(Path::new("empty"))?;
    scratch.directory(Path::new("pack"))?;
    let mut inherited = Vec::new();
    for (name, fd) in links {
        ensure!(
            name.file_name() == Some(name.as_os_str()),
            "invalid preview filename"
        );
        if *fd != 0 {
            ensure!(*fd > 2, "standard descriptor collision");
            inherited.push(*fd);
        }
        scratch.link(
            &Path::new("pack").join(name),
            Path::new(&format!("/dev/fd/{fd}")),
        )?;
    }
    scratch.seal()?;
    let view = View {
        scratch,
        input,
        inherited,
    };
    let result = operation(&view)?;
    view.scratch.check()?;
    Ok(result)
}

impl View<'_> {
    pub(super) fn command(&self, walk: bool) -> Result<Command> {
        let admin = self.scratch.path();
        let mut git = transfer_git(Path::new("/"));
        git.env("GIT_DIR", admin)
            .env("GIT_COMMON_DIR", admin)
            .env(
                "GIT_OBJECT_DIRECTORY",
                if walk {
                    admin.to_path_buf()
                } else {
                    admin.join("empty")
                },
            )
            .env("GIT_SHALLOW_FILE", "")
            .stdin(Stdio::from(self.input.try_clone()?));
        let fds = self.inherited.clone();
        // Only the child inherits these descriptors. fcntl is async-signal-safe.
        unsafe {
            git.pre_exec(move || {
                for fd in &fds {
                    if libc::fcntl(*fd, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(git)
    }

    pub(super) fn verify(&self, name: &Path, configure: impl FnOnce(&mut Command)) -> Result<()> {
        let mut git = self.command(false)?;
        configure(&mut git);
        git.arg(
            self.scratch
                .path()
                .join("pack")
                .join(name.file_name().context("missing pack name")?),
        );
        super::super::checked(git)?;
        Ok(())
    }
}

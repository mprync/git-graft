//! The project's Graft configuration, read from two JSON files:
//! `.graftconfig`, which a project commits and shares, and
//! `.graftconfig.local`, the person's own. Each may set the `include`
//! patterns a new graft copies by default and the `hooks` that run when a
//! graft is added, about to be archived or restored.
//!
//! The files are read from the root of the primary checkout. When neither is
//! there, they are read from the nearest directory above it that holds
//! either, where a `repositories` object sets the same keys for each
//! repository below that directory, by its path relative to the directory.
//! This lets one directory that holds several repositories configure them
//! all. A hook there may name its program by a path relative to that
//! directory, which no graft of the repository contains.
//!
//! The local file overrides the shared one setting by setting. Include lists
//! are joined, shared patterns first, and a local `!pattern` removes the same
//! shared pattern. Hooks are keyed by name: a local hook replaces the shared
//! hook of that name whole, a local `null` turns it off, and a new name adds
//! a hook after the shared ones.
//!
//! Both files are read from the primary checkout or a directory above it,
//! never from a graft, so what a graft changes in its own copy does not
//! change what Graft runs.
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::{self, MapAccess, Visitor};
use serde_json::{Value, json};

use crate::workspace::hook::{self, Event};

/// The configuration a project commits and shares.
pub(crate) const SHARED: &str = ".graftconfig";
/// The person's own configuration, which overrides the shared one.
pub(crate) const LOCAL: &str = ".graftconfig.local";
const MAX_BYTES: u64 = 1024 * 1024;
const MAX_NAME_BYTES: usize = 64;
/// Whether Graft checks who owns a configuration file above the checkout.
/// Where it does not, on Windows, a hook from such a file needs trust whether
/// Git tracks the file or not.
pub(crate) const OWNER_CHECKED: bool = cfg!(unix);
/// Whether paths that differ only in case name the same file, as they do on
/// the default filesystems of macOS and Windows.
const FOLD_CASE: bool = cfg!(any(windows, target_os = "macos"));

/// A hook as the configuration defines it: the program and arguments to run
/// for an event, and the file that defined it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hook {
    pub name: String,
    pub event: Event,
    pub argv: Vec<String>,
    /// The file that defined the hook, as reports name it: relative to the
    /// primary checkout, such as `.graftconfig` or `../.graftconfig`.
    pub file: String,
    /// Whether the hook comes from `.graftconfig.local`.
    pub local: bool,
    /// The program to start, when `argv[0]` is a path relative to the
    /// directory of the file that defined the hook.
    pub program: Option<PathBuf>,
    /// The directory that holds the file that defined the hook.
    pub dir: Option<PathBuf>,
}

impl Hook {
    pub(crate) fn describe(&self) -> Value {
        let mut described =
            json!({"name": self.name, "event": self.event, "argv": self.argv, "file": self.file});
        if let Some(program) = &self.program {
            described["program"] = json!(program);
        }
        described
    }
}

/// An entry of a configuration file's `remotes` list, as written: a remote's
/// name, or `!name`, which removes a shared one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Remote {
    pub name: String,
    /// The file that lists it, named as [`Hook::file`] names it.
    pub file: String,
    /// Whether it comes from `.graftconfig.local`.
    pub local: bool,
}

/// The effective configuration of a project: both files merged.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Config {
    /// The include patterns a graft copies when a request names none.
    pub include: Vec<String>,
    /// The files whose `include` lists supplied a pattern, named as
    /// [`Hook::file`] names them.
    pub include_files: Vec<String>,
    /// Every hook, in the order they run.
    pub hooks: Vec<Hook>,
    /// Every hook as each file defines it, before the local file overrides
    /// the shared one: the shared file's, then the local file's.
    pub defined: Vec<Hook>,
    /// The directory that holds the files the configuration was read from:
    /// the primary checkout or a directory above it. `None` when there are
    /// no files.
    pub dir: Option<PathBuf>,
    /// Whether that directory is above the primary checkout.
    pub above: bool,
    /// Each key under `repositories` in those files that names no repository
    /// below the directory, with the file that has it, so that a mistyped
    /// path can be reported rather than configure nothing unnoticed.
    pub missing_repositories: Vec<Value>,
    /// The remotes of the primary every new graft gets.
    pub remotes: Vec<String>,
    /// Every entry of the files' `remotes` lists as written: the shared
    /// file's, then the local file's.
    pub listed_remotes: Vec<Remote>,
}

impl Config {
    /// Read and merge the configuration of the checkout `primary`: the files
    /// at its root, or else those of the nearest directory above it that
    /// holds either, for the repository at `primary`. A missing file sets
    /// nothing. A bare repository has no configuration.
    ///
    /// The search above stops at the person's home directory, which it
    /// still reads, when the checkout is inside it, as Git's
    /// `GIT_CEILING_DIRECTORIES` stops its own search; otherwise it goes up
    /// to the root of the filesystem. A stray file in a directory that holds
    /// homes, such as `/Users`, or at the root, then cannot configure or
    /// break every repository in a home.
    pub(crate) fn read(primary: &Path) -> Result<Self> {
        Self::read_within(primary, home().as_deref())
    }

    /// [`Self::read`], with `home` as the person's home directory.
    fn read_within(primary: &Path, home: Option<&Path>) -> Result<Self> {
        if primary.join(".git").symlink_metadata().is_err() {
            return Ok(Self::default());
        }
        if present(primary, SHARED) || present(primary, LOCAL) {
            let shared = load(primary, SHARED, false)?;
            let local = load(primary, LOCAL, false)?;
            let names = Names {
                shared: SHARED.to_owned(),
                local: LOCAL.to_owned(),
                dir: primary,
                below: false,
            };
            let missing = missing(primary, &shared, &local, &names);
            let mut config = merge(
                shared.map(|file| file.own),
                local.map(|file| file.own),
                &names,
            )?;
            config.missing_repositories = missing;
            return Ok(config.placed(primary, false));
        }
        for (depth, dir) in searched(primary, home) {
            if !(present(dir, SHARED) || present(dir, LOCAL)) {
                continue;
            }
            let key = repository_key(primary, dir)?;
            let shared = load(dir, SHARED, true)?;
            let local = load(dir, LOCAL, true)?;
            let up = "../".repeat(depth);
            let names = Names {
                shared: format!("{up}{SHARED}"),
                local: format!("{up}{LOCAL}"),
                dir,
                below: true,
            };
            let missing = missing(dir, &shared, &local, &names);
            let mut config = merge(
                shared.and_then(|file| file.section(&key)),
                local.and_then(|file| file.section(&key)),
                &names,
            )?;
            config.missing_repositories = missing;
            return Ok(config.placed(dir, true));
        }
        Ok(Self::default())
    }

    /// Record `dir` as the directory that holds the files, and whether it is
    /// above the primary checkout.
    fn placed(mut self, dir: &Path, above: bool) -> Self {
        for hook in self.hooks.iter_mut().chain(self.defined.iter_mut()) {
            hook.dir = Some(dir.to_owned());
        }
        self.dir = Some(dir.to_owned());
        self.above = above;
        self
    }

    /// The hooks for `event`, in the order they run.
    pub(crate) fn hooks_for(&self, event: Event) -> Vec<Hook> {
        self.hooks
            .iter()
            .filter(|hook| hook.event == event)
            .cloned()
            .collect()
    }

    /// How `local-files` describes the include defaults.
    pub(crate) fn include_defaults(&self) -> Value {
        json!({"patterns": self.include, "files": self.include_files})
    }
}

/// The person's home directory, from `HOME` on Unix and `USERPROFILE` on
/// Windows, when that is an absolute path.
fn home() -> Option<PathBuf> {
    let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
}

/// The `turns` both files list, the shared file's first, each once.
fn turns(shared: &Option<File>, local: &Option<File>) -> Vec<String> {
    let mut turns: Vec<String> = Vec::new();
    for resource in [shared, local]
        .into_iter()
        .flatten()
        .flat_map(|file| &file.turns)
    {
        if !turns.contains(resource) {
            turns.push(resource.clone());
        }
    }
    turns
}

/// The `turns` the files in `dir` list, and the file that lists them: the
/// shared one when it lists any. `above` is as for [`load`].
fn listed_turns(dir: &Path, above: bool) -> Result<(Vec<String>, PathBuf)> {
    let shared = load(dir, SHARED, above)?;
    let local = load(dir, LOCAL, above)?;
    let name = if shared.as_ref().is_some_and(|file| !file.turns.is_empty()) {
        SHARED
    } else {
        LOCAL
    };
    Ok((turns(&shared, &local), dir.join(name)))
}

/// The shared resources whose turns the repository at `primary` keeps in a
/// directory above it, and that directory. They are the `turns` of the
/// nearest directory above, up to the same home-directory bound as the
/// configuration, whose files list any; they are read whether or not the
/// checkout has files of its own: those govern everything else, but cannot
/// take the repository out of a queue it shares. When that directory is
/// itself a checkout that keeps the same turns further up, they are kept
/// there, so a queue is never split between levels. `None` when no
/// directory above lists turns.
///
/// A checkout whose own files list `turns` as well must list the same
/// resources, or Graft would have to guess which list is meant.
pub(crate) fn shared_turns(primary: &Path) -> Result<Option<(PathBuf, Vec<String>)>> {
    shared_turns_within(primary, home().as_deref())
}

/// [`shared_turns`], with `home` as the person's home directory.
fn shared_turns_within(
    primary: &Path,
    home: Option<&Path>,
) -> Result<Option<(PathBuf, Vec<String>)>> {
    if primary.join(".git").symlink_metadata().is_err() {
        return Ok(None);
    }
    // A directory whose files list no turns is passed over, so a checkout or
    // directory configured for something else cannot split a queue kept
    // further up.
    let mut found = None;
    for (_, dir) in searched(primary, home) {
        if !(present(dir, SHARED) || present(dir, LOCAL)) {
            continue;
        }
        let (listed, file) = listed_turns(dir, true)?;
        if !listed.is_empty() {
            found = Some((dir, listed, file));
            break;
        }
    }
    let Some((dir, above, above_file)) = found else {
        return Ok(None);
    };
    if present(primary, SHARED) || present(primary, LOCAL) {
        let (own, own_file) = listed_turns(primary, false)?;
        let set = |list: &[String]| {
            let mut list = list.to_vec();
            list.sort();
            list
        };
        if !own.is_empty() && set(&own) != set(&above) {
            return Err(crate::api::fault(
                "invalid_config",
                format!(
                    "{} lists the turns {}, but {} above it lists {}. The directory above decides \
                     which turns a repository shares, so list the same resources in both files, \
                     or list them only above",
                    own_file.display(),
                    own.join(", "),
                    above_file.display(),
                    above.join(", ")
                ),
                json!({"file": own_file, "above": above_file}),
            ));
        }
    }
    Ok(Some(
        shared_turns_within(dir, home)?.unwrap_or_else(|| (dir.to_owned(), above)),
    ))
}

/// The directories above `primary` that Graft searches for a configuration,
/// nearest first, each with how many levels up it is: up to and including
/// `home` when `primary` is inside it, and up to the root otherwise.
fn searched<'a>(
    primary: &'a Path,
    home: Option<&Path>,
) -> impl Iterator<Item = (usize, &'a Path)> + use<'a> {
    let ceiling = home
        .map(|home| dunce::canonicalize(home).unwrap_or_else(|_| home.to_owned()))
        .filter(|home| inside(primary, home, FOLD_CASE));
    primary
        .ancestors()
        .enumerate()
        .skip(1)
        .take_while(move |(_, dir)| {
            ceiling
                .as_deref()
                .is_none_or(|home| inside(dir, home, FOLD_CASE))
        })
}

/// Whether `dir` has an entry called `name`, of any type, so that a file
/// Graft cannot read is reported rather than passed over.
fn present(dir: &Path, name: &str) -> bool {
    dir.join(name).symlink_metadata().is_ok()
}

/// The keys under `repositories` in the files `shared` and `local` in `dir`
/// that name no repository there: no `.git` at that path, as for a bare
/// repository, which has no configuration either.
fn missing(
    dir: &Path,
    shared: &Option<File>,
    local: &Option<File>,
    names: &Names<'_>,
) -> Vec<Value> {
    [(shared, &names.shared), (local, &names.local)]
        .into_iter()
        .flat_map(|(file, name)| {
            file.iter()
                .flat_map(|file| &file.repositories)
                .filter(|(key, _)| !present(&dir.join(key), ".git"))
                .map(move |(key, _)| json!({"file": name, "repository": key}))
        })
        .collect()
}

/// The key of the repository at `primary` in the `repositories` object of a
/// configuration in `dir`, which is above it: its path relative to `dir`,
/// with `/` between components.
fn repository_key(primary: &Path, dir: &Path) -> Result<String> {
    let relative = primary.strip_prefix(dir)?;
    let parts: Option<Vec<&str>> = relative
        .components()
        .map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect();
    let parts = parts.ok_or_else(|| {
        anyhow::anyhow!(
            "{} has no Unicode path relative to {}",
            primary.display(),
            dir.display()
        )
    })?;
    Ok(parts.join("/"))
}

/// The settings for one repository: at the top level of a file for the
/// repository whose root holds it, or in `repositories` for one below.
#[derive(Debug, Default)]
struct Part {
    include: Option<Vec<String>>,
    hooks: Option<Named>,
    remotes: Option<Vec<String>>,
}

/// A part read straight from the file, so the hooks keep their order: only
/// an object, with no key but `include` and `hooks`, each at most once.
impl<'de> Deserialize<'de> for Part {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Fields;
        impl<'de> Visitor<'de> for Fields {
            type Value = Part;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object of a repository's settings")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Part, A::Error> {
                let mut part = Part::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "include" if part.include.is_some() => {
                            return Err(de::Error::duplicate_field("include"));
                        }
                        "hooks" if part.hooks.is_some() => {
                            return Err(de::Error::duplicate_field("hooks"));
                        }
                        "remotes" if part.remotes.is_some() => {
                            return Err(de::Error::duplicate_field("remotes"));
                        }
                        "include" => part.include = Some(map.next_value()?),
                        "hooks" => part.hooks = Some(map.next_value()?),
                        "remotes" => part.remotes = Some(map.next_value()?),
                        other => {
                            return Err(de::Error::unknown_field(
                                other,
                                &["include", "hooks", "remotes"],
                            ));
                        }
                    }
                }
                Ok(part)
            }
        }
        deserializer.deserialize_map(Fields)
    }
}

/// One configuration file as written.
#[derive(Debug, Default)]
struct File {
    own: Part,
    repositories: Vec<(String, Part)>,
    turns: Vec<String>,
}

impl File {
    /// The settings the file holds for the repository `key` below it.
    fn section(self, key: &str) -> Option<Part> {
        self.repositories
            .into_iter()
            .find(|(known, _)| known == key)
            .map(|(_, part)| part)
    }
}

impl<'de> Deserialize<'de> for File {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Written {
            #[serde(default)]
            include: Option<Vec<String>>,
            #[serde(default)]
            hooks: Option<Named>,
            #[serde(default)]
            repositories: Option<Repositories>,
            #[serde(default)]
            turns: Option<Vec<String>>,
            remotes: Option<Vec<String>>,
        }
        let written = Written::deserialize(deserializer)?;
        Ok(File {
            own: Part {
                include: written.include,
                hooks: written.hooks,
                remotes: written.remotes,
            },
            repositories: written.repositories.map(|r| r.0).unwrap_or_default(),
            turns: written.turns.unwrap_or_default(),
        })
    }
}

/// A hook entry as written. `null` in the local file turns off the shared
/// hook of the same name.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    event: Event,
    argv: Vec<String>,
}

/// Read an object's entries in the order they are written, refusing a name
/// given twice and any entry `entry` refuses.
fn ordered<'de, A, T>(
    mut map: A,
    what: &str,
    entry: impl Fn(&str, Value) -> Result<T, String>,
) -> Result<Vec<(String, T)>, A::Error>
where
    A: MapAccess<'de>,
{
    let mut entries: Vec<(String, T)> = Vec::new();
    while let Some(name) = map.next_key::<String>()? {
        if entries.iter().any(|(known, _)| *known == name) {
            return Err(de::Error::custom(format!(
                "{what} {name:?} is defined twice"
            )));
        }
        let value = entry(&name, map.next_value::<Value>()?).map_err(de::Error::custom)?;
        entries.push((name, value));
    }
    Ok(entries)
}

/// The `hooks` object, in the order its names are written.
#[derive(Debug, Default)]
struct Named(Vec<(String, Option<Spec>)>);

impl<'de> Deserialize<'de> for Named {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Ordered;
        impl<'de> Visitor<'de> for Ordered {
            type Value = Named;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object of hooks keyed by name")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Named, A::Error> {
                // Read each entry as JSON first: only an object or null is a
                // hook entry, not the array a derived struct would accept.
                ordered(map, "hook", |name, value| match value {
                    Value::Null => Ok(None),
                    entry @ Value::Object(_) => Spec::deserialize(entry)
                        .map(Some)
                        .map_err(|error| error.to_string()),
                    other => Err(format!(
                        "hook {name:?} must be an object or null, not {other}"
                    )),
                })
                .map(Named)
            }
        }
        deserializer.deserialize_map(Ordered)
    }
}

/// The `repositories` object, in the order its paths are written.
#[derive(Debug, Default)]
struct Repositories(Vec<(String, Part)>);

impl<'de> Deserialize<'de> for Repositories {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Ordered;
        impl<'de> Visitor<'de> for Ordered {
            type Value = Repositories;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object of repository settings keyed by path")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Repositories, A::Error> {
                let mut parts: Vec<(String, Part)> = Vec::new();
                while let Some(name) = map.next_key::<String>()? {
                    if parts.iter().any(|(known, _)| *known == name) {
                        return Err(de::Error::custom(format!(
                            "repository {name:?} is defined twice"
                        )));
                    }
                    parts.push((name, map.next_value()?));
                }
                Ok(Repositories(parts))
            }
        }
        deserializer.deserialize_map(Ordered)
    }
}

fn invalid(path: &Path, cause: impl fmt::Display) -> anyhow::Error {
    crate::api::fault(
        "invalid_config",
        format!(
            "{} is not a valid Graft configuration: {cause}",
            path.display()
        ),
        json!({"file": path}),
    )
}

/// Refuse a configuration above the checkout that another user could have
/// written: the file, the link to it if it is one, and its directory must
/// belong to the user running Graft or to the superuser. A file at the root
/// of the checkout is the checkout's own and is not checked.
#[cfg(unix)]
fn owned(dir: &Path, path: &Path, file: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let user = unsafe { libc::geteuid() };
    let ours = |uid: u32| uid == user || uid == 0;
    let link = fs::symlink_metadata(path)?;
    let parent = fs::metadata(dir)?;
    anyhow::ensure!(
        ours(file.uid()) && ours(link.uid()) && ours(parent.uid()),
        "it is above the checkout and belongs to another user, so it could run that user's commands as you"
    );
    Ok(())
}

/// Windows is not checked: there a hook from a file above the checkout
/// needs trust whether Git tracks the file or not (see [`OWNER_CHECKED`]).
#[cfg(not(unix))]
fn owned(_: &Path, _: &Path, _: &fs::Metadata) -> Result<()> {
    Ok(())
}

/// Read and check the file `name` in `dir`, if it exists. `above` says the
/// directory is above the checkout rather than its root.
fn load(dir: &Path, name: &'static str, above: bool) -> Result<Option<File>> {
    let path = dir.join(name);
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(invalid(&path, error)),
    };
    let meta = file.metadata().map_err(|error| invalid(&path, error))?;
    if !meta.is_file() {
        return Err(invalid(&path, "it is not a regular file"));
    }
    if above {
        owned(dir, &path, &meta).map_err(|error| invalid(&path, format!("{error:#}")))?;
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| invalid(&path, error))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid(
            &path,
            format!("it is larger than {MAX_BYTES} bytes"),
        ));
    }
    // A derived struct would also accept an array; the file holds an object.
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err(invalid(&path, "it must hold a JSON object"));
    }
    let parsed: File = serde_json::from_slice(&bytes).map_err(|error| invalid(&path, error))?;
    check(&parsed, name, dir).map_err(|error| invalid(&path, format!("{error:#}")))?;
    Ok(Some(parsed))
}

/// Refuse what Graft could not use, in every part of the file `name` in
/// `dir`.
fn check(file: &File, name: &'static str, dir: &Path) -> Result<()> {
    check_part(&file.own, name, None)?;
    for resource in &file.turns {
        crate::workspace::validate_resource(resource)
            .map_err(|_| anyhow::anyhow!("turns: {resource:?} is not a resource name: use 1 to 64 lowercase letters, numbers or hyphens, starting with a letter or number"))?;
    }
    for (key, part) in &file.repositories {
        check_key(key)?;
        check_part(part, name, Some((key, dir)))
            .map_err(|error| anyhow::anyhow!("repository {key:?}: {error:#}"))?;
    }
    Ok(())
}

/// A repository is named by a plain relative path below the directory, with
/// `/` between components.
fn check_key(key: &str) -> Result<()> {
    anyhow::ensure!(
        !key.is_empty()
            && !key.contains('\\')
            && !key.contains('\0')
            && key
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
            && !Path::new(key).is_absolute()
            && Path::new(key)
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "repository {key:?} must be a relative path below the configuration's directory, \
         such as \"app\" or \"services/api\", with '/' between its parts"
    );
    Ok(())
}

/// Refuse a pattern on more than one line, a hook name Graft could not
/// report plainly, or a program it would not start. `below` names the
/// repository a part of an enclosing file is for and the file's directory.
fn check_part(part: &Part, name: &'static str, below: Option<(&str, &Path)>) -> Result<()> {
    if let Some(patterns) = &part.include {
        crate::include::validate(patterns)?;
    }
    for remote in part.remotes.iter().flatten() {
        let bare = remote.strip_prefix('!').unwrap_or(remote);
        anyhow::ensure!(
            name == LOCAL || bare == remote,
            "remote {remote:?} starts with '!', which removes a shared remote and belongs in {LOCAL}"
        );
        anyhow::ensure!(
            !bare.is_empty()
                && bare.len() <= MAX_NAME_BYTES
                && !bare.starts_with(['-', '.'])
                && bare
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "remote {remote:?} must be the name of a remote: 1 to {MAX_NAME_BYTES} letters, digits, '-', '_' or '.', \
             not starting with '-' or '.'"
        );
    }
    for (hook, spec) in part.hooks.iter().flat_map(|named| &named.0) {
        anyhow::ensure!(
            !hook.is_empty()
                && hook.len() <= MAX_NAME_BYTES
                && hook
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "hook name {hook:?} must be 1 to {MAX_NAME_BYTES} letters, digits, '-', '_' or '.'"
        );
        match spec {
            Some(spec) => match below {
                Some((key, dir)) => validate_below(&spec.argv, key, dir),
                // A post-take hook runs in the checkout, never in a graft.
                None if spec.event == Event::PostTake => validate_inside(&spec.argv).map(|_| ()),
                None => hook::validate(&spec.argv),
            }
            .map_err(|error| anyhow::anyhow!("hook {hook:?}: {error:#}"))?,
            None => anyhow::ensure!(
                name == LOCAL,
                "hook {hook:?} is null; null turns off a shared hook and belongs in {LOCAL}"
            ),
        }
    }
    Ok(())
}

/// A hook for a repository below the configuration's directory `dir` may
/// also name its program by a path relative to that directory, as long as
/// the path stays inside it and outside the repository `key` and every
/// directory of grafts, whose files a graft's work could change. Links are
/// resolved first, and where the filesystem ignores case, so is the
/// comparison.
fn validate_below(argv: &[String], key: &str, dir: &Path) -> Result<()> {
    let Some(normal) = validate_inside(argv)? else {
        return Ok(());
    };
    let program = &argv[0];
    let inside_repository = || {
        anyhow::anyhow!(
            "the hook program {program:?} is inside the repository {key:?}, \
             whose files a graft's work can change; keep it outside it"
        )
    };
    let in_grafts = |part: &OsStr| {
        anyhow::anyhow!(
            "the hook program {program:?} is inside {:?}, a directory of grafts, \
             whose files a graft's work can change; keep it outside it",
            part.to_string_lossy()
        )
    };
    if inside(&normal, Path::new(key), FOLD_CASE) {
        return Err(inside_repository());
    }
    if let Some(part) = normal.iter().find(|part| grafts(part)) {
        return Err(in_grafts(part));
    }
    // A link on the way can lead anywhere, so check where it leads too.
    let root = resolve(dir)?;
    let resolved = resolve(&dir.join(&normal))?;
    let repository = resolve(&dir.join(key))?;
    anyhow::ensure!(
        inside(&resolved, &root, FOLD_CASE),
        "the hook program {program:?} leads outside the configuration's directory, to {}, \
         through a link; keep it inside the directory",
        resolved.display()
    );
    if inside(&resolved, &repository, FOLD_CASE) {
        return Err(inside_repository());
    }
    if let Some(part) = resolved
        .iter()
        .skip(root.iter().count())
        .find(|part| grafts(part))
    {
        return Err(in_grafts(part));
    }
    Ok(())
}

/// Check a hook whose program may be a path relative to the configuration's
/// directory, inside it. Returns that path without `.` parts, or `None` when
/// the program is an absolute path or a bare name.
fn validate_inside(argv: &[String]) -> Result<Option<PathBuf>> {
    let Some(program) = argv.first().filter(|program| relative_program(program)) else {
        hook::validate(argv)?;
        return Ok(None);
    };
    let mut rest = argv.to_vec();
    // Check everything else as for any hook, with a bare name in its place.
    rest[0] = "program".to_owned();
    hook::validate(&rest)?;
    anyhow::ensure!(
        program.len() <= 4096 && !program.contains('\0'),
        "the hook program must be at most 4096 bytes and contain no NUL character"
    );
    let path = Path::new(program);
    anyhow::ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
            && path.components().any(|c| matches!(c, Component::Normal(_))),
        "the hook program {program:?} must be a path inside the configuration's directory, \
         without '..', or an absolute path, or a bare name to look up in PATH"
    );
    Ok(Some(
        path.components()
            .filter(|component| matches!(component, Component::Normal(_)))
            .collect(),
    ))
}

/// Whether `path` is `base` or inside it, comparing component by component,
/// and ignoring case when `fold` says the filesystem does.
fn inside(path: &Path, base: &Path, fold: bool) -> bool {
    let same = |a: &OsStr, b: &OsStr| {
        a == b || fold && a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    };
    let mut parts = path.iter();
    base.iter()
        .all(|part| parts.next().is_some_and(|own| same(own, part)))
}

/// Whether the path component `part` names a directory of grafts, which
/// Graft makes beside a repository as `NAME.graft`, in any case.
fn grafts(part: &OsStr) -> bool {
    part.to_string_lossy().to_lowercase().ends_with(".graft")
}

/// `path` with every link resolved in the part of it that exists: its
/// nearest existing ancestor made canonical, and the rest appended as
/// written. A link that leads nowhere is refused, since what it names could
/// appear later.
fn resolve(path: &Path) -> Result<PathBuf> {
    let mut rest = Vec::new();
    let mut existing = path;
    loop {
        match dunce::canonicalize(existing) {
            Ok(canonical) => {
                return Ok(rest
                    .into_iter()
                    .rev()
                    .fold(canonical, |path: PathBuf, part| path.join(part)));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                anyhow::ensure!(
                    existing.symlink_metadata().is_err(),
                    "{} is a link that leads nowhere",
                    existing.display()
                );
                rest.push(
                    existing
                        .file_name()
                        .with_context(|| format!("no part of {} exists", path.display()))?,
                );
                existing = existing
                    .parent()
                    .with_context(|| format!("no part of {} exists", path.display()))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("resolve {}", path.display()));
            }
        }
    }
}

/// Whether `program` is a path with a directory in it that is not absolute.
fn relative_program(program: &str) -> bool {
    hook::validate(&[program.to_owned()]).is_err()
        && !program.is_empty()
        && !Path::new(program).is_absolute()
}

/// How the merged configuration names its files, and the directory a
/// relative program is resolved against, for a part of an enclosing file.
struct Names<'a> {
    shared: String,
    local: String,
    /// The directory that holds the files.
    dir: &'a Path,
    /// Whether these are a repository's settings in a directory above it.
    below: bool,
}

impl Names<'_> {
    fn hook(&self, name: String, spec: Spec, local: bool) -> Hook {
        // Checked already: only these may name a relative program.
        let resolves = self.below || spec.event == Event::PostTake;
        let program = (resolves && relative_program(&spec.argv[0])).then(|| {
            Path::new(&spec.argv[0])
                .components()
                .filter(|component| matches!(component, Component::Normal(_)))
                .fold(self.dir.to_owned(), |path, part| path.join(part))
        });
        Hook {
            name,
            event: spec.event,
            argv: spec.argv,
            file: if local { &self.local } else { &self.shared }.clone(),
            local,
            program,
            dir: None,
        }
    }
}

/// The effective configuration: the shared part, overridden by the local one.
fn merge(shared: Option<Part>, local: Option<Part>, names: &Names<'_>) -> Result<Config> {
    let mut config = Config::default();
    let shared = shared.unwrap_or_default();
    let local = local.unwrap_or_default();
    // Remotes are joined like include patterns; a local `!name` removes one.
    for (remotes, file, local) in [
        (&shared.remotes, &names.shared, false),
        (&local.remotes, &names.local, true),
    ] {
        for name in remotes.iter().flatten() {
            config.listed_remotes.push(Remote {
                name: name.clone(),
                file: file.clone(),
                local,
            });
        }
    }
    for remote in shared.remotes.iter().flatten() {
        if !config.remotes.contains(remote) {
            config.remotes.push(remote.clone());
        }
    }
    for remote in local.remotes.iter().flatten() {
        match remote.strip_prefix('!') {
            Some(removed) => config.remotes.retain(|known| known != removed),
            None if !config.remotes.contains(remote) => config.remotes.push(remote.clone()),
            None => {}
        }
    }
    let blank = |pattern: &String| pattern.trim().is_empty();

    let shared_patterns: Vec<String> = shared.include.unwrap_or_default();
    config.include = shared_patterns
        .iter()
        .filter(|p| !blank(p))
        .cloned()
        .collect();
    if !config.include.is_empty() {
        config.include_files.push(names.shared.clone());
    }
    let mut local_added = false;
    for pattern in local.include.unwrap_or_default() {
        if blank(&pattern) {
            continue;
        }
        // `!pattern` removes the same shared pattern. One that matches no
        // shared pattern stays, as a .gitignore negation of what precedes it.
        if let Some(removed) = pattern.strip_prefix('!')
            && config.include.iter().any(|known| known == removed)
        {
            config.include.retain(|known| known != removed);
        } else {
            config.include.push(pattern);
        }
        local_added = true;
    }
    if local_added {
        config.include_files.push(names.local.clone());
    }
    if config.include.is_empty() {
        config.include_files.clear();
    }

    for (name, spec) in shared.hooks.unwrap_or_default().0 {
        if let Some(spec) = spec {
            config.hooks.push(names.hook(name, spec, false));
        }
    }
    config.defined = config.hooks.clone();
    for (name, spec) in local.hooks.unwrap_or_default().0 {
        let hook = spec.map(|spec| names.hook(name.clone(), spec, true));
        if let Some(hook) = &hook {
            config.defined.push(hook.clone());
        }
        let known = config.hooks.iter().position(|known| known.name == name);
        match (known, hook) {
            // Replacing a shared hook keeps its place in the order.
            (Some(at), Some(hook)) => config.hooks[at] = hook,
            (Some(at), None) => {
                config.hooks.remove(at);
            }
            (None, Some(hook)) => config.hooks.push(hook),
            // Turning off a hook the shared file does not define changes nothing.
            (None, None) => {}
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn primary() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        crate::git::bytes(tmp.path(), &["init", "-q", "--template="]).unwrap();
        tmp
    }

    fn write(root: &Path, name: &str, body: &str) {
        fs::write(root.join(name), body).unwrap();
    }

    fn absolute(name: &str) -> String {
        if cfg!(windows) {
            format!(r"C:\tools\{name}.exe")
        } else {
            format!("/usr/local/bin/{name}")
        }
    }

    fn names(config: &Config) -> Vec<(&str, Event, &str)> {
        config
            .hooks
            .iter()
            .map(|hook| (hook.name.as_str(), hook.event, hook.file.as_str()))
            .collect()
    }

    #[test]
    fn missing_files_and_bare_repositories_set_nothing() {
        let tmp = primary();
        assert_eq!(Config::read(tmp.path()).unwrap(), Config::default());
        let bare = tempfile::tempdir().unwrap();
        crate::git::bytes(bare.path(), &["init", "-q", "--bare", "--template="]).unwrap();
        write(bare.path(), SHARED, "not json");
        assert_eq!(Config::read(bare.path()).unwrap(), Config::default());
        write(tmp.path(), SHARED, "{}");
        write(tmp.path(), LOCAL, r#"{"include": [], "hooks": {}}"#);
        assert_eq!(
            Config::read(tmp.path()).unwrap(),
            Config {
                dir: Some(tmp.path().to_owned()),
                ..Config::default()
            }
        );
    }

    #[test]
    fn local_include_patterns_add_to_the_shared_ones_and_remove_them_with_a_bang() {
        let tmp = primary();
        write(
            tmp.path(),
            SHARED,
            r#"{"include": [".env", "config/*.json", " ", "build/"]}"#,
        );
        let config = Config::read(tmp.path()).unwrap();
        assert_eq!(config.include, [".env", "config/*.json", "build/"]);
        assert_eq!(config.include_files, [SHARED]);

        write(
            tmp.path(),
            LOCAL,
            r#"{"include": ["!build/", ".env.local", "!config/secret.json", ""]}"#,
        );
        let config = Config::read(tmp.path()).unwrap();
        assert_eq!(
            config.include,
            [".env", "config/*.json", ".env.local", "!config/secret.json"]
        );
        assert_eq!(config.include_files, [SHARED, LOCAL]);
        assert_eq!(
            config.include_defaults(),
            json!({"patterns": config.include, "files": [SHARED, LOCAL]})
        );

        // Removing every shared pattern leaves no default at all.
        write(
            tmp.path(),
            LOCAL,
            r#"{"include": ["!.env", "!config/*.json", "!build/"]}"#,
        );
        let config = Config::read(tmp.path()).unwrap();
        assert!(config.include.is_empty());
        assert!(config.include_files.is_empty());

        // The local file alone can set the default.
        fs::remove_file(tmp.path().join(SHARED)).unwrap();
        write(tmp.path(), LOCAL, r#"{"include": [".env"]}"#);
        let config = Config::read(tmp.path()).unwrap();
        assert_eq!(config.include, [".env"]);
        assert_eq!(config.include_files, [LOCAL]);
    }

    #[test]
    fn local_hooks_replace_turn_off_or_add_to_shared_hooks_by_name() {
        let tmp = primary();
        // Written as text: the order of the names is the order hooks run.
        let create = json!(absolute("create-db"));
        let drop = json!(absolute("drop-db"));
        let shared = format!(
            r#"{{"hooks": {{
                "database": {{"event": "post-add", "argv": [{create}]}},
                "drop-database": {{"event": "pre-archive", "argv": [{drop}]}},
                "assets": {{"event": "post-add", "argv": ["npm", "run", "assets"]}},
                "reopen": {{"event": "post-restore", "argv": [{create}]}}
            }}}}"#
        );
        write(tmp.path(), SHARED, &shared);
        let config = Config::read(tmp.path()).unwrap();
        assert_eq!(
            names(&config),
            [
                ("database", Event::PostAdd, SHARED),
                ("drop-database", Event::PreArchive, SHARED),
                ("assets", Event::PostAdd, SHARED),
                ("reopen", Event::PostRestore, SHARED),
            ]
        );

        let local = format!(
            r#"{{"hooks": {{
                "notify": {{"event": "post-add", "argv": ["notify-send", "graft added"]}},
                "assets": null,
                "database": {{"event": "post-add", "argv": [{create}, "--small"]}},
                "not-shared": null
            }}}}"#
        );
        write(tmp.path(), LOCAL, &local);
        let config = Config::read(tmp.path()).unwrap();
        // A replaced hook keeps its place; a new one runs after the shared ones.
        assert_eq!(
            names(&config),
            [
                ("database", Event::PostAdd, LOCAL),
                ("drop-database", Event::PreArchive, SHARED),
                ("reopen", Event::PostRestore, SHARED),
                ("notify", Event::PostAdd, LOCAL),
            ]
        );
        assert_eq!(
            config.hooks[0].argv,
            [absolute("create-db"), "--small".into()]
        );
        let post_add: Vec<_> = config
            .hooks_for(Event::PostAdd)
            .into_iter()
            .map(|hook| hook.name)
            .collect();
        assert_eq!(post_add, ["database", "notify"]);
        assert!(config.hooks_for(Event::PreArchive).len() == 1);
        assert_eq!(
            config.hooks[1].describe(),
            json!({"name": "drop-database", "event": "pre-archive", "argv": [absolute("drop-db")], "file": SHARED})
        );
    }

    #[test]
    fn a_local_hook_may_move_a_shared_one_to_another_event() {
        let tmp = primary();
        write(
            tmp.path(),
            SHARED,
            r#"{"hooks": {"setup": {"event": "post-add", "argv": ["setup"]}}}"#,
        );
        write(
            tmp.path(),
            LOCAL,
            r#"{"hooks": {"setup": {"event": "post-restore", "argv": ["setup"]}}}"#,
        );
        let config = Config::read(tmp.path()).unwrap();
        assert!(config.hooks_for(Event::PostAdd).is_empty());
        assert_eq!(config.hooks_for(Event::PostRestore)[0].file, LOCAL);
    }

    #[test]
    fn malformed_files_are_refused_with_the_file_named() {
        let tmp = primary();
        for (file, body, cause) in [
            (SHARED, "{", "EOF"),
            (SHARED, "[]", "JSON object"),
            (SHARED, "", "JSON object"),
            (
                LOCAL,
                r#"{"hooks": {"a": ["post-add", ["x"]]}}"#,
                "object or null",
            ),
            (SHARED, r#"{"includes": [".env"]}"#, "unknown field"),
            (SHARED, r#"{"include": ".env"}"#, "invalid type"),
            (SHARED, r#"{"include": ["a\nb"]}"#, "single line"),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-merge", "argv": ["x"]}}}"#,
                "unknown variant",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-add", "argv": ["x"], "shell": true}}}"#,
                "unknown field",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-add"}}}"#,
                "missing field",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-add", "argv": []}}}"#,
                "program",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-add", "argv": ["./setup"]}}}"#,
                "relative path",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-add", "argv": "setup"}}}"#,
                "invalid type",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a b": {"event": "post-add", "argv": ["x"]}}}"#,
                "hook name",
            ),
            (
                LOCAL,
                r#"{"hooks": {"": {"event": "post-add", "argv": ["x"]}}}"#,
                "hook name",
            ),
            (
                LOCAL,
                r#"{"hooks": {"a": {"event": "post-add", "argv": ["x"]}, "a": null}}"#,
                "defined twice",
            ),
            (
                SHARED,
                r#"{"hooks": {"a": null}}"#,
                "belongs in .graftconfig.local",
            ),
            (SHARED, r#"{"hooks": []}"#, "object of hooks"),
        ] {
            let _ = fs::remove_file(tmp.path().join(SHARED));
            let _ = fs::remove_file(tmp.path().join(LOCAL));
            write(tmp.path(), file, body);
            let error = Config::read(tmp.path()).expect_err(body);
            let json = crate::api::error_json(&error);
            assert_eq!(json["code"], "invalid_config", "{body}");
            let message = json["message"].as_str().unwrap();
            assert!(message.contains(file), "{message}");
            assert!(message.contains(cause), "{body}: {message}");
            assert_eq!(json["details"]["file"], json!(tmp.path().join(file)));
        }
        let _ = fs::remove_file(tmp.path().join(LOCAL));
        fs::remove_file(tmp.path().join(SHARED)).unwrap();
        fs::create_dir(tmp.path().join(SHARED)).unwrap();
        let error = Config::read(tmp.path()).unwrap_err();
        assert_eq!(crate::api::error_json(&error)["code"], "invalid_config");
    }

    /// A directory holding the repositories `app` and `services/api`, which
    /// is not a repository itself.
    fn enclosing() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let app = root.join("app");
        let api = root.join("services").join("api");
        for repo in [&app, &api] {
            fs::create_dir_all(repo).unwrap();
            crate::git::bytes(repo, &["init", "-q", "--template="]).unwrap();
        }
        (tmp, root, app, api)
    }

    #[test]
    fn a_directory_above_configures_each_repository_below_it_by_path() {
        let (_tmp, root, app, api) = enclosing();
        let create = json!(absolute("create-db"));
        write(
            &root,
            SHARED,
            &format!(
                r#"{{
                "include": ["own.txt"],
                "hooks": {{"own": {{"event": "post-add", "argv": ["own"]}}}},
                "repositories": {{
                    "app": {{
                        "include": [".env", "storage/*.key"],
                        "hooks": {{
                            "prepare": {{"event": "post-add", "argv": ["./scripts/graft-hook.sh", "prepare"]}},
                            "database": {{"event": "post-add", "argv": [{create}]}},
                            "cleanup": {{"event": "pre-archive", "argv": ["scripts/graft-hook.sh"]}}
                        }}
                    }},
                    "services/api": {{"include": ["config.local.json"]}}
                }}
            }}"#
            ),
        );
        write(
            &root,
            LOCAL,
            r#"{"repositories": {"app": {
                "include": ["!storage/*.key", "notes.txt"],
                "hooks": {"database": null, "notify": {"event": "post-add", "argv": ["notify-send", "ready"]}}
            }}}"#,
        );
        let config = Config::read(&app).unwrap();
        assert_eq!(config.dir.as_deref(), Some(root.as_path()));
        assert_eq!(config.include, [".env", "notes.txt"]);
        assert_eq!(
            config.include_files,
            ["../.graftconfig", "../.graftconfig.local"]
        );
        assert_eq!(
            names(&config),
            [
                ("prepare", Event::PostAdd, "../.graftconfig"),
                ("cleanup", Event::PreArchive, "../.graftconfig"),
                ("notify", Event::PostAdd, "../.graftconfig.local"),
            ]
        );
        // A relative program is resolved against the directory; the others
        // are started as written.
        let program = root.join("scripts").join("graft-hook.sh");
        assert_eq!(config.hooks[0].program.as_deref(), Some(program.as_path()));
        assert_eq!(config.hooks[1].program.as_deref(), Some(program.as_path()));
        assert_eq!(config.hooks[2].program, None);
        assert!(
            config
                .hooks
                .iter()
                .all(|hook| hook.dir.as_deref() == Some(root.as_path()))
        );
        assert_eq!(
            config.hooks[0].describe(),
            json!({"name": "prepare", "event": "post-add", "argv": ["./scripts/graft-hook.sh", "prepare"],
                   "file": "../.graftconfig", "program": program})
        );
        assert_eq!(config.defined.len(), 4);

        let config = Config::read(&api).unwrap();
        assert_eq!(config.include, ["config.local.json"]);
        assert_eq!(config.include_files, ["../../.graftconfig"]);
        assert!(config.hooks.is_empty());

        // A repository the files do not name gets nothing from them, and
        // the directory's own settings are not inherited.
        let other = root.join("other");
        fs::create_dir(&other).unwrap();
        crate::git::bytes(&other, &["init", "-q", "--template="]).unwrap();
        let config = Config::read(&other).unwrap();
        assert!(config.include.is_empty() && config.hooks.is_empty());
        assert_eq!(config.dir.as_deref(), Some(root.as_path()));
    }

    #[test]
    fn the_checkout_s_own_files_and_then_the_nearest_directory_above_win() {
        let (_tmp, root, _app, api) = enclosing();
        write(
            &root,
            SHARED,
            r#"{"repositories": {"services/api": {"include": ["from-root"]}}}"#,
        );
        write(
            &root.join("services"),
            LOCAL,
            r#"{"repositories": {"api": {"include": ["from-services"]}}}"#,
        );
        let config = Config::read(&api).unwrap();
        assert_eq!(config.include, ["from-services"]);
        assert_eq!(config.include_files, ["../.graftconfig.local"]);
        assert_eq!(config.dir.as_deref(), Some(root.join("services").as_path()));
        write(&api, SHARED, r#"{"include": ["own"]}"#);
        let config = Config::read(&api).unwrap();
        assert_eq!(config.include, ["own"]);
        assert_eq!(config.include_files, [SHARED]);
        assert_eq!(config.dir.as_deref(), Some(api.as_path()));
    }

    #[test]
    fn the_search_above_stops_at_the_home_directory_that_holds_the_checkout() {
        let (_tmp, root, _app, api) = enclosing();
        let services = root.join("services");
        // A broken file above home is not read, so it breaks nothing.
        write(&root, SHARED, "not json");
        let config = Config::read_within(&api, Some(&services)).unwrap();
        assert_eq!(config, Config::default());
        // A file in home itself is read.
        write(
            &services,
            SHARED,
            r#"{"repositories": {"api": {"include": ["from-home"]}}}"#,
        );
        let config = Config::read_within(&api, Some(&services)).unwrap();
        assert_eq!(config.include, ["from-home"]);
        assert_eq!(config.dir.as_deref(), Some(services.as_path()));
        fs::remove_file(services.join(SHARED)).unwrap();
        // Where the filesystem ignores case, so does the comparison with home.
        let other_case = root.join("SERVICES");
        let config = Config::read_within(&api, Some(&other_case));
        assert_eq!(config.is_ok(), FOLD_CASE, "{config:?}");
        // A home that does not hold the checkout sets no bound, nor does none.
        for home in [Some(root.join("app")), None] {
            let error = Config::read_within(&api, home.as_deref()).unwrap_err();
            assert_eq!(crate::api::error_json(&error)["code"], "invalid_config");
        }
    }

    #[test]
    fn a_malformed_file_above_is_refused_with_the_file_named() {
        let (_tmp, root, app, _api) = enclosing();
        for (body, cause) in [
            (
                r#"{"repositories": []}"#,
                "repository settings keyed by path",
            ),
            (
                r#"{"repositories": {"app": []}}"#,
                "an object of a repository's settings",
            ),
            (
                r#"{"repositories": {"app": {"includes": []}}}"#,
                "unknown field",
            ),
            (
                r#"{"repositories": {"app": {"repositories": {}}}}"#,
                "unknown field",
            ),
            (
                r#"{"repositories": {"app": {}, "app": {}}}"#,
                "defined twice",
            ),
            (r#"{"repositories": {"../app": {}}}"#, "relative path below"),
            (r#"{"repositories": {"/app": {}}}"#, "relative path below"),
            (r#"{"repositories": {"a//b": {}}}"#, "relative path below"),
            (r#"{"repositories": {"./app": {}}}"#, "relative path below"),
            (r#"{"repositories": {"a\\b": {}}}"#, "relative path below"),
            (r#"{"repositories": {"": {}}}"#, "relative path below"),
            (
                r#"{"repositories": {"app": {"hooks": {"a": {"event": "post-add", "argv": ["../setup"]}}}}}"#,
                "without '..'",
            ),
            (
                r#"{"repositories": {"app": {"hooks": {"a": {"event": "post-add", "argv": ["./app/bin/setup"]}}}}}"#,
                "inside the repository",
            ),
            (
                r#"{"repositories": {"app": {"hooks": {"a": {"event": "post-add", "argv": ["app.graft/x/setup"]}}}}}"#,
                "a directory of grafts",
            ),
            (
                r#"{"repositories": {"app": {"hooks": {"a": {"event": "post-add", "argv": ["./setup", "nul\u0000"]}}}}}"#,
                "NUL",
            ),
            // The directory's own hooks run in its own grafts, so a relative
            // program there would still name the graft's copy.
            (
                r#"{"hooks": {"a": {"event": "post-add", "argv": ["./setup"]}}}"#,
                "relative path",
            ),
            // A null is refused in the shared file, inside a section too.
            (
                r#"{"repositories": {"other": {"hooks": {"a": null}}}}"#,
                "belongs in",
            ),
        ] {
            write(&root, SHARED, body);
            let error = Config::read(&app).expect_err(body);
            let json = crate::api::error_json(&error);
            assert_eq!(json["code"], "invalid_config", "{body}");
            let message = json["message"].as_str().unwrap();
            assert!(message.contains(cause), "{body}: {message}");
            assert_eq!(json["details"]["file"], json!(root.join(SHARED)));
        }
        // A program inside the directory, with `./` or without, is accepted.
        write(
            &root,
            SHARED,
            r#"{"repositories": {"app": {"hooks": {"a": {"event": "post-add", "argv": ["tools/./setup"]}}}}}"#,
        );
        let config = Config::read(&app).unwrap();
        assert_eq!(
            config.hooks[0].program.as_deref(),
            Some(root.join("tools").join("setup").as_path())
        );
    }

    /// The error reading `app`'s configuration gives when the directory's
    /// `.graftconfig` gives its hook `program`, or `None` if it is accepted.
    fn refusal(root: &Path, app: &Path, program: &str) -> Option<String> {
        let argv = json!([program]);
        write(
            root,
            SHARED,
            &format!(
                r#"{{"repositories": {{"app": {{"hooks": {{"a": {{"event": "post-add", "argv": {argv}}}}}}}}}}}"#
            ),
        );
        Config::read(app).err().map(|error| {
            let json = crate::api::error_json(&error);
            assert_eq!(json["code"], "invalid_config", "{program}");
            json["message"].as_str().unwrap().to_owned()
        })
    }

    #[test]
    fn a_program_in_the_repository_in_another_case_or_in_any_grafts_is_refused() {
        let (_tmp, root, app, _api) = enclosing();
        fs::create_dir_all(root.join("app.graft").join("x")).unwrap();
        for (program, cause) in [
            ("APP.graft/x/evil", "a directory of grafts"),
            ("other.graft/g/evil", "a directory of grafts"),
            ("./tools/Nested.Graft/evil", "a directory of grafts"),
            ("app/bin/setup", "inside the repository"),
        ] {
            let message = refusal(&root, &app, program).expect(program);
            assert!(message.contains(cause), "{program}: {message}");
        }
        // Where the filesystem ignores case, `App` is the repository.
        let other_case = refusal(&root, &app, "App/bin/setup");
        assert_eq!(other_case.is_some(), FOLD_CASE, "{other_case:?}");
        if let Some(message) = other_case {
            assert!(message.contains("inside the repository"), "{message}");
        }
        // A name that only contains `.graft`, or another repository's
        // directory, is not refused.
        assert_eq!(refusal(&root, &app, "tools/x.graft.d/setup"), None);
        assert_eq!(refusal(&root, &app, "services/setup"), None);
    }

    #[test]
    fn paths_are_compared_by_component_and_in_any_case_when_the_filesystem_ignores_it() {
        let path = Path::new("App").join("bin").join("setup");
        assert!(inside(&path, Path::new("App"), false));
        assert!(!inside(&path, Path::new("app"), false));
        assert!(inside(&path, Path::new("app"), true));
        assert!(inside(&path, &Path::new("APP").join("BIN"), true));
        assert!(!inside(Path::new("application"), Path::new("app"), true));
        assert!(!inside(
            Path::new("app"),
            &Path::new("app").join("bin"),
            true
        ));
        assert!(grafts(OsStr::new("x.GRAFT")) && grafts(OsStr::new(".graft")));
        assert!(!grafts(OsStr::new("x.graft.d")) && !grafts(OsStr::new("graft")));
    }

    #[cfg(unix)]
    #[test]
    fn a_program_is_checked_where_its_links_lead() {
        use std::os::unix::fs::symlink;
        let (_tmp, root, app, _api) = enclosing();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.join("app.graft").join("x")).unwrap();
        fs::create_dir(root.join("tools")).unwrap();
        symlink(&app, root.join("linked")).unwrap();
        symlink("app.graft", root.join("grafted")).unwrap();
        symlink(outside.path(), root.join("out")).unwrap();
        symlink("nowhere", root.join("dangling")).unwrap();
        symlink("tools", root.join("kept")).unwrap();
        for (program, cause) in [
            ("./linked/setup", "inside the repository"),
            ("./linked", "inside the repository"),
            ("grafted/x/setup", "a directory of grafts"),
            ("out/setup", "leads outside the configuration's directory"),
            ("dangling/setup", "leads nowhere"),
        ] {
            let message = refusal(&root, &app, program).expect(program);
            assert!(message.contains(cause), "{program}: {message}");
        }
        // A link that stays inside the directory and outside the repository
        // is followed, and the hook keeps the path as written.
        assert_eq!(refusal(&root, &app, "kept/setup"), None);
        let config = Config::read(&app).unwrap();
        assert_eq!(
            config.hooks[0].program.as_deref(),
            Some(root.join("kept").join("setup").as_path())
        );
    }

    #[test]
    fn keys_that_name_no_repository_are_reported() {
        let (_tmp, root, app, _api) = enclosing();
        write(
            &root,
            SHARED,
            r#"{"repositories": {"app": {}, "ap": {"include": [".env"]}, "services": {}, "services/api": {}}}"#,
        );
        write(&root, LOCAL, r#"{"repositories": {"services/apl": {}}}"#);
        let config = Config::read(&app).unwrap();
        assert_eq!(
            config.missing_repositories,
            [
                json!({"file": "../.graftconfig", "repository": "ap"}),
                json!({"file": "../.graftconfig", "repository": "services"}),
                json!({"file": "../.graftconfig.local", "repository": "services/apl"}),
            ]
        );
        // So are those in the checkout's own files, for the repositories
        // below it.
        write(&app, SHARED, r#"{"repositories": {"vendor/lib": {}}}"#);
        let config = Config::read(&app).unwrap();
        assert_eq!(
            config.missing_repositories,
            [json!({"file": SHARED, "repository": "vendor/lib"})]
        );
        assert!(!config.above);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_above_the_checkout_is_read_through_a_link() {
        let (_tmp, root, app, _api) = enclosing();
        let kept = root.join("kept.json");
        write(
            &root,
            "kept.json",
            r#"{"repositories": {"app": {"include": [".env"]}}}"#,
        );
        std::os::unix::fs::symlink(&kept, root.join(LOCAL)).unwrap();
        assert_eq!(Config::read(&app).unwrap().include, [".env"]);
    }

    #[test]
    fn turns_are_listed_at_the_top_of_either_file_and_checked() {
        let (_tmp, root, app, _api) = enclosing();
        write(&root, SHARED, r#"{"turns": ["stack", "database"]}"#);
        write(&root, LOCAL, r#"{"turns": ["database", "simulator"]}"#);
        assert_eq!(
            shared_turns(&app).unwrap(),
            Some((
                root.clone(),
                vec!["stack".into(), "database".into(), "simulator".into()]
            ))
        );
        // Only the top level lists them, not a repository's settings.
        write(
            &root,
            LOCAL,
            r#"{"repositories": {"app": {"turns": ["web"]}}}"#,
        );
        let error = Config::read(&app).unwrap_err();
        assert!(format!("{error:#}").contains("unknown field"), "{error:#}");
        for bad in [
            r#"{"turns": ["Stack"]}"#,
            r#"{"turns": "stack"}"#,
            r#"{"turns": [""]}"#,
        ] {
            write(&root, LOCAL, bad);
            for error in [
                Config::read(&app).unwrap_err(),
                shared_turns(&app).unwrap_err(),
            ] {
                assert_eq!(
                    crate::api::error_json(&error)["code"],
                    "invalid_config",
                    "{bad}"
                );
            }
        }
    }

    #[test]
    fn a_checkout_s_own_files_do_not_hide_the_turns_of_the_directory_above() {
        let (_tmp, root, app, api) = enclosing();
        write(
            &root,
            SHARED,
            r#"{"turns": ["stack"], "repositories": {"app": {"include": ["from-root"]}}}"#,
        );
        write(&app, SHARED, r#"{"include": ["own"]}"#);
        let stack = Some((root.clone(), vec!["stack".to_owned()]));
        // The checkout's own file governs everything else.
        let config = Config::read(&app).unwrap();
        assert_eq!(config.include, ["own"]);
        assert_eq!(config.dir.as_deref(), Some(app.as_path()));
        assert_eq!(shared_turns(&app).unwrap(), stack);
        assert_eq!(shared_turns(&api).unwrap(), stack);

        // Its own `turns` may repeat those above, in any order.
        write(&root, SHARED, r#"{"turns": ["stack", "database"]}"#);
        write(&app, LOCAL, r#"{"turns": ["database", "stack", "stack"]}"#);
        assert_eq!(
            shared_turns(&app).unwrap(),
            Some((root.clone(), vec!["stack".into(), "database".into()]))
        );
        // A list that differs is refused rather than guessed at, naming both files.
        write(&app, LOCAL, r#"{"turns": ["stack"]}"#);
        let error = shared_turns(&app).unwrap_err();
        let json = crate::api::error_json(&error);
        assert_eq!(json["code"], "invalid_config");
        assert_eq!(json["details"]["file"], json!(app.join(LOCAL)));
        assert_eq!(json["details"]["above"], json!(root.join(SHARED)));
        let message = json["message"].as_str().unwrap();
        assert!(message.contains("same resources"), "{message}");
        // Only turns: every other command still reads the checkout's own files.
        assert_eq!(Config::read(&app).unwrap().include, ["own"]);

        // Nothing listed above leaves the checkout's own turns its own.
        write(&root, SHARED, r#"{"repositories": {}}"#);
        assert_eq!(shared_turns(&app).unwrap(), None);
    }

    #[test]
    fn a_directory_between_that_lists_no_turns_does_not_split_the_queue() {
        let (_tmp, root, app, api) = enclosing();
        write(&root, SHARED, r#"{"turns": ["stack"]}"#);
        let stack = Some((root.clone(), vec!["stack".to_owned()]));
        // A checkout configured for something else, with a repository inside it.
        write(&app, SHARED, r#"{"include": ["x"]}"#);
        let web = app.join("web");
        fs::create_dir_all(&web).unwrap();
        crate::git::bytes(&web, &["init", "-q", "--template="]).unwrap();
        assert_eq!(shared_turns(&app).unwrap(), stack);
        assert_eq!(shared_turns(&web).unwrap(), stack);
        // A plain directory configured for something else.
        write(&root.join("services"), SHARED, r#"{"repositories": {}}"#);
        assert_eq!(shared_turns(&api).unwrap(), stack);
    }

    #[test]
    fn turns_listed_above_home_are_not_shared() {
        let (_tmp, root, app, api) = enclosing();
        write(&root, SHARED, r#"{"turns": ["stack"]}"#);
        let stack = Some((root.clone(), vec!["stack".to_owned()]));
        // With the root as home, the file there is read.
        assert_eq!(shared_turns_within(&app, Some(&root)).unwrap(), stack);
        // With home below it, the search stops before reaching it.
        let services = root.join("services");
        assert_eq!(shared_turns_within(&api, Some(&services)).unwrap(), None);
        // A file at home is still read.
        write(&services, SHARED, r#"{"turns": ["web"]}"#);
        assert_eq!(
            shared_turns_within(&api, Some(&services)).unwrap(),
            Some((services.clone(), vec!["web".to_owned()]))
        );
    }

    #[test]
    fn turns_listed_at_two_levels_are_kept_at_the_higher() {
        let (_tmp, root, app, _api) = enclosing();
        crate::git::bytes(&root, &["init", "-q", "--template="]).unwrap();
        // `app` is a checkout that holds the repository `web`, and lists
        // `stack` for it as the directory above `app` does.
        let web = app.join("web");
        fs::create_dir(&web).unwrap();
        crate::git::bytes(&web, &["init", "-q", "--template="]).unwrap();
        write(&root, SHARED, r#"{"turns": ["stack"]}"#);
        write(&app, SHARED, r#"{"turns": ["stack"]}"#);
        let stack = Some((root.clone(), vec!["stack".to_owned()]));
        assert_eq!(shared_turns(&app).unwrap(), stack);
        assert_eq!(shared_turns(&web).unwrap(), stack);
        // A repository the higher list disagrees with is refused below it too.
        write(&root, SHARED, r#"{"turns": ["database"]}"#);
        for repo in [&app, &web] {
            let error = shared_turns(repo).unwrap_err();
            assert_eq!(crate::api::error_json(&error)["code"], "invalid_config");
        }
        // With nothing listed higher up, `app` keeps them for `web`.
        write(&root, SHARED, "{}");
        assert_eq!(
            shared_turns(&web).unwrap(),
            Some((app.clone(), vec!["stack".to_owned()]))
        );
        assert_eq!(shared_turns(&app).unwrap(), None);
    }

    #[test]
    fn a_post_take_hook_may_name_a_program_inside_its_own_checkout() {
        let tmp = primary();
        write(
            tmp.path(),
            SHARED,
            r#"{"hooks": {"stack": {"event": "post-take", "argv": ["./scripts/stack", "up"]}}}"#,
        );
        let config = Config::read(tmp.path()).unwrap();
        assert_eq!(
            config.hooks[0].program.as_deref(),
            Some(tmp.path().join("scripts").join("stack").as_path())
        );
        // Other events still refuse it, and no post-take hook may leave the checkout.
        for body in [
            r#"{"hooks": {"stack": {"event": "post-add", "argv": ["./scripts/stack"]}}}"#,
            r#"{"hooks": {"stack": {"event": "post-take", "argv": ["../stack"]}}}"#,
        ] {
            write(tmp.path(), SHARED, body);
            let error = Config::read(tmp.path()).unwrap_err();
            assert_eq!(
                crate::api::error_json(&error)["code"],
                "invalid_config",
                "{body}"
            );
        }
    }

    #[test]
    fn remotes_are_joined_and_a_local_bang_removes_one() {
        let (_tmp, root, app, _api) = enclosing();
        write(
            &root,
            SHARED,
            r#"{"repositories": {"app": {"remotes": ["origin", "upstream"]}}}"#,
        );
        write(
            &root,
            LOCAL,
            r#"{"repositories": {"app": {"remotes": ["!upstream", "fork", "origin"]}}}"#,
        );
        assert_eq!(Config::read(&app).unwrap().remotes, ["origin", "fork"]);
        for bad in [
            r#"{"remotes": [""]}"#,
            r#"{"remotes": ["-x"]}"#,
            r#"{"remotes": ["a b"]}"#,
            r#"{"remotes": "origin"}"#,
        ] {
            write(&root, LOCAL, bad);
            let error = Config::read(&app).unwrap_err();
            assert_eq!(
                crate::api::error_json(&error)["code"],
                "invalid_config",
                "{bad}"
            );
        }
        // `!name` removes a shared remote, so only the local file may say it.
        write(&root, LOCAL, "{}");
        for (file, bad) in [
            (
                SHARED,
                r#"{"repositories": {"app": {"remotes": ["!origin"]}}}"#,
            ),
            (
                SHARED,
                r#"{"repositories": {"app": {"remotes": ["origin", "!origin"]}}}"#,
            ),
        ] {
            write(&root, file, bad);
            let error = Config::read(&app).unwrap_err();
            assert_eq!(
                crate::api::error_json(&error)["code"],
                "invalid_config",
                "{bad}"
            );
        }
    }
}

//! A project: a folder of circuits that aispice may read and write.
//!
//! Every path a tool receives is resolved here and must land inside the
//! project root after symlinks are followed; anything else is refused before
//! a file is touched. Saves are atomic (write to a temporary file, then
//! rename) and every save records a snapshot, so any edit can be undone, and
//! a change made outside aispice (in LTspice, say) is captured as its own
//! snapshot before the next edit so undo never loses it.

use aispice_core::schematic::{self, Schematic};
use aispice_core::symbol::{SymbolLibrary, SymbolSource};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// The folder aispice keeps its own state in, inside the project.
pub const STATE_DIR: &str = ".aispice";

/// What makes two paths the same folder on disk, whatever their spelling:
/// device and inode on Unix, read with a stat (never an open, so a FIFO in
/// the project cannot block it); volume serial and file index on Windows,
/// from a handle opened without access rights.
#[cfg(unix)]
type FileId = (u64, u64);
#[cfg(windows)]
type FileId = same_file::Handle;

/// The identity of a folder; `None` for anything else, or when it cannot
/// be read. Only folders are compared: the state folder is one.
#[cfg(unix)]
fn file_id(p: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).ok().filter(|m| m.is_dir())?;
    Some((m.dev(), m.ino()))
}
#[cfg(windows)]
fn file_id(p: &Path) -> Option<FileId> {
    std::fs::metadata(p).ok().filter(|m| m.is_dir())?;
    same_file::Handle::from_path(p).ok()
}

/// An output path after its checks: its deepest existing folder (real),
/// the names below it still to make (the file's last), and the identity
/// of the state folder they were checked against.
struct OutputWalk {
    folder: PathBuf,
    rest: Vec<String>,
    state: FileId,
}

/// File types listed as circuits.
const CIRCUIT_EXTS: &[&str] = &["asc", "cir", "net", "sp", "spi", "spice", "ckt"];
/// Folders never searched for circuits.
const SKIP_DIRS: &[&str] = &[
    STATE_DIR,
    ".git",
    "node_modules",
    "target",
    "dist",
    ".venv",
    "__pycache__",
];

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("`{0}` is outside the project folder")]
    Outside(String),
    #[error("`{0}` does not exist in the project")]
    Missing(String),
    #[error("`{0}` is not a schematic (.asc)")]
    NotSchematic(String),
    #[error("could not read or write {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("nothing to {0}")]
    NoHistory(&'static str),
    #[error("{0}")]
    Other(String),
}

fn io(path: &Path, source: std::io::Error) -> ProjectError {
    ProjectError::Io {
        path: path.display().to_string(),
        source,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CircuitEntry {
    /// Relative to the project root, `/` separated.
    pub path: String,
    pub name: String,
    /// Unix milliseconds.
    pub modified: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub id: String,
    pub time: u64,
    pub summary: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HistoryIndex {
    snapshots: Vec<Snapshot>,
    /// Index of the snapshot that matches the file on disk.
    current: usize,
}

/// Settings that shape a project's symbol library and simulators.
#[derive(Debug, Clone, Default)]
pub struct ProjectOptions {
    pub extra_symbol_dirs: Vec<PathBuf>,
    pub ltspice_lib: Option<PathBuf>,
}

pub struct Project {
    /// Canonical: symlinks resolved, so every comparison with it is between
    /// real paths.
    root: PathBuf,
    lib: Arc<SymbolLibrary>,
    options: ProjectOptions,
    /// Compare names case-insensitively, as the file systems of Windows
    /// and macOS do. Tests set it on Linux to check what those systems see.
    fold_case: bool,
    /// Serialises saves and history updates.
    write_lock: Mutex<()>,
}

impl std::fmt::Debug for Project {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Project").field("root", &self.root).finish()
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Project {
    pub fn open(root: &Path, options: ProjectOptions) -> Result<Self, ProjectError> {
        let root = std::fs::canonicalize(root).map_err(|e| io(root, e))?;
        if !root.is_dir() {
            return Err(ProjectError::Other(format!(
                "{} is not a folder",
                root.display()
            )));
        }
        let lib = Arc::new(SymbolLibrary::for_project(
            Some(&root),
            &options.extra_symbol_dirs,
            options.ltspice_lib.as_deref(),
        ));
        Ok(Self {
            root,
            lib,
            options,
            fold_case: cfg!(any(windows, target_os = "macos")),
            write_lock: Mutex::new(()),
        })
    }

    /// A project whose names compare case-insensitively whatever the
    /// platform, for testing what Windows and macOS see on Linux.
    #[doc(hidden)]
    pub fn open_folding_case(root: &Path, options: ProjectOptions) -> Result<Self, ProjectError> {
        let mut p = Self::open(root, options)?;
        p.fold_case = true;
        Ok(p)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".into())
    }

    pub fn library(&self) -> &Arc<SymbolLibrary> {
        &self.lib
    }

    pub fn options(&self) -> &ProjectOptions {
        &self.options
    }

    /// Folders a simulated deck may read files from.
    pub fn allowed_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.root.clone()];
        dirs.extend(self.options.extra_symbol_dirs.iter().cloned());
        if let Some(l) = &self.options.ltspice_lib {
            dirs.push(l.clone());
            if let Some(parent) = l.parent() {
                dirs.push(parent.join("sub"));
                dirs.push(parent.join("cmp"));
            }
        }
        dirs.extend(
            self.lib
                .dirs()
                .into_iter()
                .filter(|(_, s)| *s != SymbolSource::Project)
                .map(|(d, _)| d),
        );
        dirs
    }

    /// Resolve a project-relative path, refusing anything that escapes the
    /// root, including through symlinks. The file need not exist yet, but its
    /// parent folder must.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, ProjectError> {
        let rel = rel.trim();
        let p = Path::new(rel);
        if rel.is_empty() || rel.contains('\0') {
            return Err(ProjectError::Outside(rel.to_string()));
        }
        let candidate = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        if p.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(ProjectError::Outside(rel.to_string()));
        }
        let real = match std::fs::canonicalize(&candidate) {
            Ok(r) => r,
            Err(_) => {
                let parent = candidate
                    .parent()
                    .ok_or_else(|| ProjectError::Outside(rel.to_string()))?;
                let real_parent = std::fs::canonicalize(parent)
                    .map_err(|_| ProjectError::Missing(rel.to_string()))?;
                real_parent.join(
                    candidate
                        .file_name()
                        .ok_or_else(|| ProjectError::Outside(rel.to_string()))?,
                )
            }
        };
        self.check_real(&real, rel)?;
        Ok(real)
    }

    fn same_name(&self, a: &str, b: &str) -> bool {
        if self.fold_case {
            a.to_lowercase() == b.to_lowercase()
        } else {
            a == b
        }
    }

    /// Whether a path below the project folder is named as `.aispice/`
    /// would be where names fold: an early refusal with a plain message.
    /// Names cannot settle it (every file system folds its own way), so the
    /// state folder's identity does ([`Project::in_state_dir`]).
    fn named_state(&self, below: &Path) -> bool {
        below.components().next().is_some_and(|c| {
            let name = c.as_os_str().to_string_lossy();
            let name = name.split(':').next().unwrap_or_default();
            self.same_name(name.trim_end_matches(['.', ' ']), STATE_DIR)
        })
    }

    /// The state folder's identity, when it is a plain folder. With `make`
    /// it is created first if missing (never through a link), so a folder a
    /// tool makes cannot become it afterwards; one that is a link or a file
    /// stops every write.
    fn state_id(&self, make: bool) -> Result<Option<FileId>, ProjectError> {
        let dir = self.root.join(STATE_DIR);
        if make {
            self.state_subdir(&[])?;
        }
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => match file_id(&dir) {
                Some(id) => Ok(Some(id)),
                None => Err(ProjectError::Other(format!(
                    "{} cannot be opened",
                    dir.display()
                ))),
            },
            _ => Ok(None),
        }
    }

    /// Whether a real path (symlinks resolved) is aispice's own `.aispice/`
    /// folder or inside it: the path, or a folder above it up to the
    /// project folder, is the same folder on disk as `.aispice/`, however
    /// it was spelled or reached.
    pub fn in_state_dir(&self, real: &Path) -> bool {
        let Ok(below) = real.strip_prefix(&self.root) else {
            return false;
        };
        if self.named_state(below) {
            return true;
        }
        match self.state_id(false) {
            Ok(Some(state)) => self.reaches(real, &state),
            Ok(None) => false,
            Err(_) => true,
        }
    }

    /// Whether `real` or a folder above it, below the project folder, has
    /// the state folder's identity.
    fn reaches(&self, real: &Path, state: &FileId) -> bool {
        real.ancestors()
            .take_while(|a| *a != self.root.as_path())
            .any(|a| file_id(a).as_ref() == Some(state))
    }

    /// The one check every path aispice reads or writes for a tool passes
    /// once its symlinks are resolved: inside the project folder (compared
    /// as paths, both canonical) and not in `.aispice/` (by identity).
    /// `rel` is the path as given, for the message.
    pub fn check_real(&self, real: &Path, rel: &str) -> Result<(), ProjectError> {
        let state = self.state_id(false)?;
        self.check_with(real, rel, state.as_ref())
    }

    fn check_with(
        &self,
        real: &Path,
        rel: &str,
        state: Option<&FileId>,
    ) -> Result<(), ProjectError> {
        let outside = || ProjectError::Outside(rel.to_string());
        let below = real.strip_prefix(&self.root).map_err(|_| outside())?;
        if self.named_state(below) || state.is_some_and(|s| self.reaches(real, s)) {
            return Err(outside());
        }
        Ok(())
    }

    /// Whether aispice writes a file or folder of this name: an early
    /// refusal of names Windows reads as something else, so no trailing dot
    /// or space (dropped there), no colon (an alternate data stream), no `~`
    /// before a digit (a short 8.3 name such as `AISPIC~1`) and no device
    /// name (`CON`, `NUL`, `COM1`...).
    pub fn plain_name(name: &str) -> bool {
        let b = name.as_bytes();
        let stem = name.split('.').next().unwrap_or_default().trim_end();
        let device = matches!(
            stem.to_ascii_uppercase().as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || (stem.len() == 4
            && ["COM", "LPT"]
                .iter()
                .any(|d| stem.is_char_boundary(3) && stem[..3].eq_ignore_ascii_case(d))
            && stem.as_bytes()[3].is_ascii_digit());
        !name.is_empty()
            && !device
            && !name.ends_with(['.', ' '])
            && !name.contains(':')
            && !b.windows(2).any(|w| w[0] == b'~' && w[1].is_ascii_digit())
    }

    /// The checks an output path gets before anything is made or written:
    /// given without `..`, `~`, a drive or control characters, every name
    /// below the project plain ([`Project::plain_name`]), and its deepest
    /// existing folder, symlinks resolved, inside the project and not in
    /// `.aispice/`, which is made first so its identity can be compared.
    fn output_walk(&self, rel: &str) -> Result<OutputWalk, ProjectError> {
        let rel = rel.trim();
        let outside = || ProjectError::Outside(rel.to_string());
        if rel.is_empty()
            || rel.contains('\0')
            || rel.starts_with('~')
            || rel.contains("..")
            || rel.chars().any(char::is_control)
        {
            return Err(outside());
        }
        let p = Path::new(rel);
        // A path rooted without a drive (`\x` on Windows) is relative to the
        // current drive, not the project.
        if p.components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
            || (p.has_root() && !p.is_absolute())
        {
            return Err(outside());
        }
        let bad_name = |n: &str| {
            ProjectError::Other(format!(
                "{rel}: aispice does not write a file or folder named `{n}` (a trailing dot or space, a colon, a short name or a device name)"
            ))
        };
        if !p.is_absolute() {
            for c in p.components() {
                let n = c.as_os_str().to_string_lossy();
                if !Self::plain_name(&n) {
                    return Err(bad_name(&n));
                }
            }
        }
        let candidate = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        let mut rest = vec![
            candidate
                .file_name()
                .ok_or_else(outside)?
                .to_string_lossy()
                .into_owned(),
        ];
        let mut base = candidate.parent().ok_or_else(outside)?.to_path_buf();
        loop {
            match std::fs::symlink_metadata(&base) {
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let name = base.file_name().ok_or_else(outside)?;
                    rest.insert(0, name.to_string_lossy().into_owned());
                    base = base.parent().ok_or_else(outside)?.to_path_buf();
                }
                Err(e) => return Err(io(&base, e)),
            }
        }
        let real = std::fs::canonicalize(&base).map_err(|e| io(&base, e))?;
        if !real.is_dir() {
            return Err(ProjectError::Other(format!(
                "{} is not a folder",
                base.display()
            )));
        }
        // Containment first, so nothing is made for a path outside.
        self.check_with(&real, rel, None)?;
        let state = self.state_id(true)?.ok_or_else(|| {
            ProjectError::Other(format!("{STATE_DIR} is not a plain folder of the project"))
        })?;
        self.check_with(&real, rel, Some(&state))?;
        for n in &rest {
            if !Self::plain_name(n) {
                return Err(bad_name(n));
            }
        }
        // Each name still to make, by name only until it exists.
        let mut at = real.clone();
        for n in &rest {
            at = at.join(n);
            self.check_with(&at, rel, Some(&state))?;
        }
        Ok(OutputWalk {
            folder: real,
            rest,
            state,
        })
    }

    /// Where a tool may write a new or replaced file: a path that passes the
    /// output checks (inside the project and outside `.aispice/`, no `..`,
    /// `~` or drive, plain names), whose folder exists (symlinks in it
    /// resolved, and still inside), and whose final name is not a symbolic
    /// link, since aispice never writes through one.
    pub fn resolve_output(&self, rel: &str) -> Result<PathBuf, ProjectError> {
        let walk = self.output_walk(rel)?;
        let [name] = &walk.rest[..] else {
            return Err(ProjectError::Missing(rel.trim().to_string()));
        };
        let target = walk.folder.join(name);
        if std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(ProjectError::Other(format!(
                "{} is a symbolic link; aispice does not write through links",
                rel.trim()
            )));
        }
        Ok(target)
    }

    /// The folders of an output path that do not exist yet, outermost first,
    /// after the checks [`Project::resolve_output`] makes.
    pub fn missing_dirs(&self, rel: &str) -> Result<Vec<PathBuf>, ProjectError> {
        let walk = self.output_walk(rel)?;
        let mut at = walk.folder;
        let mut out = Vec::new();
        for n in &walk.rest[..walk.rest.len() - 1] {
            at = at.join(n);
            out.push(at.clone());
        }
        Ok(out)
    }

    /// Make the folders [`Project::missing_dirs`] lists, one level at a
    /// time. Each is checked again as the file system made it, symlinks
    /// resolved: one that turns out outside the project or to be
    /// `.aispice/` or inside it (a link put there in between, a name the
    /// file system folds onto it) is removed again if this made it, and
    /// stops it.
    pub fn create_missing_dirs(&self, rel: &str) -> Result<(), ProjectError> {
        let walk = self.output_walk(rel)?;
        let mut at = walk.folder;
        for n in &walk.rest[..walk.rest.len() - 1] {
            let next = at.join(n);
            let made = match std::fs::create_dir(&next) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
                Err(e) => return Err(io(&next, e)),
            };
            let real = std::fs::canonicalize(&next).map_err(|e| io(&next, e))?;
            if !real.is_dir() || self.check_with(&real, rel, Some(&walk.state)).is_err() {
                if made {
                    let _ = std::fs::remove_dir(&next);
                }
                return Err(ProjectError::Outside(rel.trim().to_string()));
            }
            at = real;
        }
        Ok(())
    }

    /// Write any file of the project (a layout, its record), recording the
    /// previous and new contents in the history so it can be undone like a
    /// schematic edit. Returns the snapshot ids before and after.
    pub fn save_file_tracked(
        &self,
        rel: &str,
        bytes: &[u8],
        summary: &str,
    ) -> Result<(Option<String>, String), ProjectError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| ProjectError::Other("project lock poisoned".into()))?;
        let path = self.resolve_output(rel)?;
        let rel = self.relative(&path);
        let mut idx = self.read_index(&rel);
        self.capture_external(&rel, &mut idx)?;
        let before = idx.snapshots.get(idx.current).map(|s| s.id.clone());
        atomic_write(&path, bytes)?;
        let id = self.push_snapshot(&rel, &mut idx, bytes, summary)?;
        self.write_index(&rel, &idx)?;
        Ok((before, id))
    }

    /// The project-relative form of an absolute path inside the project.
    pub fn relative(&self, abs: &Path) -> String {
        abs.strip_prefix(&self.root)
            .unwrap_or(abs)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Circuit files, newest first, at most four folders deep.
    pub fn circuits(&self) -> Vec<CircuitEntry> {
        let mut out = Vec::new();
        walk(&self.root, &self.root, 4, &mut out);
        out.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.path.cmp(&b.path)));
        out
    }

    pub fn read_text(&self, rel: &str) -> Result<String, ProjectError> {
        let path = self.resolve(rel)?;
        let bytes = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProjectError::Missing(rel.into())
            } else {
                io(&path, e)
            }
        })?;
        Ok(aispice_core::encoding::decode(&bytes).0)
    }

    pub fn load(
        &self,
        rel: &str,
    ) -> Result<(Schematic, Vec<schematic::ParseWarning>), ProjectError> {
        if !rel.to_ascii_lowercase().ends_with(".asc") {
            return Err(ProjectError::NotSchematic(rel.to_string()));
        }
        let path = self.resolve(rel)?;
        let bytes = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProjectError::Missing(rel.into())
            } else {
                io(&path, e)
            }
        })?;
        Ok(schematic::parse_bytes(&bytes))
    }

    /// Create (if needed) and return a folder under the state directory,
    /// refusing to go through any symlink: a project can ship a `.aispice`
    /// that is a link elsewhere, and writes must never follow it.
    pub fn state_subdir(&self, parts: &[&str]) -> Result<PathBuf, ProjectError> {
        let mut dir = self.root.join(STATE_DIR);
        let mut chain = vec![dir.clone()];
        for p in parts {
            if p.is_empty() || p.contains(['/', '\\']) || *p == "." || *p == ".." {
                return Err(ProjectError::Outside((*p).to_string()));
            }
            dir = dir.join(p);
            chain.push(dir.clone());
        }
        for d in &chain {
            match std::fs::symlink_metadata(d) {
                Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
                    return Err(ProjectError::Other(format!(
                        "{} is not a plain folder; refusing to write through it",
                        d.display()
                    )));
                }
                Ok(_) => {}
                Err(_) => std::fs::create_dir(d).map_err(|e| io(d, e))?,
            }
        }
        Ok(dir)
    }

    /// Read the history index. It lives inside the project, so it is treated
    /// as untrusted: snapshot ids that are not plain `digits-digits` names are
    /// dropped, which keeps a crafted index from pointing reads or writes
    /// outside the history folder.
    fn read_index(&self, rel: &str) -> HistoryIndex {
        let Ok(dir) = self.state_subdir(&["history", &history_key(rel)]) else {
            return HistoryIndex::default();
        };
        let mut idx: HistoryIndex = read_plain(&dir.join("index.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        idx.snapshots.retain(|s| valid_snapshot_id(&s.id));
        if idx.current >= idx.snapshots.len() {
            idx.current = idx.snapshots.len().saturating_sub(1);
        }
        idx
    }

    fn write_index(&self, rel: &str, idx: &HistoryIndex) -> Result<(), ProjectError> {
        let dir = self.state_subdir(&["history", &history_key(rel)])?;
        let json =
            serde_json::to_vec_pretty(idx).map_err(|e| ProjectError::Other(e.to_string()))?;
        atomic_write(&dir.join("index.json"), &json)
    }

    /// Where a snapshot is stored. Goes through `state_subdir`, so neither a
    /// read nor a write can follow a symlink out of the project, and only
    /// well-formed ids are accepted.
    fn snapshot_path(&self, rel: &str, id: &str) -> Result<PathBuf, ProjectError> {
        if !valid_snapshot_id(id) {
            return Err(ProjectError::Other(format!("invalid snapshot id `{id}`")));
        }
        Ok(self
            .state_subdir(&["history", &history_key(rel)])?
            .join(format!("{id}.asc")))
    }

    /// Record the file as it is now if it differs from the last snapshot
    /// (a first sight, or a change made outside aispice).
    fn capture_external(&self, rel: &str, idx: &mut HistoryIndex) -> Result<(), ProjectError> {
        let path = self.resolve(rel)?;
        let Ok(on_disk) = std::fs::read(&path) else {
            return Ok(());
        };
        let last = idx
            .snapshots
            .get(idx.current)
            .and_then(|s| self.snapshot_path(rel, &s.id).ok())
            .and_then(|p| read_plain(&p).ok());
        if last.as_deref() != Some(on_disk.as_slice()) {
            let summary = if idx.snapshots.is_empty() {
                "Opened"
            } else {
                "Changed outside aispice"
            };
            self.push_snapshot(rel, idx, &on_disk, summary)?;
        }
        Ok(())
    }

    fn push_snapshot(
        &self,
        rel: &str,
        idx: &mut HistoryIndex,
        bytes: &[u8],
        summary: &str,
    ) -> Result<String, ProjectError> {
        if !idx.snapshots.is_empty() {
            idx.snapshots.truncate(idx.current + 1);
        }
        let time = now_ms();
        let id = format!("{time}-{}", idx.snapshots.len());
        self.state_subdir(&["history", &history_key(rel)])?;
        atomic_write(&self.snapshot_path(rel, &id)?, bytes)?;
        idx.snapshots.push(Snapshot {
            id: id.clone(),
            time,
            summary: summary.to_string(),
        });
        idx.current = idx.snapshots.len() - 1;
        // Keep the history bounded: 200 snapshots per circuit.
        while idx.snapshots.len() > 200 {
            let old = idx.snapshots.remove(0);
            if let Ok(old_path) = self.snapshot_path(rel, &old.id) {
                let _ = std::fs::remove_file(old_path);
            }
            idx.current -= 1;
        }
        Ok(id)
    }

    /// Save a schematic, recording the previous and new states. Returns the
    /// new snapshot id.
    pub fn save(&self, rel: &str, sch: &Schematic, summary: &str) -> Result<String, ProjectError> {
        self.save_tracked(rel, sch, summary).map(|(_, after)| after)
    }

    /// Save and return the snapshot ids before and after, so a front end can
    /// offer "undo this edit" by restoring the first.
    pub fn save_tracked(
        &self,
        rel: &str,
        sch: &Schematic,
        summary: &str,
    ) -> Result<(Option<String>, String), ProjectError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| ProjectError::Other("project lock poisoned".into()))?;
        let path = self.resolve(rel)?;
        let mut idx = self.read_index(rel);
        self.capture_external(rel, &mut idx)?;
        let before = idx.snapshots.get(idx.current).map(|s| s.id.clone());
        let bytes = schematic::write_bytes(sch);
        atomic_write(&path, &bytes)?;
        let id = self.push_snapshot(rel, &mut idx, &bytes, summary)?;
        self.write_index(rel, &idx)?;
        Ok((before, id))
    }

    /// Create a new schematic file. Refuses to overwrite.
    pub fn create(&self, rel: &str, sch: &Schematic) -> Result<String, ProjectError> {
        self.create_as(rel, sch, "Created")
    }

    /// [`create`](Self::create), recording `summary` as the first version's
    /// description in the history.
    pub fn create_as(
        &self,
        rel: &str,
        sch: &Schematic,
        summary: &str,
    ) -> Result<String, ProjectError> {
        if !rel.to_ascii_lowercase().ends_with(".asc") {
            return Err(ProjectError::NotSchematic(rel.to_string()));
        }
        let path = self.resolve(rel)?;
        if path.exists() {
            return Err(ProjectError::Other(format!("{rel} already exists")));
        }
        self.save(rel, sch, summary)
    }

    pub fn history(&self, rel: &str) -> Result<(Vec<Snapshot>, usize), ProjectError> {
        self.resolve(rel)?;
        let idx = self.read_index(rel);
        Ok((idx.snapshots, idx.current))
    }

    fn move_to(
        &self,
        rel: &str,
        target: impl FnOnce(&HistoryIndex) -> Option<usize>,
        what: &'static str,
    ) -> Result<Snapshot, ProjectError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| ProjectError::Other("project lock poisoned".into()))?;
        let path = self.resolve(rel)?;
        let mut idx = self.read_index(rel);
        self.capture_external(rel, &mut idx)?;
        let to = target(&idx).ok_or(ProjectError::NoHistory(what))?;
        let snap = idx
            .snapshots
            .get(to)
            .cloned()
            .ok_or(ProjectError::NoHistory(what))?;
        let bytes = read_plain(&self.snapshot_path(rel, &snap.id)?).map_err(|e| io(&path, e))?;
        atomic_write(&path, &bytes)?;
        idx.current = to;
        self.write_index(rel, &idx)?;
        Ok(snap)
    }

    pub fn undo(&self, rel: &str) -> Result<Snapshot, ProjectError> {
        self.move_to(
            rel,
            |i| i.current.checked_sub(1).filter(|_| i.snapshots.len() > 1),
            "undo",
        )
    }

    pub fn redo(&self, rel: &str) -> Result<Snapshot, ProjectError> {
        self.move_to(
            rel,
            |i| (i.current + 1 < i.snapshots.len()).then_some(i.current + 1),
            "redo",
        )
    }

    pub fn restore(&self, rel: &str, id: &str) -> Result<Snapshot, ProjectError> {
        let id = id.to_string();
        self.move_to(
            rel,
            move |i| i.snapshots.iter().position(|s| s.id == id),
            "restore",
        )
    }

    pub fn can_undo_redo(&self, rel: &str) -> (bool, bool) {
        let idx = self.read_index(rel);
        (
            idx.current > 0 && idx.snapshots.len() > 1,
            idx.current + 1 < idx.snapshots.len(),
        )
    }

    /// A private scratch folder for one simulation run, inside the state dir.
    pub fn run_dir(&self, run_id: &str) -> Result<PathBuf, ProjectError> {
        let safe: String = run_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if safe.is_empty() {
            return Err(ProjectError::Outside(run_id.to_string()));
        }
        self.state_subdir(&["runs", &safe])
    }
}

/// Read a file aispice wrote under its state folder, refusing anything that
/// is not a regular file. A project can ship a `.aispice` folder whose
/// snapshot or index files are symlinks; following one would let undo copy a
/// file from outside the project into a circuit the model then reads.
fn read_plain(path: &Path) -> std::io::Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    std::fs::read(path)
}

/// The history folder name for a circuit path: one safe path segment.
fn history_key(rel: &str) -> String {
    let key: String = rel
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // A key made only of dots would be `.` or `..`.
    if key.chars().all(|c| c == '.') {
        format!("_{key}")
    } else {
        key
    }
}

fn valid_snapshot_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_digit() || c == '-')
        && !id.starts_with('-')
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<CircuitEntry>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if depth > 1 && !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') {
                walk(root, &path, depth - 1, out);
            }
            continue;
        }
        let ext = path
            .extension()
            .map(|x| x.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if CIRCUIT_EXTS.contains(&ext.as_str()) {
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(CircuitEntry {
                path: rel,
                name,
                modified,
            });
        }
    }
}

/// Write a file by writing a sibling temporary file and renaming it over the
/// target, so a crash never leaves a half-written schematic. The temporary
/// file is created exclusively with a random name, so a symlink planted at a
/// predictable name cannot redirect the write; rename replaces a link at the
/// target rather than following it.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ProjectError> {
    use std::io::Write as _;
    let dir = path
        .parent()
        .ok_or_else(|| ProjectError::Other(format!("{} has no parent folder", path.display())))?;
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nonce = format!(
        "{:x}{:x}",
        now_ms(),
        std::process::id() as u64
            ^ (bytes.len() as u64).rotate_left(17)
            ^ (path.as_os_str().len() as u64)
    );
    let mut last_err = None;
    for attempt in 0..8u32 {
        let tmp = dir.join(format!(".{base}.{nonce}{attempt}.aispice-tmp"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(mut f) => {
                f.write_all(bytes).and_then(|_| f.sync_all()).map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    io(&tmp, e)
                })?;
                drop(f);
                return std::fs::rename(&tmp, path).map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    io(path, e)
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => return Err(io(&tmp, e)),
        }
    }
    Err(io(
        path,
        last_err.unwrap_or_else(|| std::io::Error::other("could not create a temporary file")),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aispice_core::edit::{EditOp, apply};

    const RC: &str = "Version 4\nSHEET 1 880 680\nWIRE 96 96 32 96\nFLAG 32 176 0\nSYMBOL voltage 32 80 R0\nSYMATTR InstName V1\nSYMATTR Value 1\nSYMBOL res 192 80 R90\nSYMATTR InstName R1\nSYMATTR Value 1k\nTEXT 0 232 Left 2 !.op\n";

    fn project() -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("rc.asc"), RC).unwrap();
        std::fs::write(dir.path().join("sub/deck.cir"), "t\nR1 a 0 1\n.op\n").unwrap();
        let p = Project::open(dir.path(), ProjectOptions::default()).unwrap();
        (dir, p)
    }

    #[test]
    fn output_folders_are_made_inside_and_never_through_links() {
        let (_d, p) = project();
        let missing = p
            .missing_dirs("macros/ota.macro/evidence/drc.json")
            .unwrap();
        assert_eq!(missing.len(), 3);
        assert!(p.missing_dirs("sub/x.gds").unwrap().is_empty());
        for bad in [
            "../x/y.gds",
            "a/../../y.gds",
            ".aispice/x/y.gds",
            "~/y.gds",
            "/etc/y.gds",
            "",
        ] {
            assert!(p.missing_dirs(bad).is_err(), "{bad}");
        }
        p.create_missing_dirs("macros/ota.macro/evidence/drc.json")
            .unwrap();
        assert!(p.root().join("macros/ota.macro/evidence").is_dir());
        // Absolute paths inside the project are fine, except on Windows,
        // where the output resolver refuses a drive.
        let abs = p.root().join("macros/a/b.txt").display().to_string();
        if cfg!(windows) {
            assert!(p.missing_dirs(&abs).is_err());
        } else {
            assert_eq!(p.missing_dirs(&abs).unwrap().len(), 1);
        }
        std::fs::write(p.root().join("file"), "x").unwrap();
        assert!(p.missing_dirs("file/x.gds").is_err());
        #[cfg(unix)]
        {
            let elsewhere = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(elsewhere.path(), p.root().join("link")).unwrap();
            let e = p.missing_dirs("link/new/x.gds").unwrap_err().to_string();
            assert!(e.contains("outside"), "{e}");
            // A link to a folder of the project is followed.
            std::fs::create_dir(p.root().join("real")).unwrap();
            std::os::unix::fs::symlink(p.root().join("real"), p.root().join("alias")).unwrap();
            p.create_missing_dirs("alias/a/b.gds").unwrap();
            assert!(p.root().join("real/a").is_dir());
            assert!(p.create_missing_dirs("link/new/x.gds").is_err());
            assert!(!elsewhere.path().join("new").exists());
        }
    }

    /// Every way an output path is checked, so a refusal cannot depend on
    /// which one a tool happens to call.
    fn refused_everywhere(p: &Project, bad: &str) {
        assert!(p.missing_dirs(bad).is_err(), "missing_dirs {bad}");
        assert!(
            p.create_missing_dirs(bad).is_err(),
            "create_missing_dirs {bad}"
        );
        assert!(p.resolve_output(bad).is_err(), "resolve_output {bad}");
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn state_folder_is_refused_in_any_case_where_names_fold() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join(".aispice")).unwrap();
        let p = Project::open_folding_case(d.path(), ProjectOptions::default()).unwrap();
        for bad in [
            ".AISPICE/out.gds",
            ".Aispice/out.gds",
            ".AISPICE/new/out.gds",
        ] {
            refused_everywhere(&p, bad);
        }
        assert_eq!(names_in(d.path()), [".aispice"], "nothing was made");
        // A folder that only differs in case on this file system is refused
        // too, as it would be the state folder where names fold.
        std::fs::create_dir_all(d.path().join(".AISPICE")).unwrap();
        refused_everywhere(&p, ".AISPICE/out.gds");
        assert!(p.resolve(".AISPICE/out.gds").is_err());
        // Normal nested outputs are still made.
        p.create_missing_dirs("macros/ota.macro/evidence/drc.json")
            .unwrap();
        assert!(
            p.resolve_output("macros/ota.macro/evidence/drc.json")
                .is_ok()
        );
        // Where names do not fold, `.AISPICE` is another folder.
        if cfg!(target_os = "linux") {
            let q = Project::open(d.path(), ProjectOptions::default()).unwrap();
            assert!(q.resolve_output(".AISPICE/out.gds").is_ok());
        }
    }

    #[test]
    fn names_windows_reads_as_other_files_are_refused() {
        let (d, p) = project();
        let before = names_in(d.path());
        for bad in [
            // Windows drops trailing dots and spaces: `.aispice.` opens
            // `.aispice`.
            ".aispice./out.gds",
            ".aispice /out.gds",
            "out./x.gds",
            "macros/x.gds.",
            // An 8.3 short name.
            "AISPIC~1/out.gds",
            "macros/OTAMAC~2/x.gds",
            // Alternate data streams.
            "x.gds:stream",
            ".aispice:x/out.gds",
            "macros/a:b/x.gds",
            // Devices.
            "CON/x.gds",
            "macros/nul.gds",
            "macros/Com1.v",
        ] {
            refused_everywhere(&p, bad);
        }
        assert_eq!(names_in(d.path()), before, "nothing was made");
        assert!(Project::plain_name("ota~v1.gds"));
        assert!(Project::plain_name("x.gds"));
        assert!(!Project::plain_name("run~1"));
        assert!(Project::plain_name("console.v"));
        assert!(Project::plain_name("com10.v"));
        assert!(!Project::plain_name("LPT9"));
        // The state check reads names as Windows opens them.
        for real in [".aispice.", ".aispice ", ".aispice:s", ".aispice::$DATA"] {
            assert!(p.in_state_dir(&p.root().join(real).join("x")), "{real}");
        }
        assert!(!p.in_state_dir(&p.root().join(".aispice2/x")));
    }

    /// Another name for the state folder, as a case variant or a short name
    /// is on file systems that fold names, is refused by its identity.
    #[cfg(unix)]
    #[test]
    fn the_state_folder_is_known_by_identity_not_name() {
        let (d, p) = project();
        // An output makes the state folder first, as a plain folder.
        assert!(p.missing_dirs("out/x.gds").is_ok());
        assert!(d.path().join(".aispice").is_dir());
        std::fs::create_dir(d.path().join(".aispice/runs")).unwrap();
        std::os::unix::fs::symlink(d.path().join(".aispice"), d.path().join("alias")).unwrap();
        for path in ["alias", "alias/runs", "alias/runs/x.gds", "alias/new"] {
            let path = p.root().join(path);
            assert!(p.in_state_dir(&path), "{}", path.display());
            assert!(p.check_real(&path, "alias").is_err());
        }
        assert!(!p.in_state_dir(&p.root().join("sub")));
        assert!(!p.in_state_dir(&p.root().join("sub/x.gds")));
        refused_everywhere(&p, "alias/runs/x.gds");
        refused_everywhere(&p, "alias/new/x.gds");
        assert_eq!(names_in(&d.path().join(".aispice")), ["runs"]);
        // Normal nested outputs still work.
        p.create_missing_dirs("out/deep/x.gds").unwrap();
        assert_eq!(
            p.resolve_output("out/deep/x.gds").unwrap(),
            p.root().join("out/deep/x.gds")
        );
    }

    #[test]
    fn a_state_folder_that_is_not_a_plain_folder_stops_outputs() {
        let (d, p) = project();
        std::fs::write(d.path().join(".aispice"), "x").unwrap();
        refused_everywhere(&p, "out/x.gds");
        refused_everywhere(&p, "x.gds");
        assert!(!d.path().join("out").exists());
        #[cfg(unix)]
        {
            let (d, p) = project();
            let elsewhere = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(elsewhere.path(), d.path().join(".aispice")).unwrap();
            refused_everywhere(&p, "out/x.gds");
            assert!(names_in(elsewhere.path()).is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_project_opened_through_a_link_keeps_its_state_folder() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        std::fs::create_dir_all(real.join(".aispice")).unwrap();
        std::os::unix::fs::symlink(&real, d.path().join("link")).unwrap();
        let p = Project::open(&d.path().join("link"), ProjectOptions::default()).unwrap();
        let canonical = std::fs::canonicalize(&real).unwrap();
        assert_eq!(p.root(), canonical);
        for bad in [
            d.path().join("link/.aispice/out.gds"),
            d.path().join("link/.aispice/new/out.gds"),
            real.join(".aispice/out.gds"),
        ] {
            let bad = bad.display().to_string();
            refused_everywhere(&p, &bad);
            assert!(p.resolve(&bad).is_err(), "{bad}");
        }
        assert!(names_in(&real.join(".aispice")).is_empty());
        // A link inside the project that leads into the state folder.
        std::os::unix::fs::symlink(real.join(".aispice"), real.join("state")).unwrap();
        refused_everywhere(&p, "state/new/out.gds");
        refused_everywhere(&p, "state/out.gds");
        assert!(names_in(&real.join(".aispice")).is_empty());
        // An output given through the link lands in the real folder.
        let ok = d.path().join("link/macros/a/b.gds").display().to_string();
        assert_eq!(p.missing_dirs(&ok).unwrap().len(), 2);
        p.create_missing_dirs(&ok).unwrap();
        assert_eq!(
            p.resolve_output(&ok).unwrap(),
            canonical.join("macros/a/b.gds")
        );
    }

    #[test]
    fn lists_circuits_and_skips_state() {
        let (_d, p) = project();
        std::fs::create_dir_all(p.root().join(".aispice/history")).unwrap();
        std::fs::write(p.root().join(".aispice/history/x.asc"), RC).unwrap();
        let names: Vec<String> = p.circuits().into_iter().map(|c| c.path).collect();
        assert!(names.contains(&"rc.asc".to_string()));
        assert!(names.contains(&"sub/deck.cir".to_string()));
        assert_eq!(names.len(), 2, "{names:?}");
    }

    #[test]
    fn paths_cannot_escape() {
        let (_d, p) = project();
        for bad in [
            "../x.asc",
            "/etc/passwd",
            "sub/../../x",
            ".aispice/history/index.json",
            "",
            "a\0b",
        ] {
            assert!(p.resolve(bad).is_err(), "{bad}");
        }
        assert!(p.resolve("rc.asc").is_ok());
        assert!(p.resolve("new.asc").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_project_are_refused() {
        let (_d, p) = project();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.asc"), RC).unwrap();
        std::os::unix::fs::symlink(outside.path(), p.root().join("link")).unwrap();
        assert!(p.resolve("link/secret.asc").is_err());
    }

    #[test]
    fn save_undo_redo_round_trip() {
        let (_d, p) = project();
        let (mut sch, _) = p.load("rc.asc").unwrap();
        let ops: Vec<EditOp> =
            serde_json::from_str(r#"[{"op":"set_value","name":"R1","value":"2.2k"}]"#).unwrap();
        apply(&mut sch, p.library(), &ops).unwrap();
        p.save("rc.asc", &sch, "R1 to 2.2k").unwrap();
        assert!(p.read_text("rc.asc").unwrap().contains("2.2k"));
        assert_eq!(p.can_undo_redo("rc.asc"), (true, false));
        p.undo("rc.asc").unwrap();
        assert!(p.read_text("rc.asc").unwrap().contains("SYMATTR Value 1k"));
        p.redo("rc.asc").unwrap();
        assert!(p.read_text("rc.asc").unwrap().contains("2.2k"));
        let (snaps, current) = p.history("rc.asc").unwrap();
        assert_eq!(snaps.len(), 2);
        assert_eq!(current, 1);
        assert_eq!(snaps[0].summary, "Opened");
    }

    #[test]
    fn crafted_history_index_cannot_escape() {
        let (_d, p) = project();
        let (sch, _) = p.load("rc.asc").unwrap();
        p.save("rc.asc", &sch, "first").unwrap();
        let dir = p.root().join(".aispice/history/rc.asc");
        std::fs::write(dir.join("index.json"), r#"{"snapshots":[{"id":"../../../../tmp/evil","time":0,"summary":"x"},{"id":"1-0","time":0,"summary":"y"}],"current":0}"#).unwrap();
        let (snaps, current) = p.history("rc.asc").unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].id, "1-0");
        assert_eq!(current, 0);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rc.asc"), RC).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(".aispice")).unwrap();
        let p = Project::open(dir.path(), ProjectOptions::default()).unwrap();
        let (sch, _) = p.load("rc.asc").unwrap();
        assert!(p.save("rc.asc", &sch, "x").is_err());
        assert_eq!(
            std::fs::read_dir(outside.path()).unwrap().count(),
            0,
            "nothing written through the link"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_snapshot_files_are_not_followed() {
        let (_d, p) = project();
        let (mut sch, _) = p.load("rc.asc").unwrap();
        p.save("rc.asc", &sch, "first").unwrap();
        sch.symbol_mut("R1").unwrap().set_attr("Value", "2k");
        p.save("rc.asc", &sch, "second").unwrap();
        let (snaps, _) = p.history("rc.asc").unwrap();
        let first = p
            .root()
            .join(".aispice/history/rc.asc")
            .join(format!("{}.asc", snaps[snaps.len() - 2].id));
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "TOP SECRET").unwrap();
        std::fs::remove_file(&first).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), &first).unwrap();
        assert!(p.undo("rc.asc").is_err());
        assert!(!p.read_text("rc.asc").unwrap().contains("TOP SECRET"));
    }

    #[test]
    fn external_changes_are_captured_before_the_next_edit() {
        let (_d, p) = project();
        let (sch, _) = p.load("rc.asc").unwrap();
        p.save("rc.asc", &sch, "first save").unwrap();
        // Someone edits the file in LTspice.
        let edited = RC.replace("SYMATTR Value 1k", "SYMATTR Value 47k");
        std::fs::write(p.root().join("rc.asc"), &edited).unwrap();
        let (mut sch2, _) = p.load("rc.asc").unwrap();
        sch2.symbol_mut("V1").unwrap().set_attr("Value", "5");
        p.save("rc.asc", &sch2, "V1 to 5").unwrap();
        let (snaps, _) = p.history("rc.asc").unwrap();
        let summaries: Vec<&str> = snaps.iter().map(|s| s.summary.as_str()).collect();
        assert!(
            summaries.contains(&"Changed outside aispice"),
            "{summaries:?}"
        );
        p.undo("rc.asc").unwrap();
        assert!(
            p.read_text("rc.asc").unwrap().contains("47k"),
            "undo returns to the outside edit"
        );
    }
}

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
    root: PathBuf,
    lib: Arc<SymbolLibrary>,
    options: ProjectOptions,
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
            write_lock: Mutex::new(()),
        })
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
        if !real.starts_with(&self.root) || real.starts_with(self.root.join(STATE_DIR)) {
            return Err(ProjectError::Outside(rel.to_string()));
        }
        Ok(real)
    }

    /// Where a tool may write a new or replaced file: a path inside the
    /// project and outside `.aispice/`, given without `..`, `~` or a drive,
    /// whose folder exists (symlinks in it resolved, and still inside), and
    /// whose final name is not a symbolic link, since aispice never writes
    /// through one.
    pub fn resolve_output(&self, rel: &str) -> Result<PathBuf, ProjectError> {
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
        let candidate = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        let name = candidate.file_name().ok_or_else(outside)?.to_owned();
        let parent = candidate.parent().ok_or_else(outside)?;
        let real_parent =
            std::fs::canonicalize(parent).map_err(|_| ProjectError::Missing(rel.to_string()))?;
        if !real_parent.starts_with(&self.root)
            || real_parent.starts_with(self.root.join(STATE_DIR))
        {
            return Err(outside());
        }
        let target = real_parent.join(name);
        if std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(ProjectError::Other(format!(
                "{rel} is a symbolic link; aispice does not write through links"
            )));
        }
        Ok(target)
    }

    /// The folders of an output path that do not exist yet, outermost first,
    /// after checking the path as [`Project::resolve_output`] would once they
    /// do: given without `..`, `~` or a drive, inside the project and outside
    /// `.aispice/`, and every folder of it that exists a plain folder, not a
    /// link.
    pub fn missing_dirs(&self, rel: &str) -> Result<Vec<PathBuf>, ProjectError> {
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
        let p = if p.is_absolute() {
            p.strip_prefix(&self.root).map_err(|_| outside())?
        } else {
            p
        };
        if p.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(outside());
        }
        let mut dirs: Vec<&std::ffi::OsStr> = p.components().map(|c| c.as_os_str()).collect();
        dirs.pop();
        if dirs.first().is_some_and(|d| *d == STATE_DIR) {
            return Err(outside());
        }
        let mut cur = self.root.clone();
        let mut missing = Vec::new();
        for d in dirs {
            cur = cur.join(d);
            if !missing.is_empty() {
                missing.push(cur.clone());
                continue;
            }
            match std::fs::symlink_metadata(&cur) {
                Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(ProjectError::Other(format!(
                        "{} is not a plain folder; aispice does not write through it",
                        self.relative(&cur)
                    )));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing.push(cur.clone()),
                Err(e) => return Err(io(&cur, e)),
            }
        }
        Ok(missing)
    }

    /// Make the folders [`Project::missing_dirs`] lists, one level at a time:
    /// a folder that appears in between as a link or a file stops it.
    pub fn create_missing_dirs(&self, rel: &str) -> Result<(), ProjectError> {
        for d in self.missing_dirs(rel)? {
            match std::fs::create_dir(&d) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let m = std::fs::symlink_metadata(&d).map_err(|e| io(&d, e))?;
                    if !m.is_dir() || m.file_type().is_symlink() {
                        return Err(ProjectError::Other(format!(
                            "{} is not a plain folder; aispice does not write through it",
                            self.relative(&d)
                        )));
                    }
                }
                Err(e) => return Err(io(&d, e)),
            }
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
        // Absolute paths inside the project are fine.
        let abs = p.root().join("macros/a/b.txt").display().to_string();
        assert_eq!(p.missing_dirs(&abs).unwrap().len(), 1);
        std::fs::write(p.root().join("file"), "x").unwrap();
        assert!(p.missing_dirs("file/x.gds").is_err());
        #[cfg(unix)]
        {
            let elsewhere = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(elsewhere.path(), p.root().join("link")).unwrap();
            let e = p.missing_dirs("link/new/x.gds").unwrap_err().to_string();
            assert!(e.contains("not a plain folder"), "{e}");
            assert!(p.create_missing_dirs("link/new/x.gds").is_err());
            assert!(!elsewhere.path().join("new").exists());
        }
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

use super::{SymbolDef, builtin, parse_asy};
use crate::encoding;
use crate::schematic::normalize_symbol_name;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Where a resolved symbol came from. Reported to the user and the model so a
/// surprising pin layout can be traced to its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolSource {
    Project,
    User,
    Ltspice,
    Builtin,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum LibraryError {
    #[error(
        "no symbol named `{0}` in the project, the configured folders, LTspice's library or aispice's built-in set"
    )]
    NotFound(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolHit {
    /// Name as it would be written in an `.asc` SYMBOL line.
    pub name: String,
    pub source: SymbolSource,
    pub description: Option<String>,
    pub prefix: String,
    pub pins: Vec<String>,
}

struct Dir {
    root: PathBuf,
    source: SymbolSource,
    /// Lower-cased relative path without extension, `/` separated, to file.
    index: HashMap<String, PathBuf>,
    /// Lower-cased base name to relative paths, for names given without a folder.
    by_base: HashMap<String, Vec<String>>,
}

/// A resolved symbol and where it came from.
pub type Resolved = (Arc<SymbolDef>, SymbolSource);

/// Resolves symbol names to definitions, caching parsed files.
pub struct SymbolLibrary {
    dirs: Vec<Dir>,
    cache: RwLock<HashMap<String, Option<Resolved>>>,
}

impl std::fmt::Debug for SymbolLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SymbolLibrary")
            .field(
                "dirs",
                &self
                    .dirs
                    .iter()
                    .map(|d| (&d.root, d.source))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Default for SymbolLibrary {
    fn default() -> Self {
        Self::builtin_only()
    }
}

impl SymbolLibrary {
    /// Only aispice's own symbols. Deterministic, so tests use it.
    pub fn builtin_only() -> Self {
        Self {
            dirs: Vec::new(),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// The usual chain for a project: its folder, extra folders, LTspice's
    /// library if one is installed, then the built-ins.
    pub fn for_project(
        project: Option<&Path>,
        extra: &[PathBuf],
        ltspice_lib: Option<&Path>,
    ) -> Self {
        let mut lib = Self::builtin_only();
        if let Some(p) = project {
            lib = lib.with_dir(p, SymbolSource::Project);
        }
        for d in extra {
            lib = lib.with_dir(d, SymbolSource::User);
        }
        if let Some(l) = ltspice_lib {
            lib = lib.with_dir(l, SymbolSource::Ltspice);
        }
        lib
    }

    /// Add a folder to search, after the ones already added.
    pub fn with_dir(mut self, root: &Path, source: SymbolSource) -> Self {
        let mut index = HashMap::new();
        let mut by_base: HashMap<String, Vec<String>> = HashMap::new();
        // The project folder is searched shallowly: symbols sit next to the
        // schematic. Libraries are searched three levels deep (sym/OpAmps/x).
        let depth = if source == SymbolSource::Project {
            1
        } else {
            3
        };
        collect_asy(root, root, depth, &mut index);
        for key in index.keys() {
            let base = key.rsplit('/').next().unwrap_or(key).to_string();
            by_base.entry(base).or_default().push(key.clone());
        }
        for v in by_base.values_mut() {
            v.sort_by_key(|k| (k.matches('/').count(), k.clone()));
        }
        self.dirs.push(Dir {
            root: root.to_path_buf(),
            source,
            index,
            by_base,
        });
        self.cache.write().expect("cache lock").clear();
        self
    }

    pub fn dirs(&self) -> Vec<(PathBuf, SymbolSource)> {
        self.dirs
            .iter()
            .map(|d| (d.root.clone(), d.source))
            .collect()
    }

    /// Resolve a symbol name as written in an `.asc` file.
    pub fn resolve(&self, name: &str) -> Result<Resolved, LibraryError> {
        let key = normalize_symbol_name(name);
        if let Some(hit) = self.cache.read().expect("cache lock").get(&key) {
            return hit
                .clone()
                .ok_or_else(|| LibraryError::NotFound(name.to_string()));
        }
        let found = self.lookup(&key);
        self.cache
            .write()
            .expect("cache lock")
            .insert(key, found.clone());
        found.ok_or_else(|| LibraryError::NotFound(name.to_string()))
    }

    fn lookup(&self, key: &str) -> Option<Resolved> {
        let base = key.rsplit('/').next().unwrap_or(key);
        for dir in &self.dirs {
            let path = dir.index.get(key).or_else(|| {
                dir.by_base
                    .get(base)
                    .and_then(|keys| keys.first())
                    .and_then(|k| dir.index.get(k))
            });
            if let Some(path) = path
                && let Ok(bytes) = std::fs::read(path)
            {
                let (text, _) = encoding::decode(&bytes);
                return Some((Arc::new(parse_asy(&text)), dir.source));
            }
        }
        // PDK device symbols also resolve without their folder, as
        // `sg13_lv_nmos`, the name xschem and the PDK use.
        builtin::get(key)
            .or_else(|| builtin::get(base))
            .or_else(|| builtin::get(&format!("sg13g2/{base}")))
            .map(|text| (Arc::new(parse_asy(text)), SymbolSource::Builtin))
    }

    /// Search names and descriptions. An empty query lists the built-ins and
    /// top-level library symbols.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SymbolHit> {
        let q = query.trim().to_ascii_lowercase();
        let mut seen = std::collections::HashSet::new();
        let mut hits = Vec::new();
        let mut consider = |name: String, source: SymbolSource, hits: &mut Vec<SymbolHit>| {
            let key = normalize_symbol_name(&name);
            if !seen.insert(key) {
                return;
            }
            let Ok((def, _)) = self.resolve(&name) else {
                return;
            };
            let desc = def.description().map(str::to_string);
            let matches = q.is_empty()
                || name.to_ascii_lowercase().contains(&q)
                || desc
                    .as_deref()
                    .is_some_and(|d| d.to_ascii_lowercase().contains(&q));
            if matches {
                hits.push(SymbolHit {
                    pins: def
                        .pins_in_spice_order()
                        .iter()
                        .map(|p| p.name.clone())
                        .collect(),
                    prefix: def.prefix().to_string(),
                    name,
                    source,
                    description: desc,
                });
            }
        };
        for dir in &self.dirs {
            let mut keys: Vec<&String> = dir.index.keys().collect();
            keys.sort();
            for key in keys {
                if q.is_empty() && key.contains('/') {
                    continue;
                }
                let name =
                    original_case_name(&dir.root, &dir.index[key]).unwrap_or_else(|| key.clone());
                consider(name, dir.source, &mut hits);
                if hits.len() >= limit {
                    return hits;
                }
            }
        }
        for name in builtin::NAMES {
            consider(name.to_string(), SymbolSource::Builtin, &mut hits);
            if hits.len() >= limit {
                break;
            }
        }
        hits
    }
}

fn collect_asy(root: &Path, dir: &Path, depth: usize, out: &mut HashMap<String, PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 1 {
                collect_asy(root, &path, depth - 1, out);
            }
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("asy"))
            && let Ok(rel) = path.with_extension("").strip_prefix(root)
        {
            let key = rel
                .to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase();
            out.entry(key).or_insert(path);
        }
    }
}

/// The relative name with the file system's casing and LTspice's `\`
/// separator, as LTspice writes it into schematics.
fn original_case_name(root: &Path, path: &Path) -> Option<String> {
    let rel = path.with_extension("");
    let rel = rel.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('/', "\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_resolve_with_ltspice_pin_positions() {
        let lib = SymbolLibrary::builtin_only();
        let (res, source) = lib.resolve("res").unwrap();
        assert_eq!(source, SymbolSource::Builtin);
        let pins: Vec<_> = res
            .pins
            .iter()
            .map(|p| (p.name.as_str(), p.at.x, p.at.y))
            .collect();
        assert_eq!(pins, vec![("A", 16, 16), ("B", 16, 96)]);
        let (op, _) = lib.resolve("OpAmps\\opamp2").unwrap();
        assert_eq!(op.prefix(), "X");
        assert_eq!(op.pins.len(), 5);
    }

    #[test]
    fn unknown_symbol_is_an_error() {
        let lib = SymbolLibrary::builtin_only();
        assert!(matches!(
            lib.resolve("no_such_part"),
            Err(LibraryError::NotFound(_))
        ));
    }

    #[test]
    fn project_folder_overrides_builtin() {
        let dir = std::env::temp_dir().join(format!("aispice-symlib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("res.asy"), "SymbolType CELL\nSYMATTR Prefix R\nPIN 0 0 NONE 0\nPINATTR PinName A\nPIN 0 64 NONE 0\nPINATTR PinName B\n").unwrap();
        let lib = SymbolLibrary::builtin_only().with_dir(&dir, SymbolSource::Project);
        let (res, source) = lib.resolve("res").unwrap();
        assert_eq!(source, SymbolSource::Project);
        assert_eq!(res.pins[1].at.y, 64);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn search_finds_by_description() {
        let lib = SymbolLibrary::builtin_only();
        let hits = lib.search("resistor", 10);
        assert!(hits.iter().any(|h| h.name == "res"));
    }
}

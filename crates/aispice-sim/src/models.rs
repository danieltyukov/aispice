//! Models and subcircuits a netlist uses but does not define.
//!
//! LTspice finds `2N3904`, `1N4148` or `opamp.sub` in its own library without
//! being told where. Other simulators need the definitions in the deck, so
//! before a netlist goes to ngspice, Xyce or Spectre every referenced but
//! undefined model and subcircuit is appended to it: first from the user's
//! LTspice installation (read at run time, never copied into aispice), then
//! from aispice's embedded generic library. Whatever is still missing is
//! reported by name, so the failure is explained before the simulator gives
//! its own, vaguer error.

use aispice_core::netlist::{Line, Netlist, Subckt, parse};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// aispice's own generic model library (MIT). See the file header for what
/// the models are and are not.
pub const GENERIC_LIBRARY: &str = include_str!("models/generic.lib");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "path", rename_all = "snake_case")]
pub enum ModelSource {
    /// A file in the user's LTspice library.
    Ltspice(PathBuf),
    /// aispice's embedded generic library.
    Embedded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddedModel {
    pub name: String,
    /// `model` or `subckt`.
    pub kind: String,
    pub source: ModelSource,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolvedModels {
    /// The netlist with definitions appended and the `.lib` lines that named
    /// LTspice library files removed.
    pub netlist: Netlist,
    pub added: Vec<AddedModel>,
    /// Referenced names nothing could supply.
    pub missing: Vec<String>,
    pub notes: Vec<String>,
}

/// One embedded definition, for listing what aispice can supply on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddedModel {
    pub name: String,
    pub kind: String,
    /// The comment block above the definition in the library.
    pub description: String,
}

/// Everything in the embedded library.
pub fn embedded_models() -> Vec<EmbeddedModel> {
    let mut out = Vec::new();
    let mut comment: Vec<String> = Vec::new();
    for line in GENERIC_LIBRARY.lines() {
        let t = line.trim();
        if let Some(c) = t.strip_prefix('*') {
            let c = c.trim();
            if !c.is_empty() {
                comment.push(c.to_string());
            }
            continue;
        }
        if t.is_empty() {
            comment.clear();
            continue;
        }
        let mut words = t.split_whitespace();
        let kind = match words.next().map(str::to_ascii_lowercase).as_deref() {
            Some(".model") => "model",
            Some(".subckt") => "subckt",
            _ => continue,
        };
        if let Some(name) = words.next() {
            out.push(EmbeddedModel {
                name: name.to_string(),
                kind: kind.to_string(),
                description: comment.join(" "),
            });
        }
        comment.clear();
    }
    out
}

/// Resolve against one LTspice library folder (the `lib` folder holding
/// `cmp` and `sub`), or none.
pub fn resolve(
    netlist: &Netlist,
    standard_libs: &[String],
    ltspice_lib_dir: Option<&Path>,
) -> ResolvedModels {
    let dirs: Vec<PathBuf> = ltspice_lib_dir.map(Path::to_path_buf).into_iter().collect();
    resolve_with(netlist, standard_libs, &dirs)
}

/// Resolve against several LTspice library folders, searched in order. XVII
/// has two: the installation's and the user's `Documents/LTspiceXVII/lib`.
pub fn resolve_with(
    netlist: &Netlist,
    standard_libs: &[String],
    lib_dirs: &[PathBuf],
) -> ResolvedModels {
    resolve_with_defined(netlist, standard_libs, lib_dirs, &[])
}

/// As [`resolve_with`], with names something else defines and that must not
/// be looked up: a PDK's devices, which its model header brings in.
pub fn resolve_with_defined(
    netlist: &Netlist,
    standard_libs: &[String],
    lib_dirs: &[PathBuf],
    defined_elsewhere: &[String],
) -> ResolvedModels {
    let mut lib = Library::new(lib_dirs);
    let mut out = ResolvedModels {
        netlist: netlist.clone(),
        ..Default::default()
    };

    // `.lib`/`.include` lines: files that exist stay (their definitions count
    // as defined); names LTspice would look up in its library are resolved
    // here and the line removed, since the other simulator cannot find them.
    let mut named_files: Vec<PathBuf> = Vec::new();
    let mut kept_defs: HashSet<String> = HashSet::new();
    let mut items = Vec::with_capacity(out.netlist.items.len());
    for item in std::mem::take(&mut out.netlist.items) {
        if let Line::Directive { text } = &item
            && let Some(file) = include_target(text)
        {
            let path = Path::new(&file);
            if path.is_absolute() && path.is_file() {
                if let Some(n) = lib.load(path) {
                    kept_defs.extend(definitions(n));
                }
                items.push(item);
                continue;
            }
            match lib.find_file(&file) {
                Some(found) => {
                    out.notes.push(format!(
                        "`{text}` resolved in the LTspice library: {}",
                        found.display()
                    ));
                    named_files.push(found);
                }
                None => out.notes.push(format!(
                    "`{text}`: no such file in the LTspice library; its definitions are looked up by name"
                )),
            }
            continue;
        }
        items.push(item);
    }
    out.netlist.items = items;

    kept_defs.extend(defined_elsewhere.iter().map(|n| n.to_ascii_uppercase()));
    let mut gave_up: HashSet<String> = HashSet::new();
    // Each round adds definitions whose bodies may reference more; real
    // libraries nest a few levels at most.
    for _ in 0..16 {
        let mut defined = definitions(&out.netlist);
        defined.extend(kept_defs.iter().cloned());
        let wanted: Vec<(String, RefKind)> = references(&out.netlist)
            .into_iter()
            .filter(|(n, _)| !defined.contains(&n.to_ascii_uppercase()))
            .filter(|(n, _)| !gave_up.contains(&n.to_ascii_uppercase()))
            .collect();
        if wanted.is_empty() {
            break;
        }
        let mut progress = false;
        for (name, kind) in wanted {
            let key = name.to_ascii_uppercase();
            if out.added.iter().any(|a| a.name.eq_ignore_ascii_case(&name)) {
                continue;
            }
            match lib.lookup(&name, kind, &named_files, standard_libs) {
                Some((line, source)) => {
                    if let ModelSource::Ltspice(p) = &source
                        && !named_files.contains(p)
                    {
                        // Later lookups (models a subcircuit uses) try the
                        // same file first, as LTspice does.
                        named_files.push(p.clone());
                    }
                    let what = match &line {
                        Line::Subckt(_) => "subckt",
                        _ => "model",
                    };
                    let origin = match &source {
                        ModelSource::Ltspice(p) => {
                            format!("from the LTspice library ({})", p.display())
                        }
                        ModelSource::Embedded => {
                            "aispice generic model, an approximation".to_string()
                        }
                    };
                    out.netlist.items.push(Line::Comment {
                        text: format!("{name}: {origin}"),
                    });
                    out.netlist.items.push(line);
                    out.added.push(AddedModel {
                        name: name.clone(),
                        kind: what.into(),
                        source,
                    });
                    progress = true;
                }
                None => {
                    gave_up.insert(key);
                    out.missing.push(name);
                }
            }
        }
        if !progress {
            break;
        }
    }
    out
}

/// What kind of definition a reference needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefKind {
    Model(char),
    Subckt,
}

/// Upper-cased names of every model and subcircuit defined anywhere in the
/// netlist, inside subcircuits included.
fn definitions(n: &Netlist) -> HashSet<String> {
    let mut out = HashSet::new();
    fn walk(items: &[Line], out: &mut HashSet<String>) {
        for item in items {
            match item {
                Line::Directive { text } => {
                    let mut w = text.split_whitespace();
                    if w.next().is_some_and(|k| k.eq_ignore_ascii_case(".model"))
                        && let Some(name) = w.next()
                    {
                        out.insert(model_name(name).to_ascii_uppercase());
                    }
                }
                Line::Subckt(s) => {
                    out.insert(s.name.to_ascii_uppercase());
                    walk(&s.body, out);
                }
                _ => {}
            }
        }
    }
    walk(&n.items, &mut out);
    out
}

/// `.model NAME(...)` written without a space keeps the parenthesis on the
/// name; strip it.
fn model_name(word: &str) -> &str {
    word.split('(').next().unwrap_or(word)
}

/// Every model or subcircuit name used by an element, in first-use order.
fn references(n: &Netlist) -> Vec<(String, RefKind)> {
    let mut out: Vec<(String, RefKind)> = Vec::new();
    fn push(out: &mut Vec<(String, RefKind)>, name: &str, kind: RefKind) {
        let name = name.trim();
        if name.is_empty()
            || name.starts_with(['{', '\'', '"'])
            || aispice_core::units::parse(name).is_some()
            || name.contains('=')
            || out.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
        {
            return;
        }
        out.push((name.to_string(), kind));
    }
    fn walk(items: &[Line], out: &mut Vec<(String, RefKind)>) {
        for item in items {
            match item {
                Line::Element(e) => {
                    let letter = e.letter();
                    let mut words = e.rest.split_whitespace();
                    match letter {
                        'X' => {
                            if let Some(w) = words.next() {
                                push(out, w, RefKind::Subckt);
                            }
                        }
                        'D' | 'Q' | 'M' | 'J' | 'Z' | 'S' | 'O' => {
                            if let Some(w) = words.next() {
                                push(out, w, RefKind::Model(letter));
                            }
                        }
                        'W' => {
                            if let Some(w) = words.nth(1) {
                                push(out, w, RefKind::Model(letter));
                            }
                        }
                        _ => {}
                    }
                }
                Line::Subckt(s) => walk(&s.body, out),
                _ => {}
            }
        }
    }
    walk(&n.items, &mut out);
    out
}

/// The file named by `.lib file`, `.lib file section`, `.include file` or
/// `.inc file`, without quotes, read as the netlist policy reads it: a line
/// in any other form names nothing here and is refused before a run.
fn include_target(directive: &str) -> Option<String> {
    Some(aispice_core::netlist::parse_include(directive)?.ok()?.file)
}

/// Lazily indexed LTspice library folders and parsed files.
struct Library {
    dirs: Vec<PathBuf>,
    /// Lower-cased file name to path, over `sub`, `cmp` and the folder itself.
    index: Option<HashMap<String, PathBuf>>,
    parsed: HashMap<PathBuf, Option<Netlist>>,
    embedded: Netlist,
}

impl Library {
    fn new(dirs: &[PathBuf]) -> Self {
        Self {
            dirs: dirs.to_vec(),
            index: None,
            parsed: HashMap::new(),
            embedded: parse(&format!("* aispice generic\n{GENERIC_LIBRARY}")),
        }
    }

    fn index(&mut self) -> &HashMap<String, PathBuf> {
        self.index.get_or_insert_with(|| {
            let mut idx = HashMap::new();
            for dir in &self.dirs {
                for sub in ["sub", "cmp", ""] {
                    let d = if sub.is_empty() {
                        dir.clone()
                    } else {
                        dir.join(sub)
                    };
                    let Ok(entries) = std::fs::read_dir(&d) else {
                        continue;
                    };
                    for e in entries.flatten() {
                        let p = e.path();
                        if p.is_file()
                            && let Some(name) = p.file_name().and_then(|n| n.to_str())
                        {
                            idx.entry(name.to_ascii_lowercase()).or_insert(p);
                        }
                    }
                }
            }
            idx
        })
    }

    /// A file named in a `.lib` line, as LTspice would find it: by base
    /// name, case-insensitively.
    fn find_file(&mut self, name: &str) -> Option<PathBuf> {
        let base = name
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(name)
            .to_ascii_lowercase();
        self.index().get(&base).cloned()
    }

    fn load(&mut self, path: &Path) -> Option<&Netlist> {
        if !self.parsed.contains_key(path) {
            let n = crate::library::read_library(path)
                .map(|text| parse(&format!("* {}\n{text}", path.display())));
            self.parsed.insert(path.to_path_buf(), n);
        }
        self.parsed.get(path).and_then(Option::as_ref)
    }

    fn find_in(&mut self, path: &Path, name: &str, kind: RefKind) -> Option<Line> {
        let n = self.load(path)?;
        find_definition(n, name, kind)
    }

    fn lookup(
        &mut self,
        name: &str,
        kind: RefKind,
        named: &[PathBuf],
        standard_libs: &[String],
    ) -> Option<(Line, ModelSource)> {
        for f in named {
            if let Some(line) = self.find_in(f, name, kind) {
                return Some((line, ModelSource::Ltspice(f.clone())));
            }
        }
        if !self.dirs.is_empty() {
            let candidates: Vec<String> = match kind {
                RefKind::Model(letter) => {
                    let mut files: Vec<String> = standard_libs.to_vec();
                    let by_letter = match letter {
                        'D' => "standard.dio",
                        'Q' => "standard.bjt",
                        'M' => "standard.mos",
                        'J' => "standard.jft",
                        _ => "",
                    };
                    for f in [
                        by_letter,
                        "standard.dio",
                        "standard.bjt",
                        "standard.mos",
                        "standard.jft",
                    ] {
                        if !f.is_empty() && !files.iter().any(|x| x.eq_ignore_ascii_case(f)) {
                            files.push(f.to_string());
                        }
                    }
                    files
                }
                RefKind::Subckt => ["sub", "lib", "cir", "mod", "txt"]
                    .iter()
                    .map(|ext| format!("{name}.{ext}"))
                    .collect(),
            };
            for file in candidates {
                if let Some(path) = self.find_file(&file)
                    && let Some(line) = self.find_in(&path, name, kind)
                {
                    return Some((line, ModelSource::Ltspice(path)));
                }
            }
        }
        find_definition(&self.embedded, name, kind).map(|l| (l, ModelSource::Embedded))
    }
}

fn find_definition(n: &Netlist, name: &str, kind: RefKind) -> Option<Line> {
    n.items.iter().find_map(|item| match (item, kind) {
        (Line::Subckt(s), RefKind::Subckt) if s.name.eq_ignore_ascii_case(name) => {
            Some(Line::Subckt(s.clone()))
        }
        (Line::Directive { text }, RefKind::Model(_)) => {
            let mut w = text.split_whitespace();
            let is_model = w.next().is_some_and(|k| k.eq_ignore_ascii_case(".model"));
            let n = w.next().map(model_name);
            (is_model && n.is_some_and(|n| n.eq_ignore_ascii_case(name))).then(|| item.clone())
        }
        _ => None,
    })
}

/// Subcircuits defined in the embedded library, for tests and listings.
pub fn embedded_subckt(name: &str) -> Option<Subckt> {
    let n = parse(&format!("* aispice generic\n{GENERIC_LIBRARY}"));
    n.subckts()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_library_lists_its_models() {
        let all = embedded_models();
        let names: Vec<_> = all.iter().map(|m| m.name.as_str()).collect();
        for want in [
            "opamp",
            "D",
            "1N4148",
            "1N4007",
            "2N3904",
            "2N3906",
            "2N2222",
            "2N7002",
            "BSS84",
            "NMOS_POWER",
        ] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        let op = all.iter().find(|m| m.name == "opamp").unwrap();
        assert_eq!(op.kind, "subckt");
        assert!(op.description.contains("single-pole"));
        let s = embedded_subckt("opamp").unwrap();
        // LTspice's port order: inverting, non-inverting, output.
        assert_eq!(s.ports, vec!["invin", "noninvin", "out"]);
        assert_eq!(s.params, "Aol=100K GBW=10Meg");
    }

    /// The rule check in aispice-core treats these subcircuits as defined;
    /// it must know exactly the ones this library supplies.
    #[test]
    fn lint_knows_every_embedded_subckt() {
        let mut embedded: Vec<String> = embedded_models()
            .into_iter()
            .filter(|m| m.kind == "subckt")
            .map(|m| m.name.to_ascii_lowercase())
            .collect();
        embedded.sort();
        let mut known: Vec<String> = aispice_core::lint::BUILTIN_SUBCKTS
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect();
        known.sort();
        assert_eq!(embedded, known);
    }

    #[test]
    fn embedded_fallback_without_ltspice() {
        let n = parse(
            "t\nXU1 inn inp out opamp Aol=100K GBW=10Meg\nD1 a 0 1N4148\nQ1 c b 0 2N3904\nM1 d g 0 0 NMOS_POWER\nR1 a 0 1k\n.lib opamp.sub\n.op\n",
        );
        let r = resolve(&n, &[], None);
        assert!(r.missing.is_empty(), "{:?}", r.missing);
        let names: Vec<_> = r.added.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["opamp", "1N4148", "2N3904", "NMOS_POWER"]);
        assert!(r.added.iter().all(|a| a.source == ModelSource::Embedded));
        assert!(!r.netlist.directives().any(|d| d.starts_with(".lib")));
        assert_eq!(r.netlist.subckts().count(), 1);
    }

    #[test]
    fn defined_models_are_left_alone_and_unknown_ones_reported() {
        let n = parse(
            "t\nD1 a 0 MYD\nX1 a b mysub\nQ1 c b 0 QX\n.model MYD D(Is=1n)\n.subckt mysub 1 2\nR1 1 2 1k\n.ends mysub\n",
        );
        let r = resolve(&n, &[], None);
        assert!(r.added.is_empty());
        assert_eq!(r.missing, vec!["QX"]);
    }

    #[test]
    fn names_defined_elsewhere_are_not_looked_up() {
        let n = parse("t\nXM1 d g 0 0 sg13_lv_nmos w=1u\nXU1 a b c opamp\n.op\n");
        let plain = resolve_with(&n, &[], &[]);
        assert_eq!(plain.missing, vec!["sg13_lv_nmos".to_string()]);
        let pdk = resolve_with_defined(&n, &[], &[], &["SG13_LV_NMOS".to_string()]);
        assert!(pdk.missing.is_empty(), "{:?}", pdk.missing);
        assert!(pdk.added.iter().any(|a| a.name == "opamp"));
    }

    #[test]
    fn include_targets() {
        assert_eq!(include_target(".lib opamp.sub"), Some("opamp.sub".into()));
        assert_eq!(
            include_target(".include \"my models.lib\""),
            Some("my models.lib".into())
        );
        assert_eq!(include_target(".lib lib.l tt"), Some("lib.l".into()));
        assert_eq!(include_target(".tran 1m"), None);
    }

    #[test]
    fn ltspice_library_is_preferred_when_present() {
        let dir = std::env::temp_dir().join(format!("aispice-models-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("cmp")).unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(
            dir.join("cmp/standard.dio"),
            "* test\r\n.model 1N4148 D(Is=2n Rs=.5)\r\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("sub/MyAmp.sub"),
            ".subckt myamp a b c\nE1 c 0 b a 1e5\nD1 c 0 DCLAMP\n.ends myamp\n",
        )
        .unwrap();
        let n = parse("t\nD1 a 0 1N4148\nX1 a b c MYAMP\n.op\n");
        let r = resolve(&n, &["standard.dio".to_string()], Some(&dir));
        let sources: Vec<_> = r
            .added
            .iter()
            .map(|a| (a.name.as_str(), matches!(a.source, ModelSource::Ltspice(_))))
            .collect();
        assert_eq!(sources, vec![("1N4148", true), ("MYAMP", true)]);
        // The subcircuit's own model is not anywhere.
        assert_eq!(r.missing, vec!["DCLAMP"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}

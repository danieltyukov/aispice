//! xschem import and export with their proofs, shared by the tools and the
//! CLI.
//!
//! The conversions themselves are in `aispice_core::xschem`; this module
//! finds what they need on this machine (xschem's library, the PDK's
//! symbols and model folders) and, for an export, adds the second proof:
//! xschem's own netlist of the `.sch`, made headless inside a sandbox by
//! `aispice_sim::xschem`, compared with the `.asc`'s netlist.
//!
//! xschem evaluates Tcl from the files it opens, so aispice runs it only on
//! files it wrote itself:
//!
//! - an export, written through the export's guard so no value in it is
//!   evaluated, is checked with xschem when xschem and a sandbox are
//!   available, and otherwise reported as not verified with xschem;
//! - an import is never run through xschem. It is proven by aispice's own
//!   parser, Tcl subset and netlister, and reported as not checked against
//!   xschem ([`import_xschem_check`]); docs/XSCHEM.md shows how to compare
//!   by hand.
//!
//! The safety scan (`aispice_core::xschem::safety`) still runs on an export
//! before xschem does, as defense in depth.

use crate::pdk::{ProjectConfig, Registry};
use aispice_core::layout::LayoutOptions;
use aispice_core::netlist::{Netlist, compare_with};
use aispice_core::schematic::Schematic;
use aispice_core::symbol::SymbolLibrary;
use aispice_core::xschem::export::{ExportOptions, Exported, export};
use aispice_core::xschem::import::{ImportOptions, Imported, Proof, clean, import};
use aispice_core::xschem::netlist::{Finding, Options};
use aispice_core::xschem::safety;
use aispice_core::xschem::symbol::{Resolver, read_limited};
use aispice_core::xschem::tcl::Env;
use aispice_core::xschem::{file, netlist};
use aispice_sim::pdk::Pdk;
use aispice_sim::xschem::{Run, Sandbox, Xschem};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Why an import is not checked against xschem.
pub const IMPORT_NOT_CHECKED: &str = "not checked against xschem: aispice runs xschem only on files it wrote itself, and xschem evaluates Tcl from the schematic it opens; the proof above is aispice's own netlist of the schematic (docs/XSCHEM.md shows how to compare with xschem by hand)";

/// xschem's side of an import's proof: never run.
pub fn import_xschem_check() -> XschemCheck {
    XschemCheck::Skipped {
        reason: IMPORT_NOT_CHECKED.into(),
    }
}

/// Where xschem, its library and the PDK's xschem symbols are.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    pub xschem: Option<Xschem>,
    /// xschem's library path, in search order.
    pub library: Vec<PathBuf>,
    /// Folders whose symbols may hold Tcl: xschem's own library and the
    /// PDK's `libs.tech/xschem`.
    pub trusted: Vec<PathBuf>,
    pub pdk: Option<Pdk>,
}

impl Setup {
    /// xschem as installed, and the PDK's xschem folder and models.
    pub fn new(pdk: Option<Pdk>) -> Self {
        let xschem = Xschem::find();
        let mut library = match &xschem {
            Some(x) => x.library(),
            None => std::env::var_os("AISPICE_XSCHEM_SHAREDIR")
                .map(PathBuf::from)
                .map(|s| vec![s.join("xschem_library/devices"), s.join("xschem_library")])
                .unwrap_or_default(),
        };
        library.retain(|d| d.is_dir());
        let mut trusted = library.clone();
        if let Some(p) = &pdk {
            let dir = p.root.join("libs.tech/xschem");
            if dir.is_dir() {
                library.push(dir.clone());
                trusted.push(dir);
            }
        }
        Self {
            xschem,
            library,
            trusted,
            pdk,
        }
    }

    fn models(&self, which: &str) -> Option<PathBuf> {
        let p = self.pdk.as_ref()?;
        let d = p.root.join("libs.tech").join(which).join("models");
        d.is_dir().then_some(d)
    }

    /// The variables IHP's schematics read through Tcl.
    pub fn env(&self) -> Env {
        let mut env = Env::new("ngspice");
        if let Some(m) = self.models("ngspice") {
            let m = m.display().to_string();
            env = env
                .with_var("MODELS_NGSPICE", &m)
                .with_var("SG13G2_MODELS", &m);
        }
        if let Some(m) = self.models("xyce") {
            let m = m.display().to_string();
            env = env
                .with_var("MODELS_XYCE", &m)
                .with_var("SG13G2_MODELS_XYCE", &m);
        }
        if let Some(p) = &self.pdk {
            if let Some(parent) = p.root.parent() {
                env = env.with_var("PDK_ROOT", &parent.display().to_string());
            }
            env = env.with_var("PDK", p.kit.kind);
        }
        env
    }

    /// A resolver over the library that may also read files in `roots`.
    pub fn resolver(&self, roots: Vec<PathBuf>) -> Resolver {
        Resolver::new(self.library.clone(), roots)
    }
}

/// The PDK a folder's `aispice.toml` names, else the only registered one.
pub fn pdk_for(project_root: Option<&Path>, registry: &Registry) -> Option<Pdk> {
    let named = project_root
        .and_then(|r| ProjectConfig::load(r).ok())
        .and_then(|c| c.pdk);
    let name = named.or_else(|| {
        (registry.pdks.len() == 1)
            .then(|| registry.pdks.keys().next().cloned())
            .flatten()
    })?;
    registry.open(&name).ok()
}

/// xschem's side of a proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum XschemCheck {
    Ran { proof: Proof },
    Skipped { reason: String },
}

impl XschemCheck {
    pub fn equivalent(&self) -> Option<bool> {
        match self {
            XschemCheck::Ran { proof } => Some(proof.equivalent),
            XschemCheck::Skipped { .. } => None,
        }
    }
}

/// An import, with both proofs.
#[derive(Debug, Clone)]
pub struct ImportOutcome {
    pub imported: Imported,
    pub xschem: XschemCheck,
}

/// An export, with both proofs.
#[derive(Debug, Clone)]
pub struct ExportOutcome {
    pub exported: Exported,
    pub xschem: XschemCheck,
}

/// Import the `.sch` at `sch`. `roots` are the folders besides the library
/// that schematic may pull files from; `out_dir` is where the `.asc` goes.
/// The most elements the agent's tools lay out, as for `create_schematic`.
pub const MAX_LAYOUT_ELEMENTS: usize = 150;

/// Layout settings for a conversion: how hard to search, and an element
/// limit when an agent asked for it.
#[derive(Debug, Clone, Copy)]
pub struct Effort {
    pub effort: u8,
    pub max_elements: Option<usize>,
}

pub fn import_file(
    setup: &Setup,
    sch: &Path,
    roots: Vec<PathBuf>,
    out_dir: Option<&Path>,
    lib: &SymbolLibrary,
    effort: Effort,
) -> Result<Imported, String> {
    let text = read_limited(sch).map_err(|e| format!("{}: {e}", sch.display()))?;
    let f = file::parse(&text).map_err(|e| format!("{}: {e}", sch.display()))?;
    let opts = ImportOptions {
        netlist: Options {
            env: setup.env(),
            ..Options::default()
        },
        layout: LayoutOptions {
            effort: effort.effort,
            ..LayoutOptions::default()
        },
        out_dir: out_dir.map(Path::to_path_buf),
        max_elements: effort.max_elements,
    };
    import(sch, &f, &setup.resolver(roots), lib, &opts).map_err(|e| e.to_string())
}

/// Why xschem cannot give a comparable netlist of an export, if it cannot.
fn skip_reason(builtin: &[String], verdict: &safety::Verdict) -> Option<String> {
    if !builtin.is_empty() {
        return Some(format!(
            "xschem would not find {} (only aispice's built-in definitions have {})",
            builtin.join(", "),
            if builtin.len() == 1 { "it" } else { "them" }
        ));
    }
    if !verdict.allowed() {
        let mut reasons = verdict.refused.clone();
        reasons.truncate(3);
        return Some(format!(
            "the safety scan refused it: {}",
            reasons.join("; ")
        ));
    }
    None
}

/// The PDK model header an export writes as a `MODEL` block: one `.lib`
/// per corner library the netlist's PDK devices need, at the typical
/// section, in the kit's order, without a path (as IHP's benches write it).
pub fn model_lines(netlist: &Netlist) -> Vec<String> {
    let mut out = Vec::new();
    for kit in aispice_core::pdk::KITS {
        let used: Vec<&str> = netlist
            .elements()
            .filter(|e| e.letter() == 'X')
            .filter_map(|e| kit.device(e.first_word()?))
            .map(|d| d.library)
            .collect();
        for l in kit.libraries {
            if used.contains(&l.file) {
                out.push(format!(".lib {} {}", l.file, l.family.default_section()));
            }
        }
    }
    out
}

/// Export an `.asc` to the `.sch` that will be written at `out`.
pub fn export_schematic(
    setup: &Setup,
    asc: &Schematic,
    lib: &SymbolLibrary,
    title: &str,
    out: &Path,
    roots: Vec<PathBuf>,
    effort: Effort,
) -> Result<Exported, String> {
    let (built, _) = aispice_core::netlist::build(asc, lib, title);
    let opts = ExportOptions {
        layout: LayoutOptions {
            effort: effort.effort,
            ..LayoutOptions::default()
        },
        max_elements: effort.max_elements,
        model_lines: model_lines(&built.netlist),
        netlist: Options {
            env: setup.env(),
            ..Options::default()
        },
    };
    export(asc, lib, title, out, &setup.resolver(roots), &opts).map_err(|e| e.to_string())
}

/// xschem's side of an export's proof. The `.sch` text is the export
/// writer's own ([`aispice_core::xschem::export::SchText`]): the only kind of
/// file aispice gives xschem. It is scanned (defense in depth) from a private
/// copy, then handed to xschem as text, which the sandbox code writes into a
/// fresh private folder itself; an export refers only to library and PDK
/// symbols, so it netlists the same there. Without xschem or a sandbox the
/// export is reported as not verified with xschem, never failed.
pub async fn check_export(
    setup: &Setup,
    file_name: &str,
    exported: &Exported,
    cancel: &CancellationToken,
) -> XschemCheck {
    let not_verified = |why: String| XschemCheck::Skipped {
        reason: format!("not verified with xschem: {why}"),
    };
    let Some(x) = &setup.xschem else {
        return not_verified(
            "xschem is not installed (set AISPICE_XSCHEM or put it on PATH)".into(),
        );
    };
    let name = sanitize(file_name);
    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => return not_verified(format!("no private folder for the scan: {e}")),
    };
    let dir = tmp
        .path()
        .canonicalize()
        .unwrap_or_else(|_| tmp.path().to_path_buf());
    let path = dir.join(&name);
    if let Err(e) = std::fs::write(&path, &exported.text) {
        return not_verified(format!("could not write the file to scan: {e}"));
    }
    let verdict = safety::scan(&path, &setup.resolver(vec![dir.clone()]), &setup.trusted);
    if let Some(reason) = skip_reason(&exported.xschem.builtin_symbols(), &verdict) {
        return not_verified(reason);
    }
    // bubblewrap if it can start here, else Landlock at ABI 6 with seccomp,
    // else no run.
    let sandbox = match Sandbox::select(cancel).await {
        Ok(s) => s,
        Err(e) => return not_verified(e),
    };
    let run = Run {
        library: setup.library.clone(),
        models_ngspice: setup.models("ngspice"),
        models_xyce: setup.models("xyce"),
    };
    let text = match aispice_sim::xschem::netlist(x, &sandbox, &name, &exported.text, &run, cancel)
        .await
    {
        Ok(t) => t,
        Err(e) => return not_verified(format!("xschem did not netlist the file: {e}")),
    };
    let theirs = aispice_core::netlist::parse(&clean(&text, None).text);
    XschemCheck::Ran {
        proof: Proof::new(
            "the .asc's netlist against xschem's own netlist of the .sch",
            compare_with(&exported.asc_netlist, &theirs, &exported.compare),
        ),
    }
}

/// A file name xschem may see: letters, digits, `.`, `_`, `-`.
fn sanitize(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.ends_with(".sch") && clean.len() > 4 && !clean.starts_with('.') {
        clean
    } else {
        "export.sch".into()
    }
}

/// A plain-text report of findings, notes and proofs.
pub fn report(
    findings: &[Finding],
    notes: &[String],
    aispice: &Proof,
    xschem: &XschemCheck,
) -> String {
    let mut t = String::new();
    let proof_line = |p: &Proof| {
        if p.equivalent {
            format!("same circuit ({})", p.compared)
        } else {
            format!(
                "DIFFERENT ({}): {}",
                p.compared,
                p.differences
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        }
    };
    t.push_str(&format!("Proof, aispice: {}\n", proof_line(aispice)));
    match xschem {
        XschemCheck::Ran { proof } => {
            t.push_str(&format!("Proof, xschem: {}\n", proof_line(proof)))
        }
        XschemCheck::Skipped { reason } => {
            t.push_str(&format!("Proof, xschem: not run: {reason}\n"))
        }
    }
    if !findings.is_empty() {
        t.push_str(&format!("{} finding(s):\n", findings.len()));
        for f in findings {
            t.push_str(&format!("  {f}\n"));
        }
    }
    for n in notes {
        t.push_str(&format!("  note: {n}\n"));
    }
    t
}

/// The structured form of a conversion's result, for tool data and
/// `--json`.
pub fn report_json(
    findings: &[Finding],
    notes: &[String],
    aispice: &Proof,
    xschem: &XschemCheck,
) -> serde_json::Value {
    serde_json::json!({
        "findings": findings,
        "notes": notes,
        "proofs": {"aispice": aispice, "xschem": xschem},
        "equivalent": aispice.equivalent && xschem.equivalent() != Some(false),
    })
}

/// aispice's xschem netlist of a schematic, for comparisons in tests.
pub fn aispice_netlist(setup: &Setup, sch: &Path, roots: Vec<PathBuf>) -> Result<String, String> {
    let opts = Options {
        env: setup.env(),
        ..Options::default()
    };
    netlist::netlist_file(sch, &setup.resolver(roots), &opts)
        .map(|n| n.text)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitized_names() {
        assert_eq!(sanitize("dir/my amp.sch"), "my_amp.sch");
        assert_eq!(sanitize("x.asc"), "export.sch");
        assert_eq!(sanitize("$x[1].sch"), "_x_1_.sch");
    }

    #[test]
    fn model_lines_follow_the_kit() {
        let n = aispice_core::netlist::parse(
            "t\nXM1 d g 0 0 sg13_lv_nmos w=1u l=1u\nXR1 a b 0 rppd w=1u l=1u\nXM2 d g 0 0 sg13_hv_pmos w=1u l=1u\nR1 a 0 1k\n",
        );
        assert_eq!(
            model_lines(&n),
            [
                ".lib cornerMOSlv.lib mos_tt",
                ".lib cornerMOShv.lib mos_tt",
                ".lib cornerRES.lib res_typ"
            ]
        );
        assert!(model_lines(&aispice_core::netlist::parse("t\nR1 a 0 1k\n")).is_empty());
    }
}

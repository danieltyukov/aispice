//! A project's behavioural models: declared in `aispice.toml`, read from
//! the project and checked, built by aispice in this process, and handed to
//! ngspice as bytes.
//!
//! A library is loaded only from the bytes a build in this process
//! produced: the copy under `.aispice/build/` is a record for the user and
//! is never read back, so a library planted there, or left by an earlier
//! session, is rebuilt rather than trusted.

use crate::pdk::{CosimDecl, PROJECT_FILE, ProjectConfig};
use crate::project::Project;
use aispice_core::netlist::{Line, Netlist};
use aispice_sim::behavioral::{
    self, BuildError, Library, LibraryKind, MAX_VERILOG_FILE_BYTES, MAX_VERILOGA_BYTES, Module,
    Source, verilog, veriloga,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A Verilog-A file the project declares, with the modules it defines.
#[derive(Debug, Clone)]
pub struct VaModel {
    pub source: Source,
    pub modules: Vec<Module>,
}

/// A co-simulated model the project declares.
#[derive(Debug, Clone)]
pub struct CosimModel {
    pub model: String,
    pub top: String,
    pub sources: Vec<Source>,
    /// The modules the sources declare.
    pub modules: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Declared {
    pub veriloga: Vec<VaModel>,
    pub cosim: Vec<CosimModel>,
}

impl Declared {
    pub fn is_empty(&self) -> bool {
        self.veriloga.is_empty() && self.cosim.is_empty()
    }

    /// The co-simulation model names, for the dialect.
    pub fn cosim_names(&self) -> Vec<String> {
        self.cosim.iter().map(|c| c.model.clone()).collect()
    }
}

/// Read a declared source: inside the project, a regular file, within
/// `limit` bytes, UTF-8.
fn read_source(project: &Project, rel: &str, limit: usize) -> Result<Source, String> {
    let path = project.resolve(rel).map_err(|e| format!("{rel}: {e}"))?;
    let meta = std::fs::symlink_metadata(&path).map_err(|_| format!("{rel} does not exist"))?;
    if !meta.is_file() {
        return Err(format!("{rel} is not a regular file"));
    }
    if meta.len() > limit as u64 {
        return Err(format!(
            "{rel} is {} bytes, over the {limit} byte limit",
            meta.len()
        ));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("{rel} could not be read: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{rel} is not UTF-8 text"))?;
    Ok(Source {
        name: rel.to_string(),
        text,
    })
}

/// The project's declared models, read and checked; nothing is compiled.
pub fn declared(project: &Project) -> Result<Declared, String> {
    let cfg = ProjectConfig::load(project.root())?;
    let mut out = Declared::default();
    for rel in &cfg.veriloga {
        let source = read_source(project, rel, MAX_VERILOGA_BYTES)?;
        let modules = veriloga::check(&source).map_err(|e| e.to_string())?;
        out.veriloga.push(VaModel { source, modules });
    }
    for CosimDecl {
        model,
        sources,
        top,
    } in &cfg.cosim
    {
        let sources = sources
            .iter()
            .map(|rel| read_source(project, rel, MAX_VERILOG_FILE_BYTES))
            .collect::<Result<Vec<_>, _>>()?;
        let modules = verilog::check(&sources).map_err(|e| e.to_string())?;
        if !modules.iter().any(|m| m == top) {
            return Err(format!(
                "{PROJECT_FILE}: [[cosim]] {model} names top module {top}, which its sources do not declare ({})",
                modules.join(", ")
            ));
        }
        out.cosim.push(CosimModel {
            model: model.clone(),
            top: top.clone(),
            sources,
            modules,
        });
    }
    Ok(out)
}

/// The words of a card as ngspice splits them, lower-cased.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || "=(),".contains(c))
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Which declared models a netlist uses: a Verilog-A file when a `.model`
/// card has one of its modules as its type, a co-simulated model when an
/// `A` instance names it.
pub fn used<'a>(netlist: &Netlist, d: &'a Declared) -> (Vec<&'a VaModel>, Vec<&'a CosimModel>) {
    let mut types = Vec::new();
    let mut instance_models = Vec::new();
    fn walk(items: &[Line], types: &mut Vec<String>, models: &mut Vec<String>) {
        for item in items {
            match item {
                Line::Directive { text } => {
                    let w = words(text);
                    if w.first().map(String::as_str) == Some(".model")
                        && let Some(t) = w.get(2)
                    {
                        types.push(t.clone());
                    }
                }
                Line::Element(e) if e.letter() == 'A' => {
                    if let Some(m) = e
                        .nodes
                        .iter()
                        .map(String::as_str)
                        .chain(e.rest.split_whitespace())
                        .last()
                    {
                        models.push(m.to_ascii_lowercase());
                    }
                }
                Line::Subckt(s) => walk(&s.body, types, models),
                _ => {}
            }
        }
    }
    walk(&netlist.items, &mut types, &mut instance_models);
    let va = d
        .veriloga
        .iter()
        .filter(|v| {
            v.modules
                .iter()
                .any(|m| types.contains(&m.name.to_ascii_lowercase()))
        })
        .collect();
    let cosim = d
        .cosim
        .iter()
        .filter(|c| instance_models.contains(&c.model.to_ascii_lowercase()))
        .collect();
    (va, cosim)
}

/// Where the compilers are.
#[derive(Debug, Clone, Default)]
pub struct Tools {
    pub openvaf: Option<PathBuf>,
    pub verilator: Option<PathBuf>,
    pub ngspice: Option<PathBuf>,
}

/// Libraries built in this process, by a hash of what went into them.
#[derive(Debug, Default)]
pub struct Builder {
    built: tokio::sync::Mutex<HashMap<String, Arc<Library>>>,
    versions: tokio::sync::Mutex<HashMap<PathBuf, String>>,
}

fn key(parts: &[&str]) -> String {
    let mut all = String::new();
    for p in parts {
        all.push_str(&p.len().to_string());
        all.push(':');
        all.push_str(p);
    }
    behavioral::sha256_hex(all.as_bytes())
}

fn build_error(e: BuildError) -> String {
    match e {
        BuildError::Failed { tool, log } => format!("{tool} could not build the model:\n{log}"),
        other => other.to_string(),
    }
}

impl Builder {
    async fn version(&self, exe: &Path) -> Result<String, String> {
        if let Some(v) = self.versions.lock().await.get(exe) {
            return Ok(v.clone());
        }
        let v = behavioral::version(exe)
            .await
            .ok_or_else(|| format!("{} did not report a version", exe.display()))?;
        self.versions
            .lock()
            .await
            .insert(exe.to_path_buf(), v.clone());
        Ok(v)
    }

    /// The libraries for these models, built now unless this process built
    /// the same sources with the same compiler before. Each new one is also
    /// written to `.aispice/build/` as a record.
    pub async fn build(
        &self,
        project: &Project,
        tools: &Tools,
        va: &[&VaModel],
        cosim: &[&CosimModel],
        jail: &aispice_sim::behavioral::Jail,
        cancel: &CancellationToken,
    ) -> Result<Vec<Arc<Library>>, String> {
        let mut out = Vec::new();
        if !va.is_empty() {
            let exe = tools
                .openvaf
                .clone()
                .or_else(|| aispice_sim::backend::which("openvaf"))
                .ok_or("Verilog-A models need OpenVAF (openvaf on PATH, or openvaf_path in the settings)")?;
            let version = self.version(&exe).await?;
            for m in va {
                let k = key(&["osdi", &version, &m.source.name, &m.source.text]);
                let mut built = self.built.lock().await;
                if let Some(lib) = built.get(&k) {
                    out.push(lib.clone());
                    continue;
                }
                let lib = Arc::new(
                    veriloga::build(jail, &exe, &version, &m.source, cancel)
                        .await
                        .map_err(build_error)?,
                );
                record(project, &lib);
                built.insert(k, lib.clone());
                out.push(lib);
            }
        }
        if !cosim.is_empty() {
            // Verilator and the C++ compiler come from aispice's trusted tool
            // path (or the configured Verilator), never a project's `PATH`.
            let exe = tools
                .verilator
                .clone()
                .or_else(|| aispice_sim::behavioral::tool(&["verilator"]))
                .ok_or("co-simulation needs Verilator (on aispice's tool path, or verilator_path in the settings)")?;
            let version = self.version(&exe).await?;
            let cxx = jail.cxx().ok_or(
                "co-simulation needs a C++ compiler (g++ or clang++) on aispice's tool path",
            )?;
            let ngspice = tools
                .ngspice
                .clone()
                .or_else(|| aispice_sim::backend::which("ngspice"))
                .ok_or("co-simulation needs ngspice")?;
            let shim = verilog::shim_dir(&ngspice).ok_or(
                "co-simulation needs ngspice's d_cosim sources (ngspice 42 or newer installs them under share/ngspice/scripts/src)",
            )?;
            for m in cosim {
                let mut parts: Vec<&str> = vec!["cosim", &version, &m.model, &m.top];
                for s in &m.sources {
                    parts.push(&s.name);
                    parts.push(&s.text);
                }
                let shim_text =
                    std::fs::read_to_string(shim.join("verilator_shim.cpp")).unwrap_or_default();
                parts.push(&shim_text);
                let k = key(&parts);
                let mut built = self.built.lock().await;
                if let Some(lib) = built.get(&k) {
                    out.push(lib.clone());
                    continue;
                }
                let lib = Arc::new(
                    verilog::build(
                        jail, &exe, &version, &cxx, &shim, &m.model, &m.top, &m.sources, cancel,
                    )
                    .await
                    .map_err(build_error)?,
                );
                record(project, &lib);
                built.insert(k, lib.clone());
                out.push(lib);
            }
        }
        Ok(out)
    }

    /// What this process has built, for the tool's listing.
    pub async fn built(&self) -> Vec<Arc<Library>> {
        self.built.lock().await.values().cloned().collect()
    }
}

/// Write a record copy of a library under `.aispice/build/<kind>/`, never
/// through a link: the file is written beside its name and renamed over
/// it. Nothing reads it back. A failure costs only the record.
fn record(project: &Project, lib: &Library) {
    let kind = match lib.kind {
        LibraryKind::Osdi => "veriloga",
        LibraryKind::Cosim => "cosim",
    };
    let Ok(dir) = project.state_subdir(&["build", kind]) else {
        return;
    };
    let name = lib.file_name();
    write_beside(&dir, &name, &lib.bytes);
    write_beside(
        &dir,
        &format!("{name}.sha256"),
        format!("{}  {name}\n", lib.sha256).as_bytes(),
    );
}

/// Write a file by creating a new one beside it and renaming it over the
/// name, so a link planted at the name is replaced, never followed.
fn write_beside(dir: &Path, name: &str, bytes: &[u8]) {
    let tmp = dir.join(format!(".{name}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, bytes));
    if written.is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(name));
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aispice_core::netlist::parse;

    #[test]
    fn a_deck_uses_the_models_it_names() {
        let d = Declared {
            veriloga: vec![VaModel {
                source: Source {
                    name: "m/comp.va".into(),
                    text: String::new(),
                },
                modules: vec![Module {
                    name: "comparator_va".into(),
                    ports: Vec::new(),
                }],
            }],
            cosim: vec![
                CosimModel {
                    model: "counter".into(),
                    top: "counter".into(),
                    sources: Vec::new(),
                    modules: vec!["counter".into()],
                },
                CosimModel {
                    model: "unused".into(),
                    top: "unused".into(),
                    sources: Vec::new(),
                    modules: vec!["unused".into()],
                },
            ],
        };
        let n = parse(
            "t\nN1 a b out cmp\n.model cmp COMPARATOR_VA(gain=1e4)\nA1 [ad] [q0 q1] null Counter\n.end\n",
        );
        let (va, cosim) = used(&n, &d);
        assert_eq!(va.len(), 1);
        assert_eq!(cosim.len(), 1);
        assert_eq!(cosim[0].model, "counter");
        let (va, cosim) = used(&parse("t\nR1 a 0 1k\n.end\n"), &d);
        assert!(va.is_empty() && cosim.is_empty());
    }

    #[test]
    fn declared_sources_stay_in_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("p");
        std::fs::create_dir_all(root.join("m")).unwrap();
        std::fs::write(dir.path().join("outside.va"), "module x; endmodule").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("outside.va"), root.join("m/link.va")).unwrap();
        std::fs::write(
            root.join("m/comp.va"),
            "`include \"disciplines.vams\"\nmodule comparator_va(a, y); inout a, y; electrical a, y; analog V(y) <+ V(a); endmodule\n",
        )
        .unwrap();
        std::fs::write(root.join(PROJECT_FILE), "veriloga = [\"m/comp.va\"]\n").unwrap();
        let p = Project::open(&root, Default::default()).unwrap();
        let d = declared(&p).unwrap();
        assert_eq!(d.veriloga[0].modules[0].name, "comparator_va");
        // A source the checks refuse is refused before anything compiles.
        std::fs::write(
            root.join("m/bad.va"),
            "module a; analog $system(\"id\"); endmodule",
        )
        .unwrap();
        std::fs::write(root.join(PROJECT_FILE), "veriloga = [\"m/bad.va\"]\n").unwrap();
        assert!(declared(&p).unwrap_err().contains("$system"));
        #[cfg(unix)]
        {
            std::fs::write(root.join(PROJECT_FILE), "veriloga = [\"m/link.va\"]\n").unwrap();
            assert!(declared(&p).is_err());
        }
    }
}

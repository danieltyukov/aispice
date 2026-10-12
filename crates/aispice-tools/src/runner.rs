//! Running a circuit: choosing a simulator, translating the netlist,
//! resolving models, applying the safety policy, and keeping the results.
//!
//! The policy check is not optional and not up to the caller: every deck
//! that reaches a simulator goes through [`Runner::run_netlist`], which
//! refuses anything the policy refuses.

use crate::pdk::{PROJECT_FILE, ProjectConfig, Registry};
use crate::project::{Project, ProjectError, now_ms};
use aispice_core::netlist::{self, Line, Netlist, Policy};
use aispice_sim::backend::ltspice::Ltspice;
use aispice_sim::backend::ngspice::Ngspice;
use aispice_sim::backend::spectre::{self, Spectre, SpectreConfig};
use aispice_sim::backend::xyce::Xyce;
use aispice_sim::backend::{Detection, SimId, SimJob, SimOutput, Simulator};
use aispice_sim::dialect::{self, Target};
use aispice_sim::library::{LibraryFiles, expand_lib_sections, inline_includes};
use aispice_sim::models;
use aispice_sim::pdk::{Pdk, PdkRun};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Project(#[from] ProjectError),
    #[error("{0}")]
    Netlist(String),
    #[error("refused for safety:\n{0}")]
    Policy(String),
    #[error(
        "no simulator is available: install ngspice (recommended), LTspice or Xyce, or configure Spectre"
    )]
    NoSimulator,
    #[error("{0} is not available on this machine")]
    Unavailable(String),
    #[error("{0}")]
    Pdk(String),
    #[error("{0}")]
    Sim(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    /// `auto`, `ngspice`, `xyce`, `ltspice` or `spectre`.
    pub simulator: String,
    pub ltspice_exe: Option<PathBuf>,
    pub spectre: Option<SpectreConfig>,
    pub allow_control_blocks: bool,
    pub timeout_secs: u64,
    /// Use only aispice's embedded models and the project's own files: no
    /// models or symbols from an installed LTspice library, and no automatic
    /// switch to LTspice when ngspice cannot run a deck. Results then do not
    /// depend on what is installed on the machine. A run on LTspice chosen
    /// by name still reads LTspice's own library, as LTspice always does.
    #[serde(default)]
    pub embedded_models_only: bool,
    /// The Xyce executable; `Xyce` on PATH when unset. PDK runs need a Xyce
    /// built with plugin support.
    #[serde(default)]
    pub xyce_exe: Option<PathBuf>,
    /// The ngspice executable; `ngspice` on PATH when unset.
    #[serde(default)]
    pub ngspice_exe: Option<PathBuf>,
    /// The user's registered PDKs (`pdks.toml`).
    #[serde(default)]
    pub pdks: Registry,
    /// Why `pdks.toml` could not be read, if it could not, so a project that
    /// names a PDK is told the real cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdks_error: Option<String>,
    /// Where gm/Id tables are cached; `<cache dir>/aispice/gmid` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gmid_cache_dir: Option<PathBuf>,
    /// KLayout and Magic for the layout tools; found on PATH when unset.
    #[serde(default)]
    pub layout_programs: aispice_layout::tools::Programs,
    /// OpenVAF for Verilog-A models; `openvaf` on PATH when unset.
    #[serde(default)]
    pub openvaf_exe: Option<PathBuf>,
    /// Verilator for co-simulated Verilog; `verilator` on PATH when unset.
    #[serde(default)]
    pub verilator_exe: Option<PathBuf>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            simulator: "auto".into(),
            ltspice_exe: None,
            spectre: None,
            allow_control_blocks: false,
            timeout_secs: 300,
            embedded_models_only: false,
            xyce_exe: None,
            ngspice_exe: None,
            pdks: Registry::default(),
            pdks_error: None,
            gmid_cache_dir: None,
            layout_programs: Default::default(),
            openvaf_exe: None,
            verilator_exe: None,
        }
    }
}

/// A finished run, kept so measurements, plots and specs can reuse it.
#[derive(Debug)]
pub struct StoredRun {
    pub id: String,
    pub circuit: String,
    pub simulator: SimId,
    pub output: SimOutput,
    /// The deck exactly as the simulator received it.
    pub deck: String,
    /// Translation and model notes worth telling the user.
    pub notes: Vec<String>,
    pub time: u64,
    /// A hash of the circuit's netlist when the run was made (before any
    /// run-only changes), to tell whether the circuit changed since. `None`
    /// for runs not made from the circuit file as it is.
    pub source: Option<u64>,
    /// The run-only changes the run was made with, so it can be repeated.
    pub mods: RunMods,
}

/// What to change for one run without touching the file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunMods {
    /// Replace every analysis directive with this one (`.ac dec 50 1 1Meg`).
    pub analysis: Option<String>,
    /// Directives added at the end (`.meas ...`, `.options plotwinsize=0`).
    pub extra: Vec<String>,
}

const KEEP_RUNS: usize = 32;

pub struct Runner {
    cfg: RwLock<RunnerConfig>,
    detected: OnceCell<Vec<(SimId, Detection)>>,
    runs: Mutex<VecDeque<Arc<StoredRun>>>,
    counter: std::sync::atomic::AtomicU64,
    /// Behavioural model libraries this process built.
    behavioral: crate::behavioral::Builder,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner").finish_non_exhaustive()
    }
}

impl Default for Runner {
    fn default() -> Self {
        Self::new(RunnerConfig::default())
    }
}

impl Runner {
    pub fn new(cfg: RunnerConfig) -> Self {
        Self {
            cfg: RwLock::new(cfg),
            detected: OnceCell::new(),
            runs: Mutex::new(VecDeque::new()),
            counter: Default::default(),
            behavioral: Default::default(),
        }
    }

    /// The project's declared behavioural models, read and checked but not
    /// built, with whether this netlist uses any. Nothing compiles before
    /// the deck has passed the netlist policy.
    fn behavioral_plan(
        &self,
        project: &Project,
        netlist: &Netlist,
    ) -> Result<(crate::behavioral::Declared, bool), RunError> {
        let cfg = ProjectConfig::load(project.root()).map_err(RunError::Pdk)?;
        if cfg.veriloga.is_empty() && cfg.cosim.is_empty() {
            return Ok((Default::default(), false));
        }
        let declared = crate::behavioral::declared(project).map_err(RunError::Netlist)?;
        let (va, cosim) = crate::behavioral::used(netlist, &declared);
        let uses = !(va.is_empty() && cosim.is_empty());
        if uses {
            aispice_sim::behavioral::check_run_size(netlist).map_err(RunError::Netlist)?;
        }
        Ok((declared, uses))
    }

    /// Build what a netlist uses of the declared models (or take what this
    /// process built before).
    async fn behavioral_build(
        &self,
        project: &Project,
        netlist: &Netlist,
        declared: &crate::behavioral::Declared,
        cancel: &CancellationToken,
    ) -> Result<Vec<Arc<aispice_sim::behavioral::Library>>, RunError> {
        let (va, cosim) = crate::behavioral::used(netlist, declared);
        let c = self.config();
        let tools = crate::behavioral::Tools {
            openvaf: c.openvaf_exe,
            verilator: c.verilator_exe,
            ngspice: c.ngspice_exe,
        };
        self.behavioral
            .build(project, &tools, &va, &cosim, cancel)
            .await
            .map_err(RunError::Netlist)
    }

    /// Build the project's declared behavioural models (or the one named,
    /// a Verilog-A file or a co-simulation model), whether or not a deck
    /// uses them yet.
    pub async fn build_behavioral(
        &self,
        project: &Project,
        only: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Vec<Arc<aispice_sim::behavioral::Library>>, String> {
        let declared = crate::behavioral::declared(project)?;
        let pick = |name: &str| only.is_none_or(|o| o.eq_ignore_ascii_case(name));
        let va: Vec<_> = declared
            .veriloga
            .iter()
            .filter(|v| pick(&v.source.name))
            .collect();
        let cosim: Vec<_> = declared.cosim.iter().filter(|c| pick(&c.model)).collect();
        if va.is_empty() && cosim.is_empty() {
            return Err(match only {
                Some(o) => {
                    format!("aispice.toml declares no Verilog-A file or co-simulation model {o}")
                }
                None => {
                    "aispice.toml declares no behavioural models (veriloga = [...] or [[cosim]])"
                        .into()
                }
            });
        }
        let c = self.config();
        let tools = crate::behavioral::Tools {
            openvaf: c.openvaf_exe,
            verilator: c.verilator_exe,
            ngspice: c.ngspice_exe,
        };
        self.behavioral
            .build(project, &tools, &va, &cosim, cancel)
            .await
    }

    /// The compilers and sources behavioural models need, for `doctor`.
    pub async fn behavioral_toolchain(&self) -> aispice_sim::behavioral::Toolchain {
        let c = self.config();
        aispice_sim::behavioral::Toolchain::detect(
            c.openvaf_exe.as_deref(),
            c.verilator_exe.as_deref(),
            c.ngspice_exe.as_deref(),
        )
        .await
    }

    /// The behavioural libraries this process has built.
    pub async fn behavioral_built(&self) -> Vec<Arc<aispice_sim::behavioral::Library>> {
        self.behavioral.built().await
    }

    pub fn config(&self) -> RunnerConfig {
        self.cfg.read().expect("runner config").clone()
    }

    pub fn set_config(&self, cfg: RunnerConfig) {
        *self.cfg.write().expect("runner config") = cfg;
    }

    fn ltspice(&self) -> Ltspice {
        Ltspice {
            exe: self.config().ltspice_exe,
            ..Default::default()
        }
    }

    /// LTspice's library folders (with sym, sub and cmp inside), if installed.
    pub fn ltspice_lib_dirs(&self) -> Vec<PathBuf> {
        self.ltspice()
            .lib_dirs()
            .into_iter()
            .filter(|d| d.is_dir())
            .collect()
    }

    /// The first LTspice symbol folder, for the symbol library. None with
    /// `embedded_models_only`, since a vendor symbol would need a vendor
    /// model that is then not used.
    pub fn ltspice_symbols(&self) -> Option<PathBuf> {
        if self.config().embedded_models_only {
            return None;
        }
        self.ltspice_lib_dirs()
            .into_iter()
            .map(|d| d.join("sym"))
            .find(|d| d.is_dir())
    }

    /// The LTspice library folders models are resolved from: none with
    /// `embedded_models_only`, so only aispice's embedded models and the
    /// project's own files are used.
    pub fn model_lib_dirs(&self) -> Vec<PathBuf> {
        if self.config().embedded_models_only {
            Vec::new()
        } else {
            self.ltspice_lib_dirs()
        }
    }

    fn simulator(&self, id: SimId) -> Box<dyn Simulator> {
        match id {
            SimId::Ltspice => Box::new(self.ltspice()),
            SimId::Xyce => Box::new(Xyce {
                exe: self.config().xyce_exe,
            }),
            SimId::Ngspice => Box::new(Ngspice {
                exe: self.config().ngspice_exe,
                ..Ngspice::default()
            }),
            SimId::Spectre => Box::new(Spectre {
                config: self.config().spectre,
            }),
        }
    }

    /// Which simulators are installed. Detected once per process.
    pub async fn detect(&self) -> Vec<(SimId, Detection)> {
        self.detected
            .get_or_init(|| async {
                let mut out = Vec::new();
                for id in SimId::ALL {
                    out.push((id, self.simulator(id).detect().await));
                }
                out
            })
            .await
            .clone()
    }

    async fn available(&self, id: SimId) -> bool {
        self.detect().await.iter().any(|(i, d)| *i == id && d.found)
    }

    /// Pick a simulator. Automatic choice prefers ngspice (fast, and LTspice
    /// compatible in its `ltpsa` mode), then LTspice, then Xyce. Spectre is
    /// used only when asked for by name.
    pub async fn choose(&self, requested: Option<&str>) -> Result<SimId, RunError> {
        self.choose_for(requested, None).await
    }

    /// As [`choose`](Self::choose), for a circuit in a project that uses
    /// `pdk`: only ngspice and Xyce can load a PDK's compiled models.
    pub async fn choose_for(
        &self,
        requested: Option<&str>,
        pdk: Option<&Pdk>,
    ) -> Result<SimId, RunError> {
        let wanted = requested
            .map(str::to_string)
            .unwrap_or_else(|| self.config().simulator);
        let mut id = match wanted.to_ascii_lowercase().as_str() {
            "ngspice" => Some(SimId::Ngspice),
            "xyce" => Some(SimId::Xyce),
            "ltspice" => Some(SimId::Ltspice),
            "spectre" => Some(SimId::Spectre),
            _ => None,
        };
        if let (Some(pdk), Some(sim @ (SimId::Ltspice | SimId::Spectre))) = (pdk, id) {
            // Asked for by name: say why not. Only the configured default:
            // choose as `auto` would, since that default is for board work.
            if requested.is_some() {
                let why = if sim == SimId::Ltspice {
                    "LTspice cannot load its compiled (OSDI) device models"
                } else {
                    "aispice has no Spectre models for it"
                };
                return Err(RunError::Pdk(format!(
                    "this project uses PDK `{}` and {why}; simulate with ngspice or Xyce",
                    pdk.name
                )));
            }
            id = None;
        }
        if let Some(id) = id {
            return if self.available(id).await {
                Ok(id)
            } else {
                Err(RunError::Unavailable(id.name().into()))
            };
        }
        let order: &[SimId] = if pdk.is_some() {
            &[SimId::Ngspice, SimId::Xyce]
        } else {
            &[SimId::Ngspice, SimId::Ltspice, SimId::Xyce]
        };
        for &id in order {
            if self.available(id).await {
                return Ok(id);
            }
        }
        Err(RunError::NoSimulator)
    }

    /// The PDK a project's `aispice.toml` names, opened from the registry;
    /// `None` for a board-level project.
    pub fn project_pdk(&self, project: &Project) -> Result<Option<Pdk>, RunError> {
        let cfg = ProjectConfig::load(project.root()).map_err(RunError::Pdk)?;
        let Some(name) = cfg.pdk else {
            return Ok(None);
        };
        let config = self.config();
        config.pdks.open(&name).map(Some).map_err(|e| {
            let cause = config
                .pdks_error
                .map(|r| format!(" ({r})"))
                .unwrap_or_default();
            RunError::Pdk(format!(
                "this project uses PDK `{name}` ({PROJECT_FILE}), but {e}{cause}"
            ))
        })
    }

    /// The netlist for a circuit file, plus the LTspice standard libraries it
    /// relies on. In a project that uses a PDK, the netlist ends with the
    /// PDK's model header (`.lib <corner library> <section>` for each device
    /// class it calls), so every run made from it, and every corner or
    /// mismatch variant of it, has the header to work with.
    pub fn netlist_for(
        &self,
        project: &Project,
        circuit: &str,
    ) -> Result<(Netlist, Vec<String>, Vec<String>), RunError> {
        let (mut netlist, std_libs, warnings) = if circuit.to_ascii_lowercase().ends_with(".asc") {
            let (sch, _) = project.load(circuit)?;
            let (built, _) = netlist::build(&sch, project.library(), &format!("* {circuit}"));
            (built.netlist, built.standard_libs, built.warnings)
        } else {
            let text = project.read_text(circuit)?;
            (netlist::parse(&text), Vec::new(), Vec::new())
        };
        if let Some(pdk) = self.project_pdk(project)? {
            pdk.add_header(&mut netlist);
        }
        Ok((netlist, std_libs, warnings))
    }

    /// A hash of what a circuit simulates as now: its netlist, before any
    /// run-only changes, and the standard libraries it relies on. Equal
    /// hashes mean an earlier run still describes the circuit.
    pub fn source_hash(&self, project: &Project, circuit: &str) -> Result<u64, RunError> {
        let (netlist, std_libs, _) = self.netlist_for(project, circuit)?;
        Ok(netlist_hash(&netlist, &std_libs))
    }

    pub async fn run_circuit(
        &self,
        project: &Project,
        circuit: &str,
        simulator: Option<&str>,
        mods: &RunMods,
        cancel: &CancellationToken,
    ) -> Result<Arc<StoredRun>, RunError> {
        let (mut netlist, std_libs, warnings) = self.netlist_for(project, circuit)?;
        let source = netlist_hash(&netlist, &std_libs);
        apply_mods(&mut netlist, mods);
        let run = self
            .run_netlist(project, circuit, &netlist, &std_libs, simulator, cancel)
            .await?;
        // Record what the run was made from, and put netlisting warnings
        // with the run's notes.
        let mut notes = warnings;
        notes.extend(run.notes.iter().cloned());
        let run = Arc::new(StoredRun {
            notes,
            source: Some(source),
            mods: mods.clone(),
            ..Arc::try_unwrap(run).unwrap_or_else(|a| clone_run(&a))
        });
        self.remember(run.clone());
        Ok(run)
    }

    /// Translate, resolve models, check the policy and run.
    pub async fn run_netlist(
        &self,
        project: &Project,
        circuit: &str,
        netlist: &Netlist,
        std_libs: &[String],
        simulator: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Arc<StoredRun>, RunError> {
        self.run_netlist_with(project, circuit, netlist, std_libs, simulator, cancel, true)
            .await
    }

    /// As [`run_netlist`](Self::run_netlist); with `remember` false the run is
    /// not kept and its folder is removed afterwards (sweeps, Monte Carlo and
    /// optimization make hundreds of runs).
    #[allow(clippy::too_many_arguments)]
    pub async fn run_netlist_with(
        &self,
        project: &Project,
        circuit: &str,
        netlist: &Netlist,
        std_libs: &[String],
        simulator: Option<&str>,
        cancel: &CancellationToken,
        remember: bool,
    ) -> Result<Arc<StoredRun>, RunError> {
        let pdk = self.project_pdk(project)?;
        if cfg!(windows) && pdk.is_some() {
            return Err(RunError::Pdk(
                "PDK projects run on Linux and macOS: the netlist policy refuses the absolute Windows paths a PDK's model header needs".into(),
            ));
        }
        if pdk.is_none()
            && let Some(msg) = pdk_devices_without_pdk(netlist)
        {
            return Err(RunError::Pdk(msg));
        }
        // The behavioural models' checks and run limits come first: a
        // refusal must not depend on which simulators are installed.
        let (declared, uses_behavioral) = self.behavioral_plan(project, netlist)?;
        let mut id = self.choose_for(simulator, pdk.as_ref()).await?;
        // Verilog-A and co-simulated Verilog run on ngspice only: the other
        // simulators cannot load what aispice builds for them.
        if uses_behavioral && id != SimId::Ngspice {
            let asked = simulator.is_some() || self.config().simulator != "auto";
            if asked || !self.available(SimId::Ngspice).await {
                return Err(RunError::Netlist(format!(
                    "this circuit uses the project's Verilog-A or co-simulated models, which run on ngspice only, not on {}",
                    id.name()
                )));
            }
            id = SimId::Ngspice;
        }
        let cosim = declared.cosim_names();
        let lib_dirs = self.model_lib_dirs();
        let base = Self::base_dir(project, circuit)?;
        let policy = self.policy_for(project, &base);
        // Model resolution reads the files the netlist includes, so they go
        // through the policy's resolver first.
        check_includes(netlist, &policy)?;
        // Model resolution keeps only includes it can find, and the simulator
        // runs in a private folder, so files beside the circuit are named by
        // their full path from here on. The policy checks those paths below.
        let netlist = &canonical_netlist_includes(netlist, &policy);
        let mut notes = Vec::new();
        let mut text = self.deck_for_with(
            id,
            netlist,
            std_libs,
            &lib_dirs,
            &mut notes,
            pdk.as_ref(),
            &cosim,
        );
        if let Some(t) = &text.1 {
            // ngspice or Xyce cannot run this deck; LTspice can, if present
            // and its library may be used. Not for a PDK, whose models
            // LTspice cannot load.
            if simulator.is_none()
                && id != SimId::Ltspice
                && pdk.is_none()
                && !self.config().embedded_models_only
                && self.available(SimId::Ltspice).await
            {
                notes.push(format!(
                    "{} cannot run this circuit ({t}); used LTspice instead",
                    id.name()
                ));
                id = SimId::Ltspice;
                notes.clear();
                text = self.deck_for(id, netlist, std_libs, &lib_dirs, &mut notes, None);
            } else {
                return Err(RunError::Netlist(format!(
                    "{} cannot run this circuit: {t}",
                    id.name()
                )));
            }
        }
        let deck = text.0;

        let trusted = policy.trusted_dirs.clone();
        let violations = netlist::check_policy(&deck, &policy);
        if !violations.is_empty() {
            let msg = violations
                .iter()
                .map(|v| format!("  {} ({})", v.reason, v.line.trim()))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(RunError::Policy(msg));
        }
        // ngspice in its LTspice mode, and LTspice, cannot select a section
        // of a library file, so aispice inlines it, reading only files the
        // policy approves; the check below sees the result. A PDK run puts
        // ngspice in its native mode, which selects sections itself.
        // Spectre runs on another machine, which has none of the project's
        // files, so the ones the deck includes go in the deck, with their
        // expressions in the quotes Spectre reads. Files outside the
        // project are only named; see below.
        let deck = if id == SimId::Spectre {
            let files = LibraryFiles::new(policy.clone(), project.root());
            let inlined = inline_includes(&deck, &files).map_err(RunError::Netlist)?;
            if !inlined.sent.is_empty() {
                let names: Vec<String> = inlined
                    .sent
                    .iter()
                    .map(|p| {
                        p.strip_prefix(project.root())
                            .unwrap_or(p)
                            .display()
                            .to_string()
                    })
                    .collect();
                notes.push(format!(
                    "sent to the Spectre server inside the deck: {}",
                    names.join(", ")
                ));
            }
            aispice_sim::dialect::spectre_expressions(&inlined.text)
        } else if matches!(id, SimId::Ngspice | SimId::Ltspice) && pdk.is_none() {
            let files = LibraryFiles::new(policy.clone(), project.root());
            expand_lib_sections(&deck, &files).map_err(RunError::Netlist)?
        } else {
            deck
        };
        let deck = netlist::canonical_includes(&deck, &policy);
        // Check again after the rewrites, so the deck the simulator receives
        // is exactly a deck the policy accepted.
        let violations = netlist::check_policy(&deck, &policy);
        if !violations.is_empty() {
            let msg = violations
                .iter()
                .map(|v| format!("  {} ({})", v.reason, v.line.trim()))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(RunError::Policy(msg));
        }
        // What is left to include on Spectre is outside the project: a PDK
        // or a model library, possibly licensed, which never leaves this
        // machine. An include of one names the server's own copy when the
        // user configured where that is, and is refused otherwise.
        let deck = if id == SimId::Spectre {
            let folders = self
                .config()
                .spectre
                .map(|c| c.remote_folders)
                .unwrap_or_default();
            let (deck, mapped) =
                spectre::remote_deck(&deck, &folders).map_err(RunError::Netlist)?;
            for (local, server) in mapped {
                notes.push(format!("{local} is read on the Spectre server as {server}"));
            }
            deck
        } else {
            deck
        };

        let run_id = format!(
            "{}-{}",
            now_ms(),
            self.counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let run_dir = project.run_dir(&run_id)?;
        let stem = Path::new(circuit)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "deck".into());
        // Compiled only now, for a deck the policy accepted.
        let libraries = if uses_behavioral {
            self.behavioral_build(project, netlist, &declared, cancel)
                .await?
        } else {
            Vec::new()
        };
        let mut job = SimJob::new(deck.clone(), run_dir);
        job.deck_name = stem;
        for lib in &libraries {
            notes.push(format!(
                "loaded {} built by aispice from the project's {} (SHA-256 {})",
                lib.file_name(),
                match lib.kind {
                    aispice_sim::behavioral::LibraryKind::Osdi => "Verilog-A",
                    aispice_sim::behavioral::LibraryKind::Cosim => "Verilog",
                },
                &lib.sha256[..16]
            ));
        }
        job.libraries = libraries;
        job.timeout = Duration::from_secs(self.config().timeout_secs);
        // A deck that includes PDK files by hand in a project that names no
        // PDK runs without the PDK's compiled models; say why if it fails.
        let mut hint = None;
        match &pdk {
            Some(pdk) => {
                job.pdk = Some(pdk_run(pdk, id)?);
                notes.push(pdk_note(pdk, id));
            }
            None => {
                if let Some(root) = trusted
                    .iter()
                    .find(|r| deck.contains(&*r.to_string_lossy()))
                {
                    let h = format!(
                        "the deck reads PDK files from {}, but the project names no PDK in {PROJECT_FILE}, so the PDK's compiled models are not loaded",
                        root.display()
                    );
                    notes.push(h.clone());
                    hint = Some(h);
                }
            }
        }
        let run_dir = job.run_dir.clone();
        let result = self
            .simulator(id)
            .run(&job, cancel)
            .await
            .map_err(|e| match &hint {
                Some(h) => RunError::Sim(format!("{e}\n({h})")),
                None => RunError::Sim(e.to_string()),
            });
        if !remember {
            let _ = std::fs::remove_dir_all(&run_dir);
        }
        let output = result?;
        let run = Arc::new(StoredRun {
            id: run_id,
            circuit: circuit.to_string(),
            simulator: id,
            output,
            deck,
            notes,
            time: now_ms(),
            source: None,
            mods: RunMods::default(),
        });
        if remember {
            self.remember(run.clone());
        }
        Ok(run)
    }

    /// The netlist policy for a deck whose relative paths start at `base`:
    /// the project, the model folders and the registered PDKs are the places
    /// it may read.
    pub fn policy_for(&self, project: &Project, base: &Path) -> Policy {
        let mut allowed = project.allowed_dirs();
        allowed.extend(self.model_lib_dirs());
        Policy {
            base_dir: base.to_path_buf(),
            allowed_dirs: allowed,
            allow_control_blocks: self.config().allow_control_blocks,
            // Every registered PDK folder that still checks out may be
            // included from, so a hand-written deck can name a PDK's
            // libraries itself.
            trusted_dirs: self.config().pdks.trusted_roots(),
        }
    }

    /// The folder a circuit's relative paths start at.
    pub fn base_dir(project: &Project, circuit: &str) -> Result<PathBuf, RunError> {
        let circuit_path = project.resolve(circuit)?;
        Ok(circuit_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| project.root().to_path_buf()))
    }

    /// The deck text for a simulator, or the reason it cannot run there.
    fn deck_for(
        &self,
        id: SimId,
        netlist: &Netlist,
        std_libs: &[String],
        lib_dirs: &[PathBuf],
        notes: &mut Vec<String>,
        pdk: Option<&Pdk>,
    ) -> (String, Option<String>) {
        self.deck_for_with(id, netlist, std_libs, lib_dirs, notes, pdk, &[])
    }

    /// As [`deck_for`](Self::deck_for), with the co-simulation models the
    /// project declares, whose XSPICE instances ngspice keeps.
    #[allow(clippy::too_many_arguments)]
    fn deck_for_with(
        &self,
        id: SimId,
        netlist: &Netlist,
        std_libs: &[String],
        lib_dirs: &[PathBuf],
        notes: &mut Vec<String>,
        pdk: Option<&Pdk>,
        cosim: &[String],
    ) -> (String, Option<String>) {
        let target = id.target();
        let source = if target == Target::Ltspice {
            single_analysis(netlist, "LTspice", notes)
        } else {
            let (netlist, defined) = match pdk {
                Some(p) => (p.retarget(netlist, target), p.defined_names()),
                None => (netlist.clone(), Vec::new()),
            };
            let resolved = models::resolve_with_defined(&netlist, std_libs, lib_dirs, &defined);
            for a in &resolved.added {
                notes.push(format!("model {} from {:?}", a.name, a.source));
            }
            // A model from the LTspice library would go to the Spectre
            // server inside the deck; the library is licensed with LTspice
            // and stays on this machine.
            if target == Target::Spectre {
                let from_ltspice: Vec<&str> = resolved
                    .added
                    .iter()
                    .filter(|a| matches!(a.source, models::ModelSource::Ltspice(_)))
                    .map(|a| a.name.as_str())
                    .collect();
                if !from_ltspice.is_empty() {
                    return (
                        String::new(),
                        Some(format!(
                            "{} would come from your LTspice library, which aispice does not send to a Spectre server; put the definition in a file in the project and include it, or use aispice's own models",
                            from_ltspice.join(", ")
                        )),
                    );
                }
            }
            if !resolved.missing.is_empty() {
                notes.push(format!(
                    "no definition found for: {}",
                    resolved.missing.join(", ")
                ));
            }
            notes.extend(resolved.notes);
            if target == Target::Xyce {
                single_analysis(&resolved.netlist, "Xyce", notes)
            } else {
                resolved.netlist
            }
        };
        let t = dialect::translate_with(&source, target, cosim);
        notes.extend(t.notes.iter().cloned());
        let blocked = (!t.unsupported.is_empty()).then(|| t.unsupported.join("; "));
        (t.text, blocked)
    }

    fn remember(&self, run: Arc<StoredRun>) {
        let mut runs = self.runs.lock().expect("runs lock");
        runs.retain(|r| r.id != run.id);
        runs.push_back(run);
        while runs.len() > KEEP_RUNS {
            runs.pop_front();
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<StoredRun>> {
        self.runs
            .lock()
            .expect("runs lock")
            .iter()
            .find(|r| r.id == id)
            .cloned()
    }

    /// The most recent run of a circuit.
    pub fn latest(&self, circuit: &str) -> Option<Arc<StoredRun>> {
        self.runs
            .lock()
            .expect("runs lock")
            .iter()
            .rev()
            .find(|r| r.circuit == circuit)
            .cloned()
    }
}

/// What a run loads from its PDK, each file checked to be inside the PDK's
/// folder. Xyce needs plugins; without them its run is refused here rather
/// than failing on the first transistor.
fn pdk_run(pdk: &Pdk, id: SimId) -> Result<PdkRun, RunError> {
    use aispice_core::netlist::policy::check_module;
    let roots = std::slice::from_ref(&pdk.root);
    let refused = |v: aispice_core::netlist::Violation| RunError::Policy(format!("  {}", v.reason));
    let osdi = pdk
        .osdi
        .iter()
        .map(|p| check_module(p, roots, &["osdi"]).map_err(refused))
        .collect::<Result<Vec<_>, _>>()?;
    let mut xyce_plugins = Vec::new();
    if id == SimId::Xyce {
        if !pdk.xyce_models {
            return Err(RunError::Pdk(format!(
                "PDK `{}` has no complete Xyce model folder (libs.tech/xyce/models); simulate with ngspice",
                pdk.name
            )));
        }
        if pdk.xyce_plugins.is_empty() {
            return Err(RunError::Pdk(format!(
                "Xyce needs PDK `{}`'s Verilog-A models built as Xyce plugins, and {} has none. Build them with IHP's libs.tech/verilog-a/adms-compile-va.sh (needs ADMS and a Xyce built with plugin support), or simulate with ngspice",
                pdk.name,
                pdk.root.join("libs.tech/xyce/plugins").display()
            )));
        }
        xyce_plugins = pdk
            .xyce_plugins
            .iter()
            .map(|p| check_module(p, roots, &["so", "dylib", "dll"]).map_err(refused))
            .collect::<Result<Vec<_>, _>>()?;
    }
    Ok(PdkRun { osdi, xyce_plugins })
}

fn pdk_note(pdk: &Pdk, id: SimId) -> String {
    let commit = pdk
        .commit
        .as_deref()
        .map(|c| format!(" at {}", &c[..c.len().min(12)]))
        .unwrap_or_default();
    let how = match id {
        SimId::Ngspice => "ngspice in its native mode, with the PDK's OSDI models",
        _ => "Xyce with the PDK's plugins",
    };
    format!("PDK {}{commit}: {how}", pdk.name)
}

/// When a netlist calls PDK devices it does not define and the project names
/// no PDK, the reason the run cannot work and how to fix it.
fn pdk_devices_without_pdk(netlist: &Netlist) -> Option<String> {
    fn walk(items: &[Line], calls: &mut Vec<(String, String)>, defined: &mut Vec<String>) {
        for item in items {
            match item {
                Line::Element(e) if e.letter() == 'X' => {
                    if let Some(w) = e.first_word() {
                        calls.push((e.name.clone(), w.to_string()));
                    }
                }
                Line::Subckt(s) => {
                    defined.push(s.name.to_ascii_lowercase());
                    walk(&s.body, calls, defined);
                }
                _ => {}
            }
        }
    }
    // A deck that includes libraries may define the subcircuit itself, or
    // read a PDK by hand; the run's own hint covers the latter.
    if netlist.directives().any(|d| {
        let kw = d
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        matches!(kw.as_str(), ".lib" | ".include" | ".inc")
    }) {
        return None;
    }
    let (mut calls, mut defined) = (Vec::new(), Vec::new());
    walk(&netlist.items, &mut calls, &mut defined);
    let (inst, device) = calls.into_iter().find_map(|(inst, name)| {
        let d = aispice_core::pdk::device(&name)?;
        (!defined.contains(&name.to_ascii_lowercase())).then_some((inst, d))
    })?;
    let kit = aispice_core::pdk::KITS
        .iter()
        .find(|k| k.device(device.name).is_some())
        .map_or("", |k| k.title);
    Some(format!(
        "{inst} is a {kit} device ({}), but this project names no PDK. Register the PDK once with `aispice pdk add <name> <path>`, then put `pdk = \"<name>\"` in {PROJECT_FILE} at the project root (or use the pdk tool's `use` action)",
        device.name
    ))
}

fn clone_run(r: &StoredRun) -> StoredRun {
    StoredRun {
        id: r.id.clone(),
        circuit: r.circuit.clone(),
        simulator: r.simulator,
        output: r.output.clone(),
        deck: r.deck.clone(),
        notes: r.notes.clone(),
        time: r.time,
        source: r.source,
        mods: r.mods.clone(),
    }
}

/// See [`Runner::source_hash`].
fn netlist_hash(netlist: &Netlist, std_libs: &[String]) -> u64 {
    use std::hash::{DefaultHasher, Hasher};
    let mut h = DefaultHasher::new();
    h.write(netlist::write(netlist).as_bytes());
    for l in std_libs {
        h.write(l.as_bytes());
        h.write_u8(0);
    }
    h.finish()
}

/// Apply run-only changes to a netlist.
pub fn apply_mods(netlist: &mut Netlist, mods: &RunMods) {
    use aispice_core::netlist::Line;
    if let Some(analysis) = &mods.analysis {
        netlist.items.retain(
            |l| !matches!(l, Line::Directive { text } if netlist::spice::is_analysis(text)),
        );
        netlist.items.push(Line::Directive {
            text: analysis.trim().to_string(),
        });
    }
    for d in &mods.extra {
        netlist.items.push(Line::Directive {
            text: d.trim().to_string(),
        });
    }
}

/// Include lines naming files beside the circuit, rewritten to name them by
/// their full path, as [`netlist::canonical_includes`] does with the folder
/// as the only allowed one. Kept for callers outside the runner, which
/// itself rewrites with the project's whole policy.
pub fn absolutize_includes(deck: &str, base: &Path) -> String {
    let policy = Policy {
        base_dir: base.to_path_buf(),
        ..Policy::default()
    };
    netlist::canonical_includes(deck, &policy)
}

/// [`netlist::canonical_includes`] on the top-level directives of a netlist.
fn canonical_netlist_includes(netlist: &Netlist, policy: &Policy) -> Netlist {
    let mut out = netlist.clone();
    for item in &mut out.items {
        if let aispice_core::netlist::Line::Directive { text } = item {
            let fixed = netlist::canonical_includes(text, policy);
            *text = fixed.trim_end_matches('\n').to_string();
        }
    }
    out
}

/// Refuse a netlist whose own include lines the policy refuses, before
/// anything reads the files they name.
fn check_includes(netlist: &Netlist, policy: &Policy) -> Result<(), RunError> {
    for text in netlist.directives() {
        for line in netlist::logical_lines(text, false) {
            let reason = match netlist::parse_include(&line) {
                Some(Err(reason)) => reason,
                Some(Ok(inc)) => {
                    match netlist::resolve_include(&inc.file, &policy.base_dir, policy) {
                        Err(reason) => reason,
                        Ok(_) => continue,
                    }
                }
                None => continue,
            };
            return Err(RunError::Policy(format!("  {reason} ({})", line.trim())));
        }
    }
    Ok(())
}

/// LTspice and Xyce run one analysis per simulation: LTspice fails at a
/// second one ("Previous analysis already found") and Xyce runs only one of
/// them. Keep the first and say which were left out; ngspice runs them all.
fn single_analysis(netlist: &Netlist, simulator: &str, notes: &mut Vec<String>) -> Netlist {
    let mut out = netlist.clone();
    let mut seen = false;
    let mut skipped = Vec::new();
    out.items.retain(|item| match item {
        aispice_core::netlist::Line::Directive { text }
            if aispice_core::netlist::spice::is_analysis(text) =>
        {
            if seen {
                skipped.push(text.trim().to_string());
                false
            } else {
                seen = true;
                true
            }
        }
        _ => true,
    });
    if !skipped.is_empty() {
        notes.push(format!(
            "{simulator} runs one analysis per simulation; left out {}",
            skipped.join(", ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Was a bug found on a live Spectre run: the policy refused a line the
    /// Spectre translation itself adds. Every simulator's translated deck of
    /// an ordinary circuit must pass the policy unchanged.
    #[test]
    fn ltspice_gets_one_analysis() {
        let netlist =
            aispice_core::netlist::parse("* x\nR1 a 0 1k\n.tran 1m\n.ac dec 10 1 1k\n.op\n");
        let mut notes = Vec::new();
        let one = single_analysis(&netlist, "LTspice", &mut notes);
        let text = aispice_core::netlist::write(&one);
        assert!(text.contains(".tran 1m") && !text.contains(".ac") && !text.contains(".op"));
        assert!(notes[0].contains(".ac dec 10 1 1k, .op"), "{notes:?}");
    }

    #[test]
    fn translated_decks_pass_the_policy() {
        let netlist = aispice_core::netlist::parse(
            "* rc\nV1 in 0 SINE(0 1 1k) AC 1\nR1 in out 1k\nC1 out 0 100n\n.ac dec 20 10 100k\n.tran 2m\n",
        );
        let runner = Runner::default();
        let policy = Policy {
            base_dir: std::env::temp_dir(),
            allowed_dirs: Vec::new(),
            allow_control_blocks: false,
            trusted_dirs: Vec::new(),
        };
        for id in SimId::ALL {
            let mut notes = Vec::new();
            let (deck, unsupported) = runner.deck_for(id, &netlist, &[], &[], &mut notes, None);
            assert!(unsupported.is_none(), "{id:?}: {unsupported:?}");
            let violations = netlist::check_policy(&deck, &policy);
            assert!(violations.is_empty(), "{id:?}: {violations:?}\n{deck}");
        }
    }

    #[test]
    fn includes_beside_the_circuit_become_absolute() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.lib"), "* x\n").unwrap();
        let deck = "t\n.lib m.lib\n.lib opamp.sub\n.include \"m.lib\"\n.include \"m.lib\" extra\n";
        let out = absolutize_includes(deck, dir.path());
        let real = std::fs::canonicalize(dir.path().join("m.lib")).unwrap();
        assert!(
            out.contains(&format!(".lib \"{}\"\n", real.display())),
            "{out}"
        );
        assert!(out.contains(".lib opamp.sub"));
        assert!(
            out.contains(&format!(".include \"{}\"\n", real.display())),
            "{out}"
        );
        // Not in the one form every simulator reads alike: left for the
        // policy to refuse.
        assert!(out.contains(".include \"m.lib\" extra\n"), "{out}");
    }

    /// An include that climbs out of the circuit's folder is left as
    /// written, never turned into the full path of a file outside it.
    #[test]
    fn includes_that_climb_out_are_not_made_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("project");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(dir.path().join("secret.txt"), "x\n").unwrap();
        let deck = ".include ../secret.txt\nR1 a 0 1k\n.inc \"../secret.txt\"\n";
        assert_eq!(absolutize_includes(deck, &base), deck);
    }

    /// A circuit file whose first line includes a file outside the project
    /// is refused before it runs: ngspice reads an `.include` on the title
    /// line, and would quote the file's lines in the run's errors and log.
    #[tokio::test]
    async fn a_first_line_include_cannot_read_files_outside() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, "TOKEN=sk-live-0123456789\nsecond secret line\n").unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let body = "V1 a 0 1\nR1 a 0 1k\n.op\n.end\n";
        std::fs::write(
            root.join("abs.cir"),
            format!(".include {}\n{body}", secret.display()),
        )
        .unwrap();
        std::fs::write(
            root.join("rel.cir"),
            format!(".include ../secret.txt\n{body}"),
        )
        .unwrap();
        let project = Project::open(&root, Default::default()).unwrap();
        let runner = Runner::default();
        for circuit in ["abs.cir", "rel.cir"] {
            let result = runner
                .run_circuit(
                    &project,
                    circuit,
                    Some("ngspice"),
                    &RunMods::default(),
                    &CancellationToken::new(),
                )
                .await;
            match result {
                Err(RunError::Policy(msg)) => {
                    assert!(!msg.contains("sk-live") && !msg.contains("second"), "{msg}")
                }
                // Nothing to run it on here; the policy test covers the check.
                Err(RunError::Unavailable(_)) => return,
                Err(e) => panic!("{circuit}: not refused by the policy: {e}"),
                Ok(run) => panic!("{circuit}: ran:\n{}", run.output.log),
            }
        }
    }

    /// Was machine-dependent: a model the netlist names was pulled from an
    /// installed LTspice library, with no way to turn that off. With
    /// embedded_models_only the deck gets aispice's own models only.
    #[test]
    fn embedded_models_only_ignores_the_ltspice_library() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("LTspice.exe");
        std::fs::write(&exe, "not really").unwrap();
        std::fs::create_dir_all(dir.path().join("lib/sub")).unwrap();
        std::fs::create_dir_all(dir.path().join("lib/sym")).unwrap();
        std::fs::write(
            dir.path().join("lib/sub/fakeamp.sub"),
            ".subckt fakeamp a b c\nR1 a c 1k\n.ends fakeamp\n",
        )
        .unwrap();
        let n = netlist::parse("t\nX1 a b c fakeamp\nX2 a b c opamp\n.op\n");
        let lib = dir.path().join("lib");

        let usual = Runner::new(RunnerConfig {
            ltspice_exe: Some(exe.clone()),
            ..RunnerConfig::default()
        });
        assert!(usual.model_lib_dirs().contains(&lib));
        let mut notes = Vec::new();
        let (deck, _) = usual.deck_for(
            SimId::Ngspice,
            &n,
            &[],
            &usual.model_lib_dirs(),
            &mut notes,
            None,
        );
        assert!(
            deck.to_ascii_lowercase().contains(".subckt fakeamp"),
            "{deck}"
        );

        let strict = Runner::new(RunnerConfig {
            ltspice_exe: Some(exe),
            embedded_models_only: true,
            ..RunnerConfig::default()
        });
        assert!(strict.model_lib_dirs().is_empty());
        assert!(strict.ltspice_symbols().is_none());
        let mut notes = Vec::new();
        let (deck, _) = strict.deck_for(
            SimId::Ngspice,
            &n,
            &[],
            &strict.model_lib_dirs(),
            &mut notes,
            None,
        );
        let lower = deck.to_ascii_lowercase();
        assert!(!lower.contains(".subckt fakeamp"), "{deck}");
        assert!(
            lower.contains(".subckt opamp"),
            "embedded models still come: {deck}"
        );
        assert!(notes.iter().any(|n| n.contains("fakeamp")), "{notes:?}");
    }

    /// A runner with a fake IHP PDK registered as `ihp`, and a project
    /// folder whose `aispice.toml` says `project_toml`.
    fn pdk_setup(project_toml: Option<&str>) -> (tempfile::TempDir, Runner, Project, Pdk) {
        let dir = tempfile::tempdir().unwrap();
        aispice_sim::pdk::fake_ihp(dir.path());
        let mut pdks = Registry::default();
        let pdk = pdks.add("ihp", &dir.path().join("IHP-Open-PDK")).unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        if let Some(t) = project_toml {
            std::fs::write(root.join(PROJECT_FILE), t).unwrap();
        }
        std::fs::write(
            root.join("inv.cir"),
            "* inverter\nVDD vdd 0 1.2\nVIN in 0 0\nXM1 out in 0 0 sg13_lv_nmos w=1u l=0.13u ng=1 m=1\nXM2 out in vdd vdd sg13_lv_pmos w=2u l=0.13u ng=1 m=1\n.dc VIN 0 1.2 10m\n.end\n",
        )
        .unwrap();
        let runner = Runner::new(RunnerConfig {
            pdks,
            ..RunnerConfig::default()
        });
        let project = Project::open(&root, Default::default()).unwrap();
        (dir, runner, project, pdk)
    }

    #[test]
    fn pdk_projects_get_the_model_header() {
        let (_d, runner, project, pdk) = pdk_setup(Some("pdk = \"ihp\"\n"));
        let (n, _, _) = runner.netlist_for(&project, "inv.cir").unwrap();
        let text = netlist::write(&n);
        let corner = pdk.models_dir(Target::Ngspice).join("cornerMOSlv.lib");
        let line = aispice_sim::pdk::lib_line(&corner, "mos_tt");
        assert!(text.contains(&format!("{line}\n")), "{text}");
        assert_eq!(
            text.lines().filter(|l| l.starts_with(".lib")).count(),
            1,
            "{text}"
        );
        // Board-level projects are untouched.
        let (_d2, runner2, project2, _) = pdk_setup(None);
        let (n2, _, _) = runner2.netlist_for(&project2, "inv.cir").unwrap();
        assert!(!netlist::write(&n2).contains(".lib"));
    }

    #[test]
    fn pdk_decks_pass_the_policy_and_retarget_for_xyce() {
        let (_d, runner, project, pdk) = pdk_setup(Some("pdk = \"ihp\"\n"));
        let (n, _, _) = runner.netlist_for(&project, "inv.cir").unwrap();
        for id in [SimId::Ngspice, SimId::Xyce] {
            let mut notes = Vec::new();
            let (deck, unsupported) = runner.deck_for(id, &n, &[], &[], &mut notes, Some(&pdk));
            assert!(unsupported.is_none(), "{unsupported:?}");
            assert!(
                !notes.iter().any(|n| n.contains("no definition")),
                "PDK devices are not looked up: {notes:?}"
            );
            let corner = pdk.models_dir(id.target()).join("cornerMOSlv.lib");
            let line = aispice_sim::pdk::lib_line(&corner, "mos_tt");
            assert!(deck.contains(&line), "{deck}");
            // The policy refuses every absolute Windows path, so PDK runs
            // are not offered there (see run_netlist_with).
            if cfg!(windows) {
                continue;
            }
            let policy = Policy {
                base_dir: project.root().to_path_buf(),
                trusted_dirs: runner.config().pdks.trusted_roots(),
                ..Default::default()
            };
            assert_eq!(netlist::check_policy(&deck, &policy), vec![], "{deck}");
            // Without the PDK registered, its files are outside the project.
            let strict = Policy {
                base_dir: project.root().to_path_buf(),
                ..Default::default()
            };
            assert!(!netlist::check_policy(&deck, &strict).is_empty());
        }
    }

    #[tokio::test]
    async fn pdk_projects_refuse_ltspice_and_spectre() {
        let (_d, runner, _project, pdk) = pdk_setup(Some("pdk = \"ihp\"\n"));
        for sim in ["ltspice", "LTspice", "spectre"] {
            let err = runner.choose_for(Some(sim), Some(&pdk)).await.unwrap_err();
            assert!(matches!(err, RunError::Pdk(_)), "{sim}: {err}");
            assert!(err.to_string().contains("ngspice or Xyce"), "{err}");
        }
        // A configured default of LTspice is for board work: a PDK project
        // chooses as `auto` would instead of failing.
        let mut cfg = runner.config();
        cfg.simulator = "ltspice".into();
        runner.set_config(cfg);
        match runner.choose_for(None, Some(&pdk)).await {
            Ok(id) => assert!(matches!(id, SimId::Ngspice | SimId::Xyce), "{id:?}"),
            Err(e) => assert!(matches!(e, RunError::NoSimulator), "{e}"),
        }
    }

    #[tokio::test]
    async fn pdk_devices_need_the_project_to_name_a_pdk() {
        let (_d, runner, project, _) = pdk_setup(None);
        let (n, std_libs, _) = runner.netlist_for(&project, "inv.cir").unwrap();
        let err = runner
            .run_netlist(
                &project,
                "inv.cir",
                &n,
                &std_libs,
                None,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("XM1") && msg.contains("aispice.toml"), "{msg}");
        // A deck that defines the subcircuit itself is not a PDK deck.
        let own = netlist::parse(
            "t\nXM1 a b 0 0 sg13_lv_nmos\n.subckt sg13_lv_nmos d g s b\nR1 d s 1k\n.ends\n.op\n",
        );
        assert!(pdk_devices_without_pdk(&own).is_none());
        let lib = netlist::parse("t\nXR1 a b 0 rhigh\n.lib mine.lib\n.op\n");
        assert!(pdk_devices_without_pdk(&lib).is_none());
        let bare = netlist::parse("t\nXR1 a b 0 rhigh\n.op\n");
        assert!(pdk_devices_without_pdk(&bare).is_some());
    }

    #[test]
    fn unknown_or_unregistered_pdk_names_explain_themselves() {
        let (_d, runner, project, _) = pdk_setup(Some("pdk = \"sky130\"\n"));
        let err = runner
            .netlist_for(&project, "inv.cir")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("sky130") && err.contains("registered: ihp"),
            "{err}"
        );
        let (_d2, _, project2, _) = pdk_setup(Some("pdk = \"ihp\"\n"));
        let empty = Runner::new(RunnerConfig {
            pdks_error: Some("pdks.toml is not valid".into()),
            ..RunnerConfig::default()
        });
        let err = empty
            .netlist_for(&project2, "inv.cir")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("none are registered") && err.contains("pdks.toml is not valid"),
            "{err}"
        );
    }

    /// A PDK file that is a link to something outside the PDK is refused
    /// when it would be used: a compiled model before the run starts, a
    /// model library by the netlist policy.
    #[cfg(unix)]
    #[test]
    fn pdk_files_linked_from_outside_are_refused() {
        let (dir, runner, project, pdk) = pdk_setup(Some("pdk = \"ihp\"\n"));
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        let osdi = pdk.root.join("libs.tech/ngspice/osdi/psp103.osdi");
        std::fs::write(outside.join("evil.osdi"), b"").unwrap();
        std::fs::remove_file(&osdi).unwrap();
        std::os::unix::fs::symlink(outside.join("evil.osdi"), &osdi).unwrap();
        let linked = Pdk::open("ihp", &pdk.root).unwrap();
        let err = pdk_run(&linked, SimId::Ngspice).unwrap_err().to_string();
        assert!(err.contains("not inside a registered PDK folder"), "{err}");

        let corner = pdk.root.join("libs.tech/ngspice/models/cornerMOSlv.lib");
        std::fs::copy(&corner, outside.join("cornerMOSlv.lib")).unwrap();
        std::fs::remove_file(&corner).unwrap();
        std::os::unix::fs::symlink(outside.join("cornerMOSlv.lib"), &corner).unwrap();
        let (n, _, _) = runner.netlist_for(&project, "inv.cir").unwrap();
        let mut notes = Vec::new();
        let (deck, _) = runner.deck_for(SimId::Ngspice, &n, &[], &[], &mut notes, Some(&pdk));
        let policy = Policy {
            base_dir: project.root().to_path_buf(),
            trusted_dirs: runner.config().pdks.trusted_roots(),
            ..Default::default()
        };
        let v = netlist::check_policy(&deck, &policy);
        assert!(
            v.iter().any(|v| v.reason.contains("outside the project")),
            "{v:?}"
        );
    }

    #[test]
    fn pdk_runs_load_checked_modules_and_xyce_needs_plugins() {
        let (_d, _runner, _project, pdk) = pdk_setup(None);
        let run = pdk_run(&pdk, SimId::Ngspice).unwrap();
        assert_eq!(run.osdi.len(), 4);
        assert!(run.osdi.iter().all(|p| p.starts_with(&pdk.root)));
        assert!(run.xyce_plugins.is_empty());
        let err = pdk_run(&pdk, SimId::Xyce).unwrap_err().to_string();
        assert!(err.contains("adms-compile-va.sh"), "{err}");
        let plugins = pdk.root.join("libs.tech/xyce/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join("libXyce_Plugin_PSP103_VA.so"), b"").unwrap();
        let pdk = Pdk::open("ihp", &pdk.root).unwrap();
        let run = pdk_run(&pdk, SimId::Xyce).unwrap();
        assert_eq!(run.xyce_plugins.len(), 1);
        assert!(pdk_note(&pdk, SimId::Ngspice).contains("native mode"));
    }

    #[test]
    fn analysis_override_replaces_existing() {
        let mut n = netlist::parse("t\nR1 a 0 1\n.tran 1m\n.op\n");
        apply_mods(
            &mut n,
            &RunMods {
                analysis: Some(".ac dec 10 1 1k".into()),
                extra: vec![".meas ac x max V(a)".into()],
            },
        );
        let d: Vec<&str> = n.directives().collect();
        assert_eq!(d, vec![".ac dec 10 1 1k", ".meas ac x max V(a)"]);
    }
}

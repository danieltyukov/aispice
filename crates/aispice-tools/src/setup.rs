//! One way to start a session, shared by the CLI, the MCP server and the
//! desktop app, so all three see the same simulators, libraries and settings.

use crate::project::{Project, ProjectError, ProjectOptions};
use crate::runner::{Runner, RunnerConfig};
use crate::workspace::Workspace;
use aispice_agent::Config;
use aispice_sim::backend::spectre::SpectreConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Simulator settings from the user's config file.
pub fn runner_config(cfg: &Config) -> RunnerConfig {
    let spectre = cfg.spectre.as_ref().map(|s| {
        let run = "cd {dir} && spectre {deck} -format nutascii -raw {raw} =log {log}";
        let command = match s
            .setup_command
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            Some(setup) => format!("{setup} && {run}"),
            None => run.to_string(),
        };
        SpectreConfig {
            host: s.ssh_host.clone(),
            remote_dir: s.remote_dir.clone(),
            command,
            ssh_options: vec![
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "ConnectTimeout=15".into(),
            ],
            ssh: None,
            scp: None,
        }
    });
    let (pdks, pdks_error) = match crate::pdk::Registry::load() {
        Ok(r) => (r, None),
        Err(e) => (crate::pdk::Registry::default(), Some(e.to_string())),
    };
    RunnerConfig {
        simulator: cfg.simulator.clone(),
        ltspice_exe: cfg.ltspice_path.clone(),
        xyce_exe: cfg.xyce_path.clone(),
        spectre,
        embedded_models_only: cfg.embedded_models_only,
        pdks,
        pdks_error,
        layout_programs: aispice_layout::tools::Programs {
            klayout: cfg.klayout_path.clone(),
            magic: cfg.magic_path.clone(),
        },
        ..RunnerConfig::default()
    }
}

/// Open a project with LTspice's symbol library when it is installed.
pub fn open_workspace(
    root: &Path,
    cfg: &Config,
    extra_symbol_dirs: &[PathBuf],
) -> Result<Arc<Workspace>, ProjectError> {
    let ws = Workspace::new();
    ws.runner.set_config(runner_config(cfg));
    let options = ProjectOptions {
        extra_symbol_dirs: extra_symbol_dirs.to_vec(),
        ltspice_lib: ws.runner.ltspice_symbols(),
    };
    ws.set_project(Project::open(root, options)?);
    Ok(Arc::new(ws))
}

/// Replace the open project in an existing workspace (the desktop app).
pub fn switch_project(
    ws: &Workspace,
    root: &Path,
    extra_symbol_dirs: &[PathBuf],
) -> Result<(), ProjectError> {
    let options = ProjectOptions {
        extra_symbol_dirs: extra_symbol_dirs.to_vec(),
        ltspice_lib: ws.runner.ltspice_symbols(),
    };
    ws.set_project(Project::open(root, options)?);
    Ok(())
}

/// A runner on its own, for commands that need no project.
pub fn runner(cfg: &Config) -> Runner {
    Runner::new(runner_config(cfg))
}

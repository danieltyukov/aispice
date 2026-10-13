//! The mixed-signal examples through the tools, with every sandbox chosen
//! as Landlock at ABI 4, the oldest aispice accepts: a real ABI-4 ruleset,
//! with seccomp's signal program in place of the scopes ABI 6 has. The
//! override (`Sandbox::use_landlock_abi_for_tests`, feature `testing`) is
//! process-wide, which is why these tests have a binary of their own.
//! Skips where the kernel has no Landlock ABI 4 or a tool is missing; the
//! StrongARM demo also needs `AISPICE_PDK_IHP`.

#![cfg(target_os = "linux")]

use aispice_agent::{Tool, ToolContext};
use aispice_sim::sandbox::Sandbox;
use aispice_tools::pdk::Registry;
use aispice_tools::tools::Simulate;
use aispice_tools::{Project, RunnerConfig, Workspace};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// Landlock at ABI 4 for every run in this process, and the tools the
/// examples need, or why the test skips.
async fn ready() -> bool {
    let ngspice = aispice_sim::backend::which("ngspice");
    let tools = ngspice.is_some()
        && aispice_sim::backend::which("openvaf").is_some()
        && aispice_sim::backend::which("verilator").is_some()
        && aispice_sim::behavioral::verilog::cxx().is_some()
        && ngspice
            .as_deref()
            .and_then(aispice_sim::behavioral::verilog::shim_dir)
            .is_some();
    if !tools {
        eprintln!(
            "skipped: needs ngspice with its d_cosim sources, OpenVAF, Verilator and a C++ compiler"
        );
        return false;
    }
    if let Err(e) = Sandbox::use_landlock_abi_for_tests(4) {
        eprintln!("skipped: {e}");
        return false;
    }
    let sandbox = aispice_sim::backend::native_code::ready(&CancellationToken::new()).await;
    match sandbox {
        Ok(s) => {
            assert_eq!(s.landlock_abi(), Some(4), "{}", s.describe());
            true
        }
        Err(e) => {
            assert!(
                std::env::var_os("AISPICE_REQUIRE_SANDBOXES").is_none(),
                "a sandbox is required here: {e}"
            );
            eprintln!("skipped: {e}");
            false
        }
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), &target).unwrap();
        }
    }
}

/// An example copied into a fresh project, with a runner on ngspice.
fn project(example: &str, cfg: RunnerConfig) -> (tempfile::TempDir, Arc<Workspace>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(example);
    copy_dir(&examples().join(example), &root);
    let ws = Workspace::new();
    ws.runner.set_config(RunnerConfig {
        embedded_models_only: true,
        ..cfg
    });
    ws.set_project(Project::open(&root, Default::default()).unwrap());
    (dir, Arc::new(ws))
}

/// The counter's four bits, read from the dac_bridge outputs at `at`.
async fn count(ws: &Arc<Workspace>, circuit: &str, at: &str) -> u32 {
    let measures: Vec<String> = (0..4)
        .map(|b| format!("q{b} = value_at(V(vq{b}), {at})"))
        .collect();
    let out = Simulate { ws: ws.clone() }
        .call(
            &ToolContext::default(),
            json!({"circuit": circuit, "simulator": "ngspice", "measurements": measures}),
        )
        .await;
    let text = out.text_content();
    assert!(!out.is_error, "{text}");
    let data = out.data.unwrap()["run"].clone();
    let values: Vec<(String, f64)> = data["measurements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| Some((m["name"].as_str()?.to_string(), m["value"].as_f64()?)))
        .collect();
    let high = values.iter().map(|(_, v)| *v).fold(1.0f64, f64::max) / 2.0;
    (0..4).fold(0, |n, b| {
        let (_, v) = values
            .iter()
            .find(|(name, _)| *name == format!("q{b}"))
            .unwrap_or_else(|| panic!("no q{b}: {text}"));
        if *v > high { n | 1 << b } else { n }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_verilog_a_comparator_drives_a_cosimulated_counter_at_landlock_4() {
    if !ready().await {
        return;
    }
    let (_dir, ws) = project("mixed_signal", RunnerConfig::default());
    assert_eq!(count(&ws, "ramp_counter.cir", "9.9u").await, 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_strongarm_template_drives_a_cosimulated_counter_at_landlock_4() {
    let Some(pdk) = std::env::var_os("AISPICE_PDK_IHP") else {
        eprintln!("skipped: AISPICE_PDK_IHP is not set");
        return;
    };
    if !ready().await {
        return;
    }
    let mut pdks = Registry::default();
    pdks.add("ihp-sg13g2", Path::new(&pdk)).unwrap();
    let (_dir, ws) = project(
        "mixed_signal_ihp",
        RunnerConfig {
            pdks,
            ..RunnerConfig::default()
        },
    );
    assert_eq!(count(&ws, "strongarm_counter.cir", "16.4n").await, 5);
}

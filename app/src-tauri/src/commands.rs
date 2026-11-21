//! The Tauri commands behind `app/src/ipc/api.ts`.
//!
//! Each command is a thin call into aispice-tools. Paths from the UI are
//! project-relative and resolved (and confined) by the project.

use crate::settings::{self, UiSettings};
use crate::state::AppState;
use aispice_agent::KeyStore;
use aispice_core::render::{Highlight, RenderOptions, render_svg};
use aispice_core::summary::summarize;
use aispice_tools::RunMods;
use aispice_tools::tools::{run_view, specs_file};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

type Res<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub const EVENT: &str = "aispice://event";

#[tauri::command]
pub async fn doctor(state: State<'_, AppState>) -> Res<Value> {
    let sims = state.ws.runner.detect().await;
    let providers = provider_status(state.keys.clone()).await;
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "platform": std::env::consts::OS,
        "simulators": sims.iter().map(|(id, d)| json!({"id": id, "found": d.found, "version": d.version, "path": d.path, "notes": d.notes})).collect::<Vec<_>>(),
        "providers": providers,
        "ltspice_lib": state.ws.runner.ltspice_lib_dirs().first().map(|p| p.display().to_string()),
    }))
}

/// Which providers have a key, and where it comes from. The keychain is
/// reached over D-Bus or a system API and can stall (a locked keyring, no
/// Secret Service), so the lookup runs off the async runtime with a time
/// limit, and a stalled keychain reads as "no key" rather than a hung window.
///
/// The providers are the key store's own list, the one `aispice doctor` and
/// `aispice keys list` go through. A list kept here left out `custom`, so a
/// key saved for the custom endpoint was stored and still shown as "No key".
async fn provider_status(keys: Arc<KeyStore>) -> Vec<Value> {
    use aispice_agent::keys::KNOWN_PROVIDERS;
    let lookup = tokio::task::spawn_blocking(move || {
        KNOWN_PROVIDERS
            .iter()
            .map(|id| {
                let source = keys.source(id).ok().flatten().map(|s| {
                    if matches!(s, aispice_agent::keys::KeySource::Env { .. }) {
                        "env"
                    } else if s.to_string().to_ascii_lowercase().contains("keychain") {
                        "keychain"
                    } else {
                        "file"
                    }
                });
                json!({"id": id, "configured": *id == "ollama" || source.is_some(), "source": source})
            })
            .collect::<Vec<_>>()
    });
    match tokio::time::timeout(std::time::Duration::from_secs(5), lookup).await {
        Ok(Ok(list)) => list,
        _ => KNOWN_PROVIDERS
            .iter()
            .map(|id| json!({"id": id, "configured": *id == "ollama", "source": null}))
            .collect(),
    }
}

fn circuits_json(state: &AppState) -> Res<Vec<Value>> {
    let p = state.ws.project().map_err(err)?;
    Ok(p.circuits()
        .into_iter()
        .map(|c| {
            let last = state.ws.runner.latest(&c.path).map(|r| r.id.clone());
            json!({"path": c.path, "name": c.name, "modified": c.modified, "last_run": last})
        })
        .collect())
}

#[tauri::command]
pub async fn open_project(root: String, app: AppHandle, state: State<'_, AppState>) -> Res<Value> {
    let root_path = std::fs::canonicalize(&root).map_err(|e| format!("{root}: {e}"))?;
    aispice_tools::setup::switch_project(&state.ws, &root_path, &[]).map_err(err)?;
    {
        let mut prefs = state.prefs.write().map_err(err)?;
        prefs.remember_project(&root_path.display().to_string());
        prefs.save();
    }
    // Watch for edits made outside aispice (LTspice saving the file).
    let handle = app.clone();
    let base = root_path.clone();
    let watcher = crate::watch::watch(&root_path, move |path| {
        let rel = path
            .strip_prefix(&base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let _ = handle.emit(
            EVENT,
            json!({"type": "circuit_changed", "path": rel, "external": true}),
        );
    })
    .ok();
    *state.watcher.lock().map_err(err)? = watcher;
    let p = state.ws.project().map_err(err)?;
    Ok(
        json!({"root": p.root().display().to_string(), "name": p.name(), "circuits": circuits_json(&state)?}),
    )
}

#[tauri::command]
pub fn recent_projects(state: State<'_, AppState>) -> Res<Vec<String>> {
    Ok(state
        .prefs
        .read()
        .map_err(err)?
        .recent_projects
        .iter()
        .filter(|p| Path::new(p).is_dir())
        .cloned()
        .collect())
}

#[tauri::command]
pub fn list_circuits(state: State<'_, AppState>) -> Res<Vec<Value>> {
    circuits_json(&state)
}

#[derive(Debug, Deserialize)]
pub struct HighlightArg {
    inst: String,
    kind: aispice_core::render::HighlightKind,
}

pub fn circuit_view(state: &AppState, path: &str, highlights: Vec<Highlight>) -> Res<Value> {
    let p = state.ws.project().map_err(err)?;
    let (can_undo, can_redo) = p.can_undo_redo(path);
    if !path.to_ascii_lowercase().ends_with(".asc") {
        // A netlist: no drawing, the text is the circuit.
        let text = p.read_text(path).map_err(err)?;
        return Ok(json!({
            "path": path,
            "summary": {"components": [], "nets": [], "directives": [], "comments": [], "analysis": null, "findings": []},
            "svg": "",
            "asc": "",
            "netlist": text,
            "can_undo": can_undo,
            "can_redo": can_redo,
        }));
    }
    let (sch, _) = p.load(path).map_err(err)?;
    let summary = summarize(&sch, p.library());
    let opts = RenderOptions {
        standalone: false,
        highlight: highlights,
        ..RenderOptions::default()
    };
    let svg = render_svg(&sch, p.library(), &opts).svg;
    let (built, _) = aispice_core::netlist::build(&sch, p.library(), &format!("* {path}"));
    Ok(json!({
        "path": path,
        "summary": summary,
        "svg": svg,
        "asc": aispice_core::schematic::write(&sch),
        "netlist": aispice_core::netlist::write(&built.netlist),
        "can_undo": can_undo,
        "can_redo": can_redo,
    }))
}

#[tauri::command]
pub fn read_circuit(
    path: String,
    highlights: Vec<HighlightArg>,
    state: State<'_, AppState>,
) -> Res<Value> {
    circuit_view(
        &state,
        &path,
        highlights
            .into_iter()
            .map(|h| Highlight {
                inst: h.inst,
                kind: h.kind,
            })
            .collect(),
    )
}

#[tauri::command]
pub fn new_circuit(name: String, state: State<'_, AppState>) -> Res<Value> {
    let p = state.ws.project().map_err(err)?;
    let name = name.trim();
    if name.is_empty() || name.contains(['/', '\\']) {
        return Err("give the circuit a plain file name".into());
    }
    let file = if name.to_ascii_lowercase().ends_with(".asc") {
        name.to_string()
    } else {
        format!("{name}.asc")
    };
    p.create(&file, &aispice_core::Schematic::new())
        .map_err(err)?;
    Ok(
        json!({"path": file, "name": file, "modified": aispice_tools::project::now_ms(), "last_run": null}),
    )
}

/// Draw a SPICE netlist the user picked as a new schematic in the project,
/// named after the netlist file. The source can be anywhere; only netlist
/// extensions are read, with the same size and policy limits the agent's
/// create_schematic uses.
#[tauri::command]
pub async fn import_netlist(path: String, state: State<'_, AppState>) -> Res<Value> {
    let p = state.ws.project().map_err(err)?;
    tokio::task::spawn_blocking(move || -> Res<Value> {
        let source = Path::new(&path);
        let ext = source
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !["cir", "net", "sp", "spi", "cki", "txt"].contains(&ext.as_str()) {
            return Err("choose a SPICE netlist (.cir, .net, .sp)".into());
        }
        let meta = std::fs::metadata(source).map_err(|e| format!("{path}: {e}"))?;
        if !meta.is_file() || meta.len() > aispice_tools::tools::MAX_NETLIST_BYTES as u64 {
            return Err(format!(
                "{path} is not a netlist file of at most {} KB",
                aispice_tools::tools::MAX_NETLIST_BYTES / 1024
            ));
        }
        let bytes = std::fs::read(source).map_err(|e| format!("{path}: {e}"))?;
        let (text, _) = aispice_core::encoding::decode(&bytes);
        let stem: String = source
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' })
            .collect();
        let stem = if stem.is_empty() { "imported".to_string() } else { stem };
        let file = (0..100)
            .map(|i| if i == 0 { format!("{stem}.asc") } else { format!("{stem}-{i}.asc") })
            .find(|f| p.resolve(f).is_ok_and(|path| !path.exists()))
            .ok_or("no free file name for the imported schematic")?;
        let (sch, _notes, _score) =
            aispice_tools::tools::layout_netlist(&text, &file, p.library())?;
        p.create(&file, &sch).map_err(err)?;
        Ok(json!({"path": file, "name": file, "modified": aispice_tools::project::now_ms(), "last_run": null}))
    })
    .await
    .map_err(err)?
}

#[tauri::command]
pub async fn simulate(
    path: String,
    simulator: Option<String>,
    state: State<'_, AppState>,
) -> Res<Value> {
    let p = state.ws.project().map_err(err)?;
    let sim = simulator.filter(|s| s != "auto");
    let run = state
        .ws
        .runner
        .run_circuit(
            &p,
            &path,
            sim.as_deref(),
            &RunMods::default(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .map_err(err)?;
    Ok(run_view(&run, &[]))
}

#[tauri::command]
pub fn waveform(
    run_id: String,
    dataset: usize,
    signals: Vec<String>,
    max_points: Option<usize>,
    state: State<'_, AppState>,
) -> Res<Value> {
    let run = state
        .ws
        .runner
        .get(&run_id)
        .ok_or("that run is no longer kept; simulate again")?;
    let ds = run
        .output
        .datasets
        .get(dataset)
        .ok_or("no such dataset in the run")?;
    crate::wave::wave_data(ds, &signals, max_points.unwrap_or(4000).clamp(100, 20000))
}

#[tauri::command]
pub async fn check_specs(path: String, state: State<'_, AppState>) -> Res<Value> {
    let p = state.ws.project().map_err(err)?;
    let Ok(text) = p.read_text(&specs_file(&path)) else {
        return Ok(Value::Null);
    };
    let specs = aispice_sim::spec::parse_specs(&text).map_err(err)?;
    let run = state
        .ws
        .runner
        .run_circuit(
            &p,
            &path,
            None,
            &RunMods::default(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .map_err(err)?;
    Ok(aispice_tools::tools::ui_spec_report(
        &aispice_sim::spec::evaluate(&specs, &run.output.datasets),
    ))
}

#[tauri::command]
pub fn history(path: String, state: State<'_, AppState>) -> Res<Vec<Value>> {
    let p = state.ws.project().map_err(err)?;
    let (snaps, current) = p.history(&path).map_err(err)?;
    Ok(snaps
        .iter()
        .enumerate()
        .rev()
        .map(|(i, s)| json!({"id": s.id, "time": s.time, "summary": s.summary, "current": i == current}))
        .collect())
}

fn after_history_move(app: &AppHandle, state: &AppState, path: &str) -> Res<Value> {
    let _ = app.emit(
        EVENT,
        json!({"type": "circuit_changed", "path": path, "external": false}),
    );
    circuit_view(state, path, vec![])
}

#[tauri::command]
pub fn undo(path: String, app: AppHandle, state: State<'_, AppState>) -> Res<Value> {
    state.ws.project().map_err(err)?.undo(&path).map_err(err)?;
    after_history_move(&app, &state, &path)
}

#[tauri::command]
pub fn redo(path: String, app: AppHandle, state: State<'_, AppState>) -> Res<Value> {
    state.ws.project().map_err(err)?.redo(&path).map_err(err)?;
    after_history_move(&app, &state, &path)
}

#[tauri::command]
pub fn restore(
    path: String,
    snapshot_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Res<Value> {
    state
        .ws
        .project()
        .map_err(err)?
        .restore(&path, &snapshot_id)
        .map_err(err)?;
    after_history_move(&app, &state, &path)
}

#[tauri::command]
pub fn open_in_ltspice(path: String, state: State<'_, AppState>) -> Res<()> {
    let p = state.ws.project().map_err(err)?;
    let abs = p.resolve(&path).map_err(err)?;
    let configured = state.config.read().map_err(err)?.ltspice_path.clone();
    crate::ltspice::open(&abs, configured.as_deref())
}

#[tauri::command]
pub fn settings(state: State<'_, AppState>) -> Res<UiSettings> {
    let cfg = state.config.read().map_err(err)?;
    let prefs = state.prefs.read().map_err(err)?;
    Ok(settings::to_ui(&cfg, &prefs))
}

#[tauri::command]
pub fn save_settings(settings: UiSettings, state: State<'_, AppState>) -> Res<()> {
    let mut cfg = state.config.write().map_err(err)?;
    let mut prefs = state.prefs.write().map_err(err)?;
    settings::apply_ui(&mut cfg, &mut prefs, &settings);
    cfg.save().map_err(err)?;
    prefs.save();
    state
        .ws
        .runner
        .set_config(aispice_tools::setup::runner_config(&cfg));
    Ok(())
}

#[tauri::command]
pub async fn key_status(state: State<'_, AppState>) -> Res<Value> {
    Ok(Value::Array(provider_status(state.keys.clone()).await))
}

#[tauri::command]
pub async fn set_key(provider: String, key: String, state: State<'_, AppState>) -> Res<()> {
    let keys = state.keys.clone();
    tokio::task::spawn_blocking(move || keys.set(&provider, &key).map(|_| ()))
        .await
        .map_err(err)?
        .map_err(err)
}

#[tauri::command]
pub async fn remove_key(provider: String, state: State<'_, AppState>) -> Res<()> {
    let keys = state.keys.clone();
    tokio::task::spawn_blocking(move || keys.remove(&provider).map(|_| ()))
        .await
        .map_err(err)?
        .map_err(err)
}

#[tauri::command]
pub async fn models(provider: String, state: State<'_, AppState>) -> Res<Value> {
    let cfg = state.config.read().map_err(err)?.clone();
    let keys = state.keys.clone();
    let p = tokio::task::spawn_blocking(move || {
        aispice_agent::providers::build_provider(&provider, &cfg, &keys)
    })
    .await
    .map_err(err)?
    .map_err(err)?;
    let list = p.list_models().await.map_err(err)?;
    serde_json::to_value(list).map_err(err)
}

#[tauri::command]
pub fn sessions(circuit: Option<String>, state: State<'_, AppState>) -> Res<Value> {
    let p = state.ws.project().map_err(err)?;
    serde_json::to_value(crate::sessions::list(&p, circuit.as_deref())?).map_err(err)
}

#[tauri::command]
pub fn load_session(id: String, state: State<'_, AppState>) -> Res<Value> {
    let p = state.ws.project().map_err(err)?;
    let s = crate::sessions::load(&p, &id)?;
    Ok(json!({"meta": s.meta, "messages": s.messages}))
}

#[tauri::command]
pub fn new_session(circuit: Option<String>, state: State<'_, AppState>) -> Res<Value> {
    let p = state.ws.project().map_err(err)?;
    let meta = crate::sessions::new_meta(circuit);
    crate::sessions::save(
        &p,
        &crate::sessions::StoredSession {
            meta: meta.clone(),
            messages: vec![],
            history: vec![],
        },
    )?;
    serde_json::to_value(meta).map_err(err)
}

#[tauri::command]
pub fn delete_session(id: String, state: State<'_, AppState>) -> Res<()> {
    let p = state.ws.project().map_err(err)?;
    crate::sessions::delete(&p, &id)
}

#[tauri::command]
pub fn cancel(session_id: String, state: State<'_, AppState>) -> Res<()> {
    if let Some(t) = state.running.lock().map_err(err)?.get(&session_id) {
        t.cancel();
    }
    Ok(())
}

#[tauri::command]
pub fn approve(request_id: String, approved: bool, state: State<'_, AppState>) -> Res<()> {
    if let Some(tx) = state.approvals.lock().map_err(err)?.remove(&request_id) {
        let _ = tx.send(approved);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aispice_agent::keys::KNOWN_PROVIDERS;

    fn entry<'a>(list: &'a [Value], id: &str) -> &'a Value {
        list.iter()
            .find(|p| p["id"] == id)
            .unwrap_or_else(|| panic!("no status for {id}: {list:?}"))
    }

    /// Was issue #82: the status list was written out here and left `custom`
    /// out, so Settings offered "Set key" again after every save.
    #[tokio::test]
    async fn a_key_saved_for_the_custom_endpoint_shows_as_configured() {
        let keys = Arc::new(KeyStore::memory());
        let before = provider_status(keys.clone()).await;
        assert_eq!(entry(&before, "custom")["configured"], false);
        assert!(entry(&before, "custom")["source"].is_null());

        keys.set("custom", "sk-test").unwrap();
        let after = provider_status(keys).await;
        assert_eq!(entry(&after, "custom")["configured"], true);
        assert!(entry(&after, "custom")["source"].is_string());
    }

    /// The interface asks for each provider by id and reads a missing one as
    /// "No key", so the list has to cover every provider a key can be saved
    /// for.
    #[tokio::test]
    async fn the_status_covers_every_provider_the_key_store_knows() {
        let list = provider_status(Arc::new(KeyStore::memory())).await;
        let ids: Vec<&str> = list.iter().filter_map(|p| p["id"].as_str()).collect();
        assert_eq!(ids, KNOWN_PROVIDERS);
        // Ollama needs no key, and is the only one configured without one.
        for p in &list {
            assert_eq!(p["configured"], p["id"] == "ollama", "{p}");
        }
    }
}

//! `aispice chat`, `doctor`, `keys`, `models` and `docs`.

use aispice_agent::providers::{build_provider, default_model};
use aispice_agent::{Agent, AgentEvent, Config, ContentBlock, KeyStore, ToolContext};
use aispice_tools::prompt::{PromptContext, system_prompt};
use aispice_tools::registry;
use aispice_tools::setup::open_workspace;
use anyhow::{Context, Result, anyhow};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

/// Text from the model or a tool, made safe to print on a terminal: control
/// characters (escape sequences could clear lines or forge a prompt) and
/// bidirectional overrides (which reorder what is shown) become visible
/// placeholders. Newlines and tabs are kept.
pub fn terminal_safe(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\t' => c,
            c if c.is_control() => '\u{FFFD}',
            '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{061C}' => '\u{FFFD}',
            c => c,
        })
        .collect()
}

/// Ask-before-apply in a terminal: show the change and read y or n. Only an
/// interactive terminal can approve; with piped or redirected input every
/// change is declined, so queued input can never answer for the user.
struct TerminalApprover;

#[async_trait::async_trait]
impl aispice_tools::Approver for TerminalApprover {
    async fn approve(&self, circuit: &str, summary: &str, diff: &str) -> bool {
        let (circuit, summary, diff) = (circuit.to_string(), summary.to_string(), diff.to_string());
        tokio::task::spawn_blocking(move || {
            use std::io::IsTerminal;
            if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
                eprintln!(
                    "\n(ask-before-apply needs an interactive terminal; declined the change to {})",
                    terminal_safe(&circuit)
                );
                return false;
            }
            eprintln!(
                "\n--- proposed change to {}: {}",
                terminal_safe(&circuit),
                terminal_safe(&summary)
            );
            for line in diff.lines().take(60) {
                eprintln!("    {}", terminal_safe(line));
            }
            eprint!("Apply? [y/N] ");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer).is_ok()
                && matches!(answer.trim(), "y" | "Y" | "yes")
        })
        .await
        .unwrap_or(false)
    }
}

pub struct ChatArgs {
    pub prompt: Option<String>,
    pub project: Option<PathBuf>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub symbols: Vec<PathBuf>,
}

pub fn chat(args: ChatArgs) -> Result<ExitCode> {
    let cfg = Config::load().unwrap_or_default();
    let keys = KeyStore::system();
    let _ = keys.migrate_legacy();
    let root = match args.project {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let ws = open_workspace(&root, &cfg, &args.symbols).context("opening the project")?;
    if cfg.edit_mode == aispice_agent::config::EditMode::Ask {
        ws.set_hooks(aispice_tools::Hooks {
            approver: Some(Arc::new(TerminalApprover)),
            after_save: None,
        });
    }
    let provider_id = args.provider.unwrap_or_else(|| cfg.provider.clone());
    let provider = build_provider(&provider_id, &cfg, &keys).map_err(|e| anyhow!("{e}"))?;
    let model = args
        .model
        .or_else(|| cfg.model_for(&provider_id))
        .or_else(|| default_model(&provider_id).map(str::to_string))
        .ok_or_else(|| {
            anyhow!("no model chosen for {provider_id}; pass --model or set one in config.toml")
        })?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let sims: Vec<String> = rt
        .block_on(ws.runner.detect())
        .into_iter()
        .filter(|(_, d)| d.found)
        .map(|(i, _)| i.name().to_string())
        .collect();
    let prompt = system_prompt(&PromptContext {
        project: Some(root.display().to_string()),
        circuit: None,
        simulators: sims,
        ltspice_library: ws.runner.ltspice_symbols().is_some(),
        pdk: aispice_tools::pdk::ProjectConfig::load(&root)
            .ok()
            .and_then(|c| c.pdk),
    });
    let agent = Agent::new(provider, Arc::new(registry(ws.clone())), &model)
        .with_system(prompt)
        .with_max_steps(cfg.agent.max_steps)
        .with_thinking(cfg.thinking_config());
    let mut history = Vec::new();
    let turn = |text: String, history: &mut Vec<_>| -> Result<()> {
        let mut on_event = |e: AgentEvent| match e {
            AgentEvent::TextDelta { text } => {
                print!("{}", terminal_safe(&text));
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ToolStart { name, .. } => eprintln!("\n  [{}]", terminal_safe(&name)),
            AgentEvent::ToolEnd {
                name,
                output,
                duration_ms,
                ..
            } => {
                let first = output
                    .text_content()
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                eprintln!(
                    "  [{}] {} ({duration_ms} ms) {}",
                    terminal_safe(&name),
                    if output.is_error { "failed" } else { "ok" },
                    terminal_safe(&first)
                );
            }
            AgentEvent::Error { message } => eprintln!("\nerror: {}", terminal_safe(&message)),
            _ => {}
        };
        rt.block_on(agent.run(
            history,
            vec![ContentBlock::text(text)],
            ToolContext::default(),
            &mut on_event,
        ))
        .map_err(|e| anyhow!("{e}"))?;
        println!();
        Ok(())
    };
    if let Some(p) = args.prompt {
        turn(p, &mut history)?;
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!(
        "aispice chat in {} with {provider_id}/{model}. Ctrl-D to quit.",
        root.display()
    );
    let stdin = std::io::stdin();
    loop {
        eprint!("> ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "exit" | "quit") {
            break;
        }
        turn(line.to_string(), &mut history)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// One line per registered PDK, or why the registry could not be read.
fn pdk_lines(runner: &aispice_tools::Runner) -> Vec<String> {
    let cfg = runner.config();
    let mut out: Vec<String> = cfg.pdks_error.into_iter().collect();
    for name in cfg.pdks.pdks.keys() {
        out.push(match cfg.pdks.open(name) {
            Ok(p) => aispice_tools::tools::summary_line(&p),
            Err(e) => format!("{name}: unusable: {e}"),
        });
    }
    out
}

pub fn doctor(json_mode: bool) -> Result<ExitCode> {
    let cfg = Config::load().unwrap_or_default();
    let runner = aispice_tools::setup::runner(&cfg);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let sims = rt.block_on(runner.detect());
    let keys = KeyStore::system();
    let configured = keys.list_configured();
    if json_mode {
        let v = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "simulators": sims.iter().map(|(i, d)| serde_json::json!({"id": i, "found": d.found, "version": d.version, "path": d.path, "notes": d.notes})).collect::<Vec<_>>(),
            "ltspice_library": runner.ltspice_lib_dirs(),
            "pdks": pdk_lines(&runner),
            "layout_tools": layout_lines(&runner),
            "layout_sandbox": rt.block_on(aispice_tools::layout::sandbox_description()),
            "behavioral_models": rt.block_on(behavioral_lines(&runner)),
            "providers": configured.iter().map(|k| serde_json::json!({"provider": k.provider, "source": k.source.to_string()})).collect::<Vec<_>>(),
            "config": Config::default_path(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!("aispice {}", env!("CARGO_PKG_VERSION"));
    println!("\nSimulators:");
    for (id, d) in &sims {
        println!(
            "  {:8} {}{}",
            id.name(),
            if d.found { "found" } else { "not found" },
            d.version
                .as_ref()
                .map(|v| format!(" ({v})"))
                .unwrap_or_default()
        );
        for n in &d.notes {
            println!("           {n}");
        }
    }
    if !sims.iter().any(|(i, d)| d.found && i.name() != "Spectre") {
        println!(
            "  Install ngspice to simulate: apt install ngspice, brew install ngspice, or https://ngspice.sourceforge.io"
        );
    }
    let libs = runner.ltspice_lib_dirs();
    println!(
        "\nLTspice library: {}",
        if libs.is_empty() {
            "not found (built-in symbols and generic models are used)".to_string()
        } else {
            libs.iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    println!("\nPDKs:");
    let pdks = pdk_lines(&runner);
    if pdks.is_empty() {
        println!("  none registered (for IC design: aispice pdk add <name> <path>)");
    }
    for l in pdks {
        println!("  {l}");
    }
    println!("\nLayout tools:");
    for l in layout_lines(&runner) {
        println!("  {l}");
    }
    println!(
        "  {:8} {}",
        "sandbox",
        rt.block_on(aispice_tools::layout::sandbox_description())
    );
    println!("\nBehavioural models (Verilog-A, Verilog co-simulation on ngspice):");
    for l in rt.block_on(behavioral_lines(&runner)) {
        println!("  {l}");
    }
    println!("\nModel providers with a key:");
    if configured.is_empty() {
        println!(
            "  none. Add one with `aispice keys set anthropic` (or openai, google, openrouter); Ollama needs no key."
        );
    }
    for k in &configured {
        println!("  {:10} from {}", k.provider, k.source);
    }
    println!(
        "\nConfig file: {}",
        Config::default_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "unavailable".into())
    );
    Ok(ExitCode::SUCCESS)
}

/// OpenVAF (Verilog-A), Verilator, a C++ compiler, make and ngspice's
/// d_cosim sources (co-simulation), and the C compiler and sandbox a run
/// that loads them needs: where each is, or how to get it.
async fn behavioral_lines(runner: &aispice_tools::Runner) -> Vec<String> {
    let t = runner.behavioral_toolchain().await;
    let tool = |name: &str, found: &Option<(std::path::PathBuf, String)>, hint: &str| match found {
        Some((p, v)) => format!("{name:9} {} ({v})", p.display()),
        None => format!("{name:9} not found ({hint})"),
    };
    let path = |name: &str, found: &Option<std::path::PathBuf>, hint: &str| match found {
        Some(p) => format!("{name:9} {}", p.display()),
        None => format!("{name:9} not found ({hint})"),
    };
    vec![
        tool(
            "OpenVAF",
            &t.openvaf,
            "23.5 from openvaf.semimod.de, or set openvaf_path",
        ),
        tool(
            "Verilator",
            &t.verilator,
            "5.0 or newer, or set verilator_path",
        ),
        path(
            "C",
            &t.cc,
            "cc, gcc or clang in /usr/local/bin, /usr/bin or /bin, for the library a sandboxed ngspice preloads",
        ),
        path("C++", &t.cxx, "g++ or clang++"),
        path("make", &t.make, "GNU make"),
        path(
            "d_cosim",
            &t.cosim_shim,
            "ngspice 42 or newer installs its sources under share/ngspice/scripts/src; set ngspice_path",
        ),
        match t.cosim_missing() {
            None => "co-simulation can be built here".into(),
            Some(m) => format!("co-simulation needs {m}"),
        },
        match runner.behavioral_sandbox().await {
            Ok(sandbox) => format!("compiled models run in a sandboxed ngspice: {sandbox}"),
            Err(e) => e,
        },
    ]
}

/// KLayout (DRC, LVS, generators, pictures) and Magic (extraction): where
/// each is, or how to get it.
fn layout_lines(runner: &aispice_tools::Runner) -> Vec<String> {
    let programs = runner.config().layout_programs;
    let line = |name: &str, found: Option<std::path::PathBuf>, hint: &str| match found {
        Some(p) => format!("{name:8} {}", p.display()),
        None => format!("{name:8} not found ({hint})"),
    };
    vec![
        line(
            "KLayout",
            programs.klayout().ok(),
            "0.30 or newer from klayout.de, or set klayout_path",
        ),
        line(
            "Magic",
            programs.magic().ok(),
            "8.3.573 or newer from opencircuitdesign.com, or set magic_path",
        ),
    ]
}

pub fn keys_set(provider: &str) -> Result<ExitCode> {
    eprintln!(
        "Paste the {provider} API key and press Enter (it is stored in the OS keychain, never printed):"
    );
    let mut key = String::new();
    std::io::stdin().lock().read_line(&mut key)?;
    let source = KeyStore::system()
        .set(provider, key.trim())
        .map_err(|e| anyhow!("{e}"))?;
    eprintln!("Saved to {source}.");
    Ok(ExitCode::SUCCESS)
}

pub fn keys_remove(provider: &str) -> Result<ExitCode> {
    let removed = KeyStore::system()
        .remove(provider)
        .map_err(|e| anyhow!("{e}"))?;
    eprintln!(
        "{}",
        if removed {
            format!("Removed the {provider} key.")
        } else {
            format!("No stored {provider} key.")
        }
    );
    Ok(ExitCode::SUCCESS)
}

pub fn keys_list() -> Result<ExitCode> {
    for k in KeyStore::system().list_configured() {
        println!("{:10} {}", k.provider, k.source);
    }
    Ok(ExitCode::SUCCESS)
}

pub fn models(provider: Option<String>) -> Result<ExitCode> {
    let cfg = Config::load().unwrap_or_default();
    let id = provider.unwrap_or_else(|| cfg.provider.clone());
    let p = build_provider(&id, &cfg, &KeyStore::system()).map_err(|e| anyhow!("{e}"))?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let list = rt.block_on(p.list_models()).map_err(|e| anyhow!("{e}"))?;
    for m in list {
        println!(
            "{}{}",
            m.id,
            if m.display_name != m.id {
                format!("  ({})", m.display_name)
            } else {
                String::new()
            }
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Markdown reference of every tool, for docs/TOOLS.md.
pub fn docs_tools() -> Result<ExitCode> {
    let ws = Arc::new(aispice_tools::Workspace::new());
    let reg = registry(ws);
    println!(
        "# Tools\n\nGenerated with `aispice docs tools`. The desktop app's agent and the MCP server expose the same tools.\n"
    );
    for (s, access) in reg.specs_with_access() {
        let class = match access {
            aispice_agent::Access::ReadOnly => {
                "Read-only: writes nothing of yours, only aispice's own folders."
            }
            aispice_agent::Access::Edit => {
                "Edits your files: each write is approved in ask-before-applying mode and can be undone."
            }
        };
        println!(
            "## `{}`\n\n{}\n\n{class}\n\n<details><summary>Input schema</summary>\n\n```json\n{}\n```\n\n</details>\n",
            s.name,
            s.description,
            serde_json::to_string_pretty(&s.input_schema)?
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::terminal_safe;

    #[test]
    fn escape_and_bidi_sequences_are_neutralised() {
        let hostile = "ok\u{1b}[2K\u{1b}[1A\rApply? [y/N] y\u{202E}gnp.exe\u{7}\nnext\tline";
        let safe = terminal_safe(hostile);
        assert!(
            !safe.contains('\u{1b}')
                && !safe.contains('\r')
                && !safe.contains('\u{202E}')
                && !safe.contains('\u{7}')
        );
        assert!(safe.contains("\nnext\tline"));
    }
}

//! `aispice eval`: run the design task suite against one or more models and
//! report the pass rate per model. A benchmark, not a test: the exit code is
//! 0 whatever the results, unless `--min-pass-rate` asks otherwise.

use crate::cmd_agent::terminal_safe;
use aispice_agent::providers::{build_provider, default_model};
use aispice_agent::{Config, KeyStore, Provider};
use aispice_tools::eval::{EvalOptions, ModelUnderTest, Patient, TaskRun, load_suite, run_suite};
use anyhow::{Context, Result, anyhow, bail};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

pub struct EvalArgs {
    pub suite: PathBuf,
    pub tasks: Vec<String>,
    pub provider: Option<String>,
    pub models: Vec<String>,
    pub repeat: u32,
    pub jobs: usize,
    pub out: Option<PathBuf>,
    pub min_pass_rate: Option<f64>,
    pub rpm: Option<f64>,
    pub timeout: u64,
    pub symbols: Vec<PathBuf>,
    pub json: bool,
}

fn progress(r: &TaskRun, done: usize, total: usize) -> String {
    let outcome = if r.skipped {
        "skipped".to_string()
    } else if r.pass {
        "pass".to_string()
    } else {
        let mut reason: String = r.reason.chars().take(160).collect();
        if r.reason.chars().count() > 160 {
            reason.push_str("...");
        }
        format!("FAIL: {reason}")
    };
    terminal_safe(&format!(
        "[{done}/{total}] {} {}: {outcome} ({} steps, {:.0} s)",
        r.label(),
        r.task,
        r.steps,
        r.agent_ms as f64 / 1000.0
    ))
}

pub fn eval(args: EvalArgs) -> Result<ExitCode> {
    if let Some(m) = args.min_pass_rate
        && !(0.0..=1.0).contains(&m)
    {
        bail!("--min-pass-rate is a fraction from 0 to 1, for example 0.8");
    }
    let mut tasks = load_suite(&args.suite).map_err(|e| {
        anyhow!("{e}. Pass the folder of tasks with --suite (the repository has one at evals/).")
    })?;
    if !args.tasks.is_empty() {
        let known: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
        if let Some(bad) = args.tasks.iter().find(|id| !known.contains(id)) {
            bail!("no task `{bad}` in the suite; tasks: {}", known.join(", "));
        }
        tasks.retain(|t| args.tasks.contains(&t.id));
    }

    let cfg = Config::load().unwrap_or_default();
    let keys = KeyStore::system();
    let _ = keys.migrate_legacy();
    let provider_id = args.provider.unwrap_or_else(|| cfg.provider.clone());
    let base = build_provider(&provider_id, &cfg, &keys).map_err(|e| anyhow!("{e}"))?;
    let provider: Arc<dyn Provider> = Arc::new(Patient::new(base).with_rpm(args.rpm));
    let model_ids = if args.models.is_empty() {
        vec![
            cfg.model_for(&provider_id)
                .or_else(|| default_model(&provider_id).map(str::to_string))
                .ok_or_else(|| {
                    anyhow!("no model chosen for {provider_id}; pass --model (see `aispice models {provider_id}`)")
                })?,
        ]
    } else {
        args.models
    };
    let models: Vec<ModelUnderTest> = model_ids
        .into_iter()
        .map(|model| ModelUnderTest {
            provider_id: provider_id.clone(),
            model,
            provider: provider.clone(),
        })
        .collect();

    // Tasks drawn in a PDK's devices run on the user's registered PDK of
    // that kind, and are skipped without one.
    let pdks = match aispice_tools::pdk::Registry::load() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("warning: {e}; tasks that need a PDK will be skipped");
            Default::default()
        }
    };
    let opts = EvalOptions {
        repeat: args.repeat.max(1),
        jobs: args.jobs.max(1),
        simulator: "ngspice".into(),
        symbols: args.symbols,
        thinking: cfg.thinking_config(),
        max_tokens: cfg.agent.max_tokens,
        timeout: Duration::from_secs(args.timeout.max(1)),
        pdks,
        ..EvalOptions::default()
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let runner = aispice_tools::Runner::default();
    let ngspice = rt
        .block_on(runner.detect())
        .into_iter()
        .any(|(id, d)| d.found && id.name() == "ngspice");
    if !ngspice {
        bail!(
            "the suite is judged with ngspice, which is not installed: apt install ngspice, brew install ngspice, or https://ngspice.sourceforge.io"
        );
    }
    eprintln!(
        "{} task(s) x {} model(s) x {} run(s) from {}",
        tasks.len(),
        models.len(),
        opts.repeat,
        args.suite.display()
    );
    let suite_name = args.suite.display().to_string();
    let report = rt.block_on(run_suite(
        &suite_name,
        &tasks,
        &models,
        &opts,
        &mut |r, done, total| eprintln!("{}", progress(r, done, total)),
    ));

    if let Some(path) = &args.out {
        let text = serde_json::to_string_pretty(&report)?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        eprintln!("Report written to {}", path.display());
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!();
        print!("{}", terminal_safe(&report.to_text()));
    }
    if let Some(min) = args.min_pass_rate {
        let below = report.below(min);
        if !below.is_empty() {
            for m in below {
                eprintln!(
                    "{}: pass rate {:.0}% is below the minimum {:.0}%",
                    terminal_safe(&m.label()),
                    m.pass_rate * 100.0,
                    min * 100.0
                );
            }
            return Ok(ExitCode::from(1));
        }
    }
    Ok(ExitCode::SUCCESS)
}

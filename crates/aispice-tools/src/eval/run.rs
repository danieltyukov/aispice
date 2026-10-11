//! Running the suite: each task in a fresh temporary project, with edits
//! applied at once (nobody is there to approve them), then judged.

use super::judge::{FileState, OUTPUT_KEPT, ToolRecord, Transcript, Verdict, judge};
use super::report::{Report, TaskRun, summarize};
use super::task::Task;
use crate::project::{Project, ProjectOptions, now_ms};
use crate::prompt::{PromptContext, system_prompt};
use crate::runner::RunnerConfig;
use crate::tools::registry;
use crate::workspace::Workspace;
use aispice_agent::provider::{Provider, ThinkingConfig};
use aispice_agent::{Agent, AgentEvent, AgentStopReason, ContentBlock, Role, ToolContext};
use futures::StreamExt;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// A model to evaluate.
#[derive(Clone)]
pub struct ModelUnderTest {
    pub provider_id: String,
    pub model: String,
    pub provider: Arc<dyn Provider>,
}

impl ModelUnderTest {
    pub fn label(&self) -> String {
        format!("{}/{}", self.provider_id, self.model)
    }
}

#[derive(Debug, Clone)]
pub struct EvalOptions {
    /// Runs of each task per model.
    pub repeat: u32,
    /// Runs in flight at once for one model.
    pub jobs: usize,
    /// Simulator for the agent and the judge.
    pub simulator: String,
    /// Extra symbol folders, as `--symbols` gives them.
    pub symbols: Vec<PathBuf>,
    pub thinking: Option<ThinkingConfig>,
    pub max_tokens: Option<u32>,
    /// Wall-clock limit for one run of the agent.
    pub timeout: Duration,
    /// After this many runs in a row end in a provider error, the model's
    /// remaining runs are skipped.
    pub max_consecutive_errors: u32,
}

impl Default for EvalOptions {
    fn default() -> Self {
        Self {
            repeat: 1,
            jobs: 1,
            simulator: "ngspice".into(),
            symbols: Vec::new(),
            thinking: Some(ThinkingConfig::adaptive()),
            max_tokens: None,
            timeout: Duration::from_secs(15 * 60),
            max_consecutive_errors: 3,
        }
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn blank_run(task: &Task, model: &ModelUnderTest, attempt: u32) -> TaskRun {
    TaskRun {
        task: task.id.clone(),
        difficulty: task.difficulty,
        provider: model.provider_id.clone(),
        model: model.model.clone(),
        attempt,
        pass: false,
        reason: String::new(),
        failures: Vec::new(),
        steps: 0,
        max_steps: task.max_steps,
        tool_calls: 0,
        tool_errors: 0,
        tools: BTreeMap::new(),
        calls: Vec::new(),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        agent_ms: 0,
        judge_ms: 0,
        stop_reason: String::new(),
        error: None,
        skipped: false,
        verdict: Verdict::default(),
        answer: String::new(),
    }
}

fn failed_setup(mut run: TaskRun, message: String) -> TaskRun {
    run.failures = vec![message.clone()];
    run.reason = message.clone();
    run.error = Some(message);
    run
}

fn stop_name(reason: &AgentStopReason) -> String {
    match reason {
        AgentStopReason::EndTurn => "end_turn".into(),
        AgentStopReason::MaxSteps => "max_steps".into(),
        AgentStopReason::MaxTokens => "max_tokens".into(),
        AgentStopReason::Refusal => "refusal".into(),
        AgentStopReason::Cancelled => "cancelled".into(),
        AgentStopReason::Other(o) => o.clone(),
    }
}

/// Run one task once with one model and judge the result.
pub async fn run_task(
    task: &Task,
    model: &ModelUnderTest,
    opts: &EvalOptions,
    attempt: u32,
) -> TaskRun {
    let mut run = blank_run(task, model, attempt);
    let tmp = match tempfile::Builder::new().prefix("aispice-eval-").tempdir() {
        Ok(t) => t,
        Err(e) => return failed_setup(run, format!("could not make a temporary folder: {e}")),
    };
    // The project folder is named after the task, which is what the model
    // sees as the project name.
    let root = tmp.path().join(&task.id);
    if let Err(e) = std::fs::create_dir_all(&root).and_then(|_| copy_dir(&task.files_dir(), &root))
    {
        return failed_setup(run, format!("could not copy the task files: {e}"));
    }
    let before = FileState::capture(&root);
    let ws = Workspace::new();
    ws.runner.set_config(RunnerConfig {
        simulator: opts.simulator.clone(),
        // aispice's own models only, so results compare across machines
        // whatever LTspice library is installed.
        embedded_models_only: true,
        ..RunnerConfig::default()
    });
    // Built-in symbols only (plus --symbols), so results compare across
    // machines whether or not LTspice is installed.
    let options = ProjectOptions {
        extra_symbol_dirs: opts.symbols.clone(),
        ltspice_lib: None,
    };
    match Project::open(&root, options) {
        Ok(p) => ws.set_project(p),
        Err(e) => return failed_setup(run, format!("could not open the project: {e}")),
    }
    let ws = Arc::new(ws);
    let system = system_prompt(&PromptContext {
        project: Some(root.display().to_string()),
        circuit: None,
        simulators: vec![opts.simulator.clone()],
        ltspice_library: false,
        pdk: crate::pdk::ProjectConfig::load(&root)
            .ok()
            .and_then(|c| c.pdk),
    });
    let mut agent = Agent::new(
        model.provider.clone(),
        Arc::new(registry(ws.clone())),
        &model.model,
    )
    .with_system(system)
    .with_max_steps(task.max_steps)
    .with_thinking(opts.thinking.clone());
    if let Some(m) = opts.max_tokens {
        agent = agent.with_max_tokens(m);
    }

    let cancel = CancellationToken::new();
    let timer = tokio::spawn({
        let cancel = cancel.clone();
        let limit = opts.timeout;
        async move {
            tokio::time::sleep(limit).await;
            cancel.cancel();
        }
    });
    let mut transcript = Transcript::default();
    let mut history = Vec::new();
    let started = Instant::now();
    let result = {
        let tools = &mut transcript.tools;
        let mut inputs: HashMap<String, Value> = HashMap::new();
        let mut on_event = |e: AgentEvent| match e {
            AgentEvent::ToolStart { id, input, .. } => {
                inputs.insert(id, input);
            }
            AgentEvent::ToolEnd {
                id, name, output, ..
            } => {
                let text = output.text_content();
                tools.push(ToolRecord {
                    name,
                    input: inputs.remove(&id).unwrap_or(Value::Null),
                    is_error: output.is_error,
                    output: text.chars().take(OUTPUT_KEPT).collect(),
                    data: output.data,
                });
            }
            _ => {}
        };
        agent
            .run(
                &mut history,
                vec![ContentBlock::text(task.prompt.clone())],
                ToolContext::new(cancel.clone()),
                &mut on_event,
            )
            .await
    };
    timer.abort();
    run.agent_ms = started.elapsed().as_millis() as u64;

    let texts: Vec<String> = history
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.text())
        .filter(|t| !t.trim().is_empty())
        .collect();
    transcript.text = texts.join("\n\n");
    run.answer = texts.last().cloned().unwrap_or_default();
    run.tool_calls = transcript.tools.len() as u32;
    run.tool_errors = transcript.tools.iter().filter(|t| t.is_error).count() as u32;
    for t in &transcript.tools {
        *run.tools.entry(t.name.clone()).or_default() += 1;
    }
    run.calls = transcript.tools.clone();

    let mut failures = Vec::new();
    match &result {
        Ok(summary) => {
            run.steps = summary.steps;
            run.input_tokens = summary.usage.input_tokens;
            run.output_tokens = summary.usage.output_tokens;
            run.cache_read_tokens = summary.usage.cache_read_tokens;
            run.cache_write_tokens = summary.usage.cache_write_tokens;
            run.stop_reason = stop_name(&summary.stop_reason);
            match &summary.stop_reason {
                AgentStopReason::MaxSteps => failures.push(format!(
                    "exceeded the step budget of {} steps",
                    task.max_steps
                )),
                AgentStopReason::Cancelled => {
                    let msg = format!("timed out after {} s", opts.timeout.as_secs());
                    run.error = Some(msg.clone());
                    failures.push(msg);
                }
                AgentStopReason::Refusal => failures.push("the model refused".into()),
                AgentStopReason::MaxTokens => {
                    failures.push("the last reply hit the output token limit".into())
                }
                _ => {}
            }
        }
        Err(e) => {
            let msg = format!("provider error: {e}");
            run.stop_reason = "error".into();
            run.error = Some(msg.clone());
            failures.push(msg);
        }
    }

    // A provider error says nothing about the model's design, so the judge
    // only runs on runs that finished.
    if run.error.is_none() {
        let judged = Instant::now();
        run.verdict = judge(
            task,
            &root,
            &before,
            &transcript,
            &opts.simulator,
            &opts.symbols,
        )
        .await;
        run.judge_ms = judged.elapsed().as_millis() as u64;
        failures.extend(run.verdict.failures.iter().cloned());
    }
    run.pass = failures.is_empty();
    run.reason = if run.pass {
        "pass".into()
    } else {
        failures.join("; ")
    };
    run.failures = failures;
    run
}

/// Run every task with every model, `opts.repeat` times each. Models run one
/// after another; a model's runs go `opts.jobs` at a time. `on_run` sees
/// each run as it finishes, with how many are done out of how many.
pub async fn run_suite(
    suite: &str,
    tasks: &[Task],
    models: &[ModelUnderTest],
    opts: &EvalOptions,
    on_run: &mut (dyn FnMut(&TaskRun, usize, usize) + Send),
) -> Report {
    let started = now_ms();
    let repeat = opts.repeat.max(1);
    let total = tasks.len() * models.len() * repeat as usize;
    let mut done = 0;
    let mut runs = Vec::with_capacity(total);
    for model in models {
        let jobs: Vec<(usize, &Task, u32)> = tasks
            .iter()
            .flat_map(|t| (1..=repeat).map(move |a| (t, a)))
            .enumerate()
            .map(|(i, (t, a))| (i, t, a))
            .collect();
        let consecutive_errors = AtomicU32::new(0);
        let consecutive_errors = &consecutive_errors;
        let mut finished: Vec<(usize, TaskRun)> = Vec::with_capacity(jobs.len());
        let mut stream = futures::stream::iter(jobs)
            .map(|(i, task, attempt)| async move {
                if consecutive_errors.load(Ordering::SeqCst) >= opts.max_consecutive_errors {
                    let mut r = blank_run(task, model, attempt);
                    r.skipped = true;
                    r.reason = "skipped: the provider failed several runs in a row".into();
                    r.failures = vec![r.reason.clone()];
                    return (i, r);
                }
                let r = run_task(task, model, opts, attempt).await;
                if r.error
                    .as_deref()
                    .is_some_and(|e| e.starts_with("provider error"))
                {
                    consecutive_errors.fetch_add(1, Ordering::SeqCst);
                } else {
                    consecutive_errors.store(0, Ordering::SeqCst);
                }
                (i, r)
            })
            .buffer_unordered(opts.jobs.max(1));
        while let Some((i, r)) = stream.next().await {
            done += 1;
            on_run(&r, done, total);
            finished.push((i, r));
        }
        finished.sort_by_key(|(i, _)| *i);
        runs.extend(finished.into_iter().map(|(_, r)| r));
    }
    Report {
        aispice_version: env!("CARGO_PKG_VERSION").into(),
        suite: suite.into(),
        started,
        simulator: opts.simulator.clone(),
        repeat,
        models: summarize(&runs),
        runs,
    }
}

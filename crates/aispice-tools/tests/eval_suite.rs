//! The design task suite and its runner, with scripted models in place of a
//! provider. Tests that simulate skip when ngspice is missing.

use aispice_agent::provider::{ChatResponse, ProviderError};
use aispice_agent::testing::{ScriptedProvider, Step, text_reply, tool_reply};
use aispice_tools::eval::{
    EvalOptions, ModelUnderTest, Patient, Task, TaskRun, load_suite, run_suite, run_task,
};
use aispice_tools::{Project, ProjectOptions};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn have_ngspice() -> bool {
    std::process::Command::new("ngspice")
        .arg("--version")
        .output()
        .is_ok()
}

fn suite_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../evals")
}

fn suite() -> Vec<Task> {
    load_suite(&suite_dir()).unwrap_or_else(|e| panic!("{e}"))
}

fn task(id: &str) -> Task {
    suite()
        .into_iter()
        .find(|t| t.id == id)
        .unwrap_or_else(|| panic!("no task {id}"))
}

/// One model turn: tool calls `(name, input)`, or a final answer.
enum Turn {
    Tools(Vec<(String, Value)>),
    Answer(String),
}

fn reply(turn: &Turn, step: usize) -> ChatResponse {
    match turn {
        Turn::Answer(text) => text_reply(text),
        Turn::Tools(calls) => {
            let ids: Vec<String> = (0..calls.len()).map(|k| format!("s{step}c{k}")).collect();
            tool_reply(
                calls
                    .iter()
                    .zip(&ids)
                    .map(|((name, input), id)| (id.as_str(), name.as_str(), input.clone())),
            )
        }
    }
}

fn scripted(turns: &[Turn]) -> ModelUnderTest {
    let mut p = ScriptedProvider::new();
    for (i, t) in turns.iter().enumerate() {
        p = p.then_reply(reply(t, i));
    }
    ModelUnderTest {
        provider_id: "scripted".into(),
        model: "solver".into(),
        provider: Arc::new(p),
    }
}

/// The task's reference solution, `solution.json`: a list of turns, each
/// `{"tools": [{"name", "input"}]}` or `{"answer": "..."}`.
fn solution(task: &Task) -> Vec<Turn> {
    let path = task.dir.join("solution.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let steps: Vec<Value> =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    steps
        .iter()
        .map(|s| match s["answer"].as_str() {
            Some(a) => Turn::Answer(a.to_string()),
            None => Turn::Tools(
                s["tools"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{}: a turn needs tools or answer", path.display()))
                    .iter()
                    .map(|t| {
                        (
                            t["name"].as_str().expect("tool name").to_string(),
                            t["input"].clone(),
                        )
                    })
                    .collect(),
            ),
        })
        .collect()
}

fn options() -> EvalOptions {
    EvalOptions {
        thinking: None,
        ..EvalOptions::default()
    }
}

fn tools(calls: &[(&str, Value)]) -> Turn {
    Turn::Tools(
        calls
            .iter()
            .map(|(n, v)| (n.to_string(), v.clone()))
            .collect(),
    )
}

#[test]
fn every_task_in_the_suite_is_valid() {
    let tasks = suite();
    assert!(
        (10..=15).contains(&tasks.len()),
        "{} tasks; the suite should have 10 to 15",
        tasks.len()
    );
    let mut ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), tasks.len(), "task ids must be unique");
    let tmp = tempfile::tempdir().unwrap();
    let empty = Project::open(tmp.path(), ProjectOptions::default()).unwrap();
    for t in &tasks {
        // Prose rules: no long dashes in what a model or reader sees.
        for text in [&t.prompt, &t.title] {
            assert!(
                !text.contains('\u{2014}') && !text.contains('\u{2013}'),
                "{}: long dash in {text}",
                t.id
            );
        }
        // The judged circuit stays inside the project.
        let resolved = empty
            .resolve(&t.circuit)
            .unwrap_or_else(|e| panic!("{}: {e}", t.id));
        assert!(resolved.starts_with(empty.root()), "{}", t.id);
        // The model has to learn the judged file's name from the prompt.
        assert!(
            t.prompt.contains(&t.circuit),
            "{}: the prompt does not name {}",
            t.id,
            t.circuit
        );
        // Starting circuits parse and netlist without warnings.
        let files = t.start_files();
        if !files.is_empty() {
            let p = Project::open(&t.files_dir(), ProjectOptions::default()).unwrap();
            for f in files.iter().filter(|f| f.ends_with(".asc")) {
                let (sch, warnings) = p.load(f).unwrap_or_else(|e| panic!("{}/{f}: {e}", t.id));
                assert!(warnings.is_empty(), "{}/{f}: {warnings:?}", t.id);
                let (built, _) = aispice_core::netlist::build(&sch, p.library(), "* check");
                assert!(
                    built.warnings.is_empty(),
                    "{}/{f}: {:?}",
                    t.id,
                    built.warnings
                );
            }
        }
        // A reference solution exists, parses, ends in an answer and fits
        // the step budget.
        let turns = solution(t);
        assert!(
            turns.len() as u32 <= t.max_steps,
            "{}: the reference solution takes {} steps, over the budget of {}",
            t.id,
            turns.len(),
            t.max_steps
        );
        assert!(
            matches!(turns.last(), Some(Turn::Answer(_))),
            "{}: the solution must end with an answer",
            t.id
        );
    }
}

/// Every task is solvable with ngspice and the built-in symbols: its
/// reference solution, played through the real tools, passes the judge.
#[tokio::test(flavor = "multi_thread")]
async fn every_reference_solution_passes() {
    if !have_ngspice() {
        eprintln!("skipped: ngspice not installed");
        return;
    }
    let tasks = suite();
    let opts = options();
    let runs: Vec<TaskRun> = futures::future::join_all(tasks.iter().map(|t| {
        let model = scripted(&solution(t));
        let opts = opts.clone();
        async move { run_task(t, &model, &opts, 1).await }
    }))
    .await;
    let failed: Vec<String> = runs
        .iter()
        .filter(|r| !r.pass || r.tool_errors > 0)
        .map(|r| format!("{} ({} tool errors): {}", r.task, r.tool_errors, r.reason))
        .collect();
    assert!(
        failed.is_empty(),
        "reference solutions failed:\n{}",
        failed.join("\n")
    );
}

/// No task is solved before the model starts: claiming success without
/// doing anything fails every one of them.
#[tokio::test(flavor = "multi_thread")]
async fn doing_nothing_fails_every_task() {
    if !have_ngspice() {
        eprintln!("skipped: ngspice not installed");
        return;
    }
    let tasks = suite();
    let opts = options();
    let runs: Vec<TaskRun> = futures::future::join_all(tasks.iter().map(|t| {
        let model = scripted(&[Turn::Answer(
            "Done, everything meets the requirements.".into(),
        )]);
        let opts = opts.clone();
        async move { run_task(t, &model, &opts, 1).await }
    }))
    .await;
    let passed: Vec<&str> = runs
        .iter()
        .filter(|r| r.pass)
        .map(|r| r.task.as_str())
        .collect();
    assert!(passed.is_empty(), "passed without any work: {passed:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_correct_change_passes() {
    if !have_ngspice() {
        eprintln!("skipped: ngspice not installed");
        return;
    }
    let t = task("rc-corner");
    let model = scripted(&[
        tools(&[("read_schematic", json!({"circuit": "rc.asc"}))]),
        tools(&[(
            "edit_schematic",
            json!({"circuit": "rc.asc", "edits": [{"op": "set_value", "name": "R1", "value": "160"}]}),
        )]),
        Turn::Answer("R1 is 160 ohm; the corner is 9.95 kHz.".into()),
    ]);
    let r = run_task(&t, &model, &options(), 1).await;
    assert!(r.pass, "{}", r.reason);
    assert_eq!(r.reason, "pass");
    assert_eq!((r.steps, r.tool_calls, r.tool_errors), (3, 2, 0));
    assert_eq!((r.input_tokens, r.output_tokens), (30, 15));
    assert_eq!(r.stop_reason, "end_turn");
    assert_eq!(r.answer, "R1 is 160 ohm; the corner is 9.95 kHz.");
    // The report keeps each call's input and the start of its output.
    let edit = &r.calls[1];
    assert_eq!(edit.name, "edit_schematic");
    assert_eq!(edit.input["edits"][0]["value"], "160");
    assert!(edit.output.starts_with("Saved rc.asc"), "{}", edit.output);
    let fc = r.verdict.specs.as_ref().unwrap().row("fc").unwrap();
    assert!(
        fc.pass && (fc.value.unwrap() - 9947.0).abs() < 100.0,
        "{fc:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_change_fails_and_names_the_spec() {
    if !have_ngspice() {
        eprintln!("skipped: ngspice not installed");
        return;
    }
    let t = task("rc-corner");
    // Moves the corner the wrong way, and breaks the E24 rule too.
    let model = scripted(&[
        tools(&[(
            "edit_schematic",
            json!({"circuit": "rc.asc", "edits": [{"op": "set_value", "name": "R1", "value": "2.21k"}]}),
        )]),
        Turn::Answer("Done, the corner is now 10 kHz.".into()),
    ]);
    let r = run_task(&t, &model, &options(), 1).await;
    assert!(!r.pass);
    assert!(r.reason.contains("spec fc: "), "{}", r.reason);
    assert!(r.reason.contains("FAIL"), "{}", r.reason);
    assert!(
        r.reason.contains("R1 = 2.21k is not an E24 value"),
        "{}",
        r.reason
    );
    assert!(r.error.is_none());
}

/// The judge checks a task's corners: a value that meets the band at room
/// temperature fails at the hot corner, and the reason names it.
#[tokio::test(flavor = "multi_thread")]
async fn the_judge_checks_every_corner() {
    if !have_ngspice() {
        eprintln!("skipped: ngspice not installed");
        return;
    }
    let t = task("temp-corners");
    let model = scripted(&[
        tools(&[(
            "edit_schematic",
            json!({"circuit": "rc.asc", "edits": [{"op": "set_value", "name": "C1", "value": "100n"}]}),
        )]),
        Turn::Answer("C1 is 100 nF, a 1.59 kHz corner.".into()),
    ]);
    let r = run_task(&t, &model, &options(), 1).await;
    assert!(!r.pass);
    assert!(r.reason.contains("worst at 125C"), "{}", r.reason);
}

/// Needs no simulator: the step budget fails the run whatever the judge
/// finds.
#[tokio::test(flavor = "multi_thread")]
async fn running_out_of_steps_fails() {
    let t = task("rc-corner");
    let read = || tools(&[("read_schematic", json!({"circuit": "rc.asc"}))]);
    let turns: Vec<Turn> = (0..t.max_steps + 2).map(|_| read()).collect();
    let r = run_task(&t, &scripted(&turns), &options(), 1).await;
    assert!(!r.pass);
    assert_eq!(r.stop_reason, "max_steps");
    assert_eq!(r.steps, t.max_steps);
    assert!(
        r.reason.starts_with(&format!(
            "exceeded the step budget of {} steps",
            t.max_steps
        )),
        "{}",
        r.reason
    );
}

/// The explanation task is judged on the answer and the files only, so it
/// runs without a simulator.
#[tokio::test(flavor = "multi_thread")]
async fn explanations_are_judged_on_the_answer_and_untouched_files() {
    let t = task("explain-filter");
    let read = || tools(&[("read_schematic", json!({"circuit": "mystery.asc"}))]);
    let good = scripted(&[
        read(),
        Turn::Answer(
            "A first-order RC high-pass; the corner is 1/(2 pi 4.7k 33n) = 1.03 kHz.".into(),
        ),
    ]);
    let r = run_task(&t, &good, &options(), 1).await;
    assert!(r.pass, "{}", r.reason);

    let wrong = scripted(&[
        read(),
        Turn::Answer("A low-pass filter with its corner at 2 kHz.".into()),
    ]);
    let r = run_task(&t, &wrong, &options(), 1).await;
    assert!(
        r.reason.contains("mentions none of: high-pass"),
        "{}",
        r.reason
    );
    assert!(
        r.reason.contains("no number within 5% of 1026"),
        "{}",
        r.reason
    );

    let meddler = scripted(&[
        tools(&[(
            "edit_schematic",
            json!({"circuit": "mystery.asc", "edits": [{"op": "set_value", "name": "R1", "value": "5k"}]}),
        )]),
        Turn::Answer("A high-pass with its corner at 1.03 kHz.".into()),
    ]);
    let r = run_task(&t, &meddler, &options(), 1).await;
    assert!(!r.pass);
    assert!(
        r.reason.contains("files changed: mystery.asc was changed"),
        "{}",
        r.reason
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_suite_reports_per_model_and_skips_a_failing_provider() {
    let t = task("explain-filter");
    let good = scripted(&[
        tools(&[("read_schematic", json!({"circuit": "mystery.asc"}))]),
        Turn::Answer("An RC high-pass filter with a 1.03 kHz corner.".into()),
        tools(&[("read_schematic", json!({"circuit": "mystery.asc"}))]),
        Turn::Answer("An RC low-pass.".into()),
    ]);
    let broken = ModelUnderTest {
        provider_id: "scripted".into(),
        model: "broken".into(),
        provider: Arc::new(
            ScriptedProvider::new().then(Step::Fail(ProviderError::Auth {
                status: 401,
                message: "bad key".into(),
            })),
        ),
    };
    let opts = EvalOptions {
        repeat: 2,
        max_consecutive_errors: 1,
        ..options()
    };
    let mut seen = Vec::new();
    let report = run_suite(
        "evals",
        &[t],
        &[good, broken],
        &opts,
        &mut |r, done, total| {
            seen.push((r.model.clone(), done, total));
        },
    )
    .await;
    assert_eq!(seen.len(), 4);
    assert_eq!((seen[3].1, seen[3].2), (4, 4));
    assert_eq!(report.models.len(), 2);
    let (a, b) = (&report.models[0], &report.models[1]);
    assert_eq!((a.runs, a.passed, a.errors), (2, 1, 0));
    assert_eq!((b.runs, b.passed, b.errors, b.skipped), (1, 0, 1, 1));
    let broken_runs: Vec<&TaskRun> = report.runs.iter().filter(|r| r.model == "broken").collect();
    assert!(
        broken_runs[0]
            .reason
            .starts_with("provider error: authentication failed"),
        "{}",
        broken_runs[0].reason
    );
    assert!(broken_runs[1].skipped);
    let text = report.to_text();
    assert!(
        text.contains("scripted/solver") && text.contains("scripted/broken"),
        "{text}"
    );
    assert!(text.contains("1/2 (50%)"), "{text}");
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["runs"].as_array().unwrap().len(), 4);
    assert_eq!(json["models"][0]["passed"], 1);
}

#[tokio::test]
async fn rate_limits_are_waited_out_then_given_up() {
    use aispice_agent::Provider;
    let limited = || {
        Step::Fail(ProviderError::RateLimited {
            message: "slow down".into(),
            retry_after: Some(Duration::ZERO),
        })
    };
    let req = aispice_agent::ChatRequest {
        model: "m".into(),
        system: String::new(),
        messages: vec![aispice_agent::Message::user_text("hi")],
        tools: vec![],
        max_tokens: 10,
        temperature: None,
        thinking: None,
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let inner = Arc::new(
        ScriptedProvider::new()
            .then(limited())
            .then(limited())
            .then_text("ok"),
    );
    let mut patient = Patient::new(inner.clone()).with_rpm(Some(6000.0));
    patient.min_backoff = Duration::ZERO;
    let out = patient.stream(&req, &mut |_| {}, &cancel).await.unwrap();
    assert_eq!(out.message.text(), "ok");
    assert_eq!(inner.requests().len(), 3);

    // A hinted wait longer than the patience left gives up at once.
    let inner = Arc::new(
        ScriptedProvider::new().then(Step::Fail(ProviderError::RateLimited {
            message: "Please retry in 30s".into(),
            retry_after: None,
        })),
    );
    let mut patient = Patient::new(inner.clone());
    patient.max_wait = Duration::from_secs(1);
    let err = patient
        .stream(&req, &mut |_| {}, &cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::RateLimited { .. }), "{err}");
    assert_eq!(inner.requests().len(), 1);
}

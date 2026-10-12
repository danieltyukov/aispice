//! Tasks of the design suite: one folder per task with a `task.toml` and an
//! optional `files/` folder that becomes the starting project.

use aispice_core::units;
use aispice_sim::corner::CornerSet;
use aispice_sim::optimize::ESeries;
use aispice_sim::spec::{Spec, parse_specs};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Difficulty {
    Easy,
    #[default]
    Medium,
    Hard,
}

impl Difficulty {
    pub fn as_str(self) -> &'static str {
        match self {
            Difficulty::Easy => "easy",
            Difficulty::Medium => "medium",
            Difficulty::Hard => "hard",
        }
    }
}

/// A check made on the result after the run, beyond the spec table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Constraint {
    /// A component with this instance name exists.
    PartExists { name: String },
    /// The circuit has at most this many components.
    MaxParts { count: usize },
    /// Between `min` and `max` components drawn with this `symbol`
    /// (`OpAmps/opamp`), or with this device letter `prefix` (`C` counts
    /// `cap` and `polcap` alike). Give one of the two.
    PartCount {
        #[serde(default)]
        symbol: Option<String>,
        #[serde(default)]
        prefix: Option<String>,
        #[serde(default)]
        min: Option<usize>,
        #[serde(default)]
        max: Option<usize>,
    },
    /// A component's value, compared as a number (`100n` equals `0.1u`).
    Value {
        name: String,
        equals: String,
        #[serde(default = "default_tol_pct")]
        tol_pct: f64,
    },
    /// Every part with this device letter (R, C, L) has a value in range.
    ValueRange {
        prefix: String,
        #[serde(default)]
        min: Option<String>,
        #[serde(default)]
        max: Option<String>,
    },
    /// Every part with this device letter has a standard value of the series.
    ESeries { prefix: String, series: ESeries },
    /// No lint warnings either (lint errors fail every task by default).
    LintClean,
    /// Every starting file is unchanged and no new circuit file appeared.
    /// New `.specs` files are allowed: they record requirements, not design
    /// changes.
    FilesUnchanged,
    /// The model called this tool at least once and it succeeded.
    ToolCalled { name: String },
    /// The model's text contains at least one of these phrases, ignoring case.
    AnswerMentions { any: Vec<String> },
    /// The model's text contains a number within `tol_pct` of `value`
    /// (`1.59 kHz`, `1591 Hz` and `1.6k` all read as numbers).
    AnswerNumber { value: String, tol_pct: f64 },
    /// The model's text contains the number a tool reported, found at the
    /// JSON `pointer` in the tool's structured output, within `tol`.
    AnswerReports {
        tool: String,
        pointer: String,
        tol: f64,
    },
    /// Every instance of this subcircuit sets `param` within limits, for
    /// example the op-amp's `GBW`.
    SubcktParam {
        subckt: String,
        param: String,
        #[serde(default)]
        min: Option<String>,
        #[serde(default)]
        max: Option<String>,
    },
}

fn default_tol_pct() -> f64 {
    0.1
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    difficulty: Difficulty,
    prompt: String,
    circuit: String,
    #[serde(default)]
    specs: String,
    #[serde(default)]
    analysis: Option<String>,
    #[serde(default)]
    corners: Option<CornerSet>,
    #[serde(default)]
    pdk: Option<String>,
    max_steps: u32,
    #[serde(default = "yes")]
    lint: bool,
    #[serde(default)]
    constraints: Vec<Constraint>,
}

/// One design task, loaded and checked.
#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub difficulty: Difficulty,
    /// What the user types.
    pub prompt: String,
    /// The circuit the result is judged on, relative to the project.
    pub circuit: String,
    /// The spec table as written, in the format `check_specs` reads.
    pub specs_text: String,
    #[serde(skip)]
    pub specs: Vec<Spec>,
    /// Analysis the judge runs instead of the circuit's own, so a missing or
    /// different analysis in the model's circuit cannot hide a result.
    pub analysis: Option<String>,
    /// Corners the specs must hold at, when the task names any.
    pub corners: Option<CornerSet>,
    /// The kind of PDK the task's circuits are drawn in (`ihp-sg13g2`). The
    /// runner opts the project into a registered PDK of that kind, and
    /// skips the task when there is none.
    pub pdk: Option<String>,
    /// Model calls the agent may make.
    pub max_steps: u32,
    /// Whether lint errors fail the task.
    pub lint: bool,
    pub constraints: Vec<Constraint>,
    #[serde(skip)]
    pub dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
#[error("{path}: {message}")]
pub struct TaskError {
    pub path: String,
    pub message: String,
}

fn task_err(path: &Path, message: impl Into<String>) -> TaskError {
    TaskError {
        path: path.display().to_string(),
        message: message.into(),
    }
}

impl Task {
    /// The folder copied into the project before the run.
    pub fn files_dir(&self) -> PathBuf {
        self.dir.join("files")
    }

    /// Starting files, relative to the project, `/` separated.
    pub fn start_files(&self) -> Vec<String> {
        let mut out = Vec::new();
        collect_files(&self.files_dir(), &self.files_dir(), &mut out);
        out.sort();
        out
    }
}

pub(crate) fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let Ok(kind) = e.file_type() else { continue };
        if kind.is_dir() {
            collect_files(root, &path, out);
        } else if kind.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// A project-relative path that cannot leave the project.
fn safe_relative(path: &str) -> bool {
    let p = Path::new(path);
    !path.trim().is_empty()
        && !p.is_absolute()
        && p.components().all(|c| matches!(c, Component::Normal(_)))
}

fn check_number(path: &Path, what: &str, v: &Option<String>) -> Result<(), TaskError> {
    match v {
        Some(t) if units::parse(t).is_none() => {
            Err(task_err(path, format!("{what} `{t}` is not a number")))
        }
        _ => Ok(()),
    }
}

/// Load one task folder.
pub fn load_task(dir: &Path) -> Result<Task, TaskError> {
    let path = dir.join("task.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| task_err(&path, e.to_string()))?;
    let f: TaskFile = toml::from_str(&text).map_err(|e| task_err(&path, e.to_string()))?;
    let folder = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if f.id != folder {
        return Err(task_err(
            &path,
            format!("id `{}` does not match the folder name `{folder}`", f.id),
        ));
    }
    if f.prompt.trim().is_empty() {
        return Err(task_err(&path, "the prompt is empty"));
    }
    if !safe_relative(&f.circuit) || !f.circuit.to_ascii_lowercase().ends_with(".asc") {
        return Err(task_err(
            &path,
            format!(
                "circuit `{}` must be a relative .asc path inside the project",
                f.circuit
            ),
        ));
    }
    if !(1..=100).contains(&f.max_steps) {
        return Err(task_err(&path, "max_steps must be between 1 and 100"));
    }
    let specs = parse_specs(&f.specs).map_err(|e| task_err(&path, format!("specs: {e}")))?;
    if let Some(a) = &f.analysis
        && !a.trim_start().starts_with('.')
    {
        return Err(task_err(
            &path,
            format!("analysis `{a}` must be a directive such as `.ac dec 50 10 1Meg`"),
        ));
    }
    if let Some(set) = &f.corners {
        set.corners()
            .map_err(|e| task_err(&path, format!("corners: {e}")))?;
    }
    if let Some(kind) = &f.pdk
        && aispice_core::pdk::kit(kind).is_none()
    {
        let known: Vec<&str> = aispice_core::pdk::KITS.iter().map(|k| k.kind).collect();
        return Err(task_err(
            &path,
            format!(
                "pdk `{kind}` is not a kind of PDK aispice knows ({})",
                known.join(", ")
            ),
        ));
    }
    for c in &f.constraints {
        match c {
            Constraint::Value { equals, .. } => {
                check_number(&path, "value", &Some(equals.clone()))?
            }
            Constraint::ValueRange { min, max, .. } | Constraint::SubcktParam { min, max, .. } => {
                check_number(&path, "limit", min)?;
                check_number(&path, "limit", max)?;
            }
            Constraint::AnswerNumber { value, .. } => {
                check_number(&path, "value", &Some(value.clone()))?
            }
            Constraint::AnswerReports { pointer, .. } if !pointer.starts_with('/') => {
                return Err(task_err(
                    &path,
                    format!("pointer `{pointer}` must start with `/`"),
                ));
            }
            Constraint::PartCount { symbol, prefix, .. }
                if symbol.is_some() == prefix.is_some() =>
            {
                return Err(task_err(
                    &path,
                    "part_count needs either a symbol or a prefix",
                ));
            }
            Constraint::AnswerMentions { any } if any.is_empty() => {
                return Err(task_err(&path, "answer_mentions needs at least one phrase"));
            }
            _ => {}
        }
    }
    let task = Task {
        id: f.id,
        title: f.title,
        difficulty: f.difficulty,
        prompt: f.prompt.trim().to_string(),
        circuit: f.circuit,
        specs_text: f.specs,
        specs,
        analysis: f.analysis,
        corners: f.corners,
        pdk: f.pdk,
        max_steps: f.max_steps,
        lint: f.lint,
        constraints: f.constraints,
        dir: dir.to_path_buf(),
    };
    let files = task.start_files();
    if let Some(bad) = files.iter().find(|f| !safe_relative(f)) {
        return Err(task_err(dir, format!("unusable file name `{bad}`")));
    }
    // The runner writes the project file from `pdk`, naming whichever PDK
    // of that kind is registered where the suite runs.
    if files.iter().any(|f| f == crate::pdk::PROJECT_FILE) {
        return Err(task_err(
            dir,
            format!(
                "files/{} is written by the runner; name the kind of PDK with `pdk` in task.toml",
                crate::pdk::PROJECT_FILE
            ),
        ));
    }
    Ok(task)
}

/// Load every task folder in a suite (folders with a `task.toml`), easiest
/// first.
pub fn load_suite(dir: &Path) -> Result<Vec<Task>, TaskError> {
    let entries = std::fs::read_dir(dir).map_err(|e| task_err(dir, e.to_string()))?;
    let mut tasks = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() && p.join("task.toml").is_file() {
            tasks.push(load_task(&p)?);
        }
    }
    if tasks.is_empty() {
        return Err(task_err(dir, "no tasks (folders with a task.toml) found"));
    }
    tasks.sort_by(|a, b| a.difficulty.cmp(&b.difficulty).then(a.id.cmp(&b.id)));
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_task(dir: &Path, id: &str, body: &str) -> PathBuf {
        let d = dir.join(id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("task.toml"), body).unwrap();
        d
    }

    #[test]
    fn loads_a_task_with_constraints() {
        let tmp = tempfile::tempdir().unwrap();
        let d = write_task(
            tmp.path(),
            "rc",
            r#"
id = "rc"
difficulty = "easy"
prompt = "Make the corner 10 kHz."
circuit = "rc.asc"
specs = "fc = bandwidth_3db(V(out)) in 9.5k..10.5k"
analysis = ".ac dec 50 10 1Meg"
max_steps = 8
constraints = [
  { kind = "value", name = "C1", equals = "100n" },
  { kind = "e_series", prefix = "R", series = "E24" },
  { kind = "files_unchanged" },
  { kind = "answer_reports", tool = "monte_carlo", pointer = "/yield_pct", tol = 1.0 },
]
"#,
        );
        let t = load_task(&d).unwrap();
        assert_eq!(t.difficulty, Difficulty::Easy);
        assert_eq!(t.specs.len(), 1);
        assert_eq!(t.constraints.len(), 4);
        assert!(matches!(
            t.constraints[1],
            Constraint::ESeries {
                series: ESeries::E24,
                ..
            }
        ));
        assert!(t.start_files().is_empty());
    }

    #[test]
    fn rejects_bad_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let base = "prompt = \"x\"\nmax_steps = 5\n";
        for (id, extra, needle) in [
            ("a", "id = \"b\"\ncircuit = \"x.asc\"", "does not match"),
            (
                "c",
                "id = \"c\"\ncircuit = \"../x.asc\"",
                "inside the project",
            ),
            (
                "d",
                "id = \"d\"\ncircuit = \"/tmp/x.asc\"",
                "inside the project",
            ),
            (
                "e",
                "id = \"e\"\ncircuit = \"x.asc\"\nspecs = \"bad spec\"",
                "specs",
            ),
            (
                "f",
                "id = \"f\"\ncircuit = \"x.asc\"\nconstraints = [{ kind = \"nope\" }]",
                "unknown variant",
            ),
            (
                "g",
                "id = \"g\"\ncircuit = \"x.asc\"\nconstraints = [{ kind = \"value\", name = \"R1\", equals = \"lots\" }]",
                "not a number",
            ),
            (
                "h",
                "id = \"h\"\ncircuit = \"x.asc\"\nsurprise = 1",
                "unknown field",
            ),
            (
                "i",
                "id = \"i\"\ncircuit = \"x.asc\"\nconstraints = [{ kind = \"part_count\", min = 1 }]",
                "either a symbol or a prefix",
            ),
            (
                "j",
                "id = \"j\"\ncircuit = \"x.asc\"\npdk = \"acme-7\"",
                "pdk `acme-7` is not a kind of PDK aispice knows (ihp-sg13g2)",
            ),
        ] {
            let d = write_task(tmp.path(), id, &format!("{base}{extra}\n"));
            let e = load_task(&d).unwrap_err().to_string();
            assert!(e.contains(needle), "{id}: {e}");
        }
    }

    /// A task names the kind of PDK; the project file that opts into a
    /// registered one is the runner's to write.
    #[test]
    fn a_task_names_its_pdk_kind_not_a_project_file() {
        let tmp = tempfile::tempdir().unwrap();
        let body = "id = \"ic\"\nprompt = \"x\"\ncircuit = \"ota.asc\"\nmax_steps = 5\npdk = \"ihp-sg13g2\"\n";
        let d = write_task(tmp.path(), "ic", body);
        assert_eq!(load_task(&d).unwrap().pdk.as_deref(), Some("ihp-sg13g2"));
        std::fs::create_dir_all(d.join("files")).unwrap();
        std::fs::write(d.join("files/aispice.toml"), "pdk = \"mine\"\n").unwrap();
        let e = load_task(&d).unwrap_err().to_string();
        assert!(
            e.contains("files/aispice.toml is written by the runner"),
            "{e}"
        );
    }
}

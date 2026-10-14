//! KLayout and Magic run confined to their run's access. Gated on each
//! program; skipped where this machine has no sandbox, unless
//! `AISPICE_REQUIRE_SANDBOXES` says one is there (CI sets it).
//!
//! The test lays out a project as the tools do: an input layout, the
//! project's `.aispice` state folder, and a fresh run folder under
//! `.aispice/layout` for the results. A script of the test's own tries, from
//! inside the program, to write beside its input, in `.aispice` outside the
//! run folder, in the user's home and in the real /tmp, to read a file of
//! the project or of `.aispice` it was not given, to change the mode of the
//! input and to open a socket. The run may read its input file and write
//! its run folder only; the result stays `ok` only when every attempt
//! failed.

#![cfg(unix)]

use aispice_layout::tools::{self, Access, Programs, RunDir};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// A project for one confined run, and the marks the run must not leave.
struct Setup {
    _tmp: tempfile::TempDir,
    /// The run's input layout, the one file of the project it may read.
    victim: PathBuf,
    /// Files of the project and of its `.aispice` it may not read.
    decoys: Vec<PathBuf>,
    /// The run folder under `.aispice/layout`, the one it may write.
    out: PathBuf,
    marks: Vec<PathBuf>,
}

fn setup(tag: &str) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().canonicalize().unwrap();
    let state = project.join(".aispice");
    let out = state.join("layout/run-1");
    std::fs::create_dir_all(&out).unwrap();
    let victim = project.join("victim.gds");
    std::fs::write(&victim, "x").unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o640)).unwrap();
    let decoys = vec![project.join("notes.cir"), state.join("history.json")];
    for d in &decoys {
        std::fs::write(d, "SECRET").unwrap();
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()));
    let name = format!("aispice-{tag}-{}", std::process::id());
    let marks = vec![
        project.join(&name),
        home.join(&name),
        Path::new("/tmp").join(&name),
        state.join(&name),
    ];
    Setup {
        _tmp: tmp,
        victim,
        decoys,
        out,
        marks,
    }
}

fn decoys(s: &Setup) -> String {
    s.decoys
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join("|")
}

/// Whether to run: the program is installed and a sandbox is here (or must
/// be, which fails instead of skipping).
async fn gate(found: bool, what: &str) -> bool {
    if !found {
        eprintln!("skipped: {what} is not installed");
        return false;
    }
    if let Some(note) = tools::unsandboxed().await {
        let required = std::env::var("AISPICE_REQUIRE_SANDBOXES").unwrap_or_default();
        assert!(required.is_empty(), "a sandbox is required here: {note}");
        eprintln!("skipped: {note}");
        return false;
    }
    true
}

/// The result the script wrote, after checking that no mark was left and
/// the input's mode is unchanged. bubblewrap gives the program a /tmp of
/// its own and makes the folders above each path it binds on a root of its
/// own, so a write there (the real /tmp, a project folder or the home above
/// a bound input, wherever the test's temporary folder is) works inside
/// and leaves nothing outside, which the check of the marks shows; under
/// bubblewrap those lines are dropped.
async fn result(s: &Setup, log: &str) -> String {
    let private = tools::sandbox_description().await.starts_with("bubblewrap");
    let left: Vec<&PathBuf> = s.marks.iter().filter(|m| m.exists()).collect();
    for m in &left {
        let _ = std::fs::remove_file(m);
    }
    assert!(left.is_empty(), "wrote outside its folders: {left:?}");
    let mode = std::fs::metadata(&s.victim).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o640, "changed the input's mode");
    let text = std::fs::read_to_string(s.out.join("result"))
        .unwrap_or_else(|_| panic!("no result: {log}"));
    text.lines()
        .filter(|l| !(private && l.starts_with("wrote ")))
        .map(|l| format!("{l}\n"))
        .collect()
}

/// The same script run without the sandbox, with marks only beside the
/// input and in a private folder (never the user's home): every attempt
/// but the socket works there, so the confined run's `ok` comes from the
/// sandbox and not from a script that fails anyway.
fn unconfined_control(mut cmd: std::process::Command, s: &Setup) {
    let out = cmd.output().unwrap();
    let text = std::fs::read_to_string(s.out.join("result")).unwrap_or_else(|_| {
        panic!(
            "no result unconfined: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    for want in ["wrote", "read the decoy", "chmod"] {
        assert!(
            text.contains(want),
            "unconfined, `{want}` did not work: {text}"
        );
    }
}

/// A setup whose marks are in the test's own project only.
fn control_setup(tag: &str) -> Setup {
    let mut s = setup(tag);
    let project = s.victim.parent().unwrap().to_path_buf();
    s.marks = vec![project.join("mark"), project.join(".aispice/mark")];
    s
}

fn marks(s: &Setup) -> String {
    s.marks
        .iter()
        .map(|m| m.display().to_string())
        .collect::<Vec<_>>()
        .join("|")
}

const KLAYOUT_SCRIPT: &str = r#"import os, socket
res = ['ok']
for m in marks.split('|'):
    try:
        open(m, 'w').write('x')
        res.append('wrote ' + m)
    except Exception:
        pass
for d in decoys.split('|'):
    try:
        open(d).read()
        res.append('read the decoy ' + d)
    except Exception:
        pass
try:
    os.chmod(victim, 0o600)
    res.append('chmod')
except Exception:
    pass
try:
    socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    res.append('socket')
except Exception:
    pass
open(out + '/result', 'w').write('\n'.join(res) + '\n')
"#;

#[tokio::test]
async fn klayout_runs_confined() {
    let programs = Programs::default();
    if !gate(programs.klayout().is_ok(), "KLayout").await {
        return;
    }
    let s = setup("klayout");
    let run = RunDir::new().unwrap();
    let script = run.script("escape.py", KLAYOUT_SCRIPT).unwrap();
    let out = tools::klayout(
        &programs,
        &run,
        &script,
        &[
            ("marks", marks(&s)),
            ("decoys", decoys(&s)),
            ("victim", s.victim.display().to_string()),
            ("out", s.out.display().to_string()),
        ],
        &Access::new().read(&s.victim).write(&s.out),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result(&s, &out.combined()).await, "ok\n");

    let c = control_setup("klayout-control");
    let mut cmd = std::process::Command::new(programs.klayout().unwrap());
    cmd.current_dir(run.path())
        .env("HOME", run.path())
        .env("KLAYOUT_HOME", run.path())
        .env("QT_QPA_PLATFORM", "offscreen")
        .arg("-b")
        .arg("-r")
        .arg(&script);
    for (k, v) in [
        ("marks", marks(&c)),
        ("decoys", decoys(&c)),
        ("victim", c.victim.display().to_string()),
        ("out", c.out.display().to_string()),
    ] {
        cmd.arg("-rd").arg(format!("{k}={v}"));
    }
    unconfined_control(cmd, &c);
}

const MAGIC_SCRIPT: &str = "set res {ok}\n\
foreach m [split $env(AISPICE_MARKS) |] {\n\
    if {![catch {set f [open $m w]; puts $f x; close $f}]} { lappend res \"wrote $m\" }\n\
}\n\
foreach d [split $env(AISPICE_DECOYS) |] {\n\
    if {![catch {set f [open $d r]; read $f; close $f}]} { lappend res \"read the decoy $d\" }\n\
}\n\
if {![catch {file attributes $env(AISPICE_VICTIM) -permissions 0600}]} { lappend res chmod }\n\
if {![catch {socket localhost 9}]} { lappend res socket }\n\
set f [open $env(AISPICE_OUT)/result w]\n\
puts $f [join $res \\n]\n\
close $f\n\
quit -noprompt\n";

#[tokio::test]
async fn magic_runs_confined() {
    let programs = Programs::default();
    if !gate(programs.magic().is_ok(), "Magic").await {
        return;
    }
    let s = setup("magic");
    let run = RunDir::new().unwrap();
    let script = run.script("escape.tcl", MAGIC_SCRIPT).unwrap();
    let rc = run.script("empty.magicrc", "").unwrap();
    let out = tools::magic(
        &programs,
        &run,
        &rc,
        &script,
        &[
            ("AISPICE_MARKS", marks(&s)),
            ("AISPICE_DECOYS", decoys(&s)),
            ("AISPICE_VICTIM", s.victim.display().to_string()),
            ("AISPICE_OUT", s.out.display().to_string()),
        ],
        &Access::new().read(&s.victim).write(&s.out),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result(&s, &out.combined()).await, "ok\n");

    let c = control_setup("magic-control");
    let mut cmd = std::process::Command::new(programs.magic().unwrap());
    cmd.current_dir(run.path())
        .env("HOME", run.path())
        .env("AISPICE_MARKS", marks(&c))
        .env("AISPICE_DECOYS", decoys(&c))
        .env("AISPICE_VICTIM", &c.victim)
        .env("AISPICE_OUT", &c.out)
        .args(["-dnull", "-noconsole", "-rcfile"])
        .arg(&rc)
        .arg(&script)
        .stdin(std::process::Stdio::null());
    unconfined_control(cmd, &c);
}

/// A sandboxed run refuses, before KLayout starts, an output folder or an
/// input reached through a link or a `..`: the grant would be on the real
/// path while the program opens the written one.
#[tokio::test]
async fn paths_through_links_or_dot_dot_are_refused_before_the_run() {
    let programs = Programs::default();
    if !gate(programs.klayout().is_ok(), "KLayout").await {
        return;
    }
    let s = setup("links");
    let project = s.victim.parent().unwrap().to_path_buf();
    let outside = s.out.parent().unwrap().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, project.join("link")).unwrap();
    let dotdot = PathBuf::from(format!("{}/../run-1", s.out.display()));
    for access in [
        Access::new().read(&s.victim).write(project.join("link")),
        Access::new()
            .read(project.join("link/../victim.gds"))
            .write(&s.out),
        Access::new().read(&s.victim).write(&dotdot),
    ] {
        let run = RunDir::new().unwrap();
        let ran = run.path().join("ran");
        let script = run
            .script(
                "touch.py",
                &format!("open('{}', 'w').write('x')\n", ran.display()),
            )
            .unwrap();
        let err = tools::klayout(
            &programs,
            &run,
            &script,
            &[],
            &access,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("real path"), "{err}");
        assert!(!ran.exists(), "KLayout ran");
    }
}

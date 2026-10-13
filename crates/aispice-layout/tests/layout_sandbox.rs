//! KLayout and Magic run confined to their run's access. Gated on each
//! program; skipped where this machine has no sandbox, unless
//! `AISPICE_REQUIRE_SANDBOXES` says one is there (CI sets it).
//!
//! A script of the test's own tries, from inside the program, to write
//! beside its input, in the user's home and in the real /tmp, to read a
//! file it was not given, to change the mode of the input and to open a
//! socket. The result file it may write stays `ok` only when every attempt
//! failed.

#![cfg(unix)]

use aispice_layout::tools::{self, Access, Programs, RunDir};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Folders and files for one confined run, and the marks it must not leave.
struct Setup {
    _tmp: tempfile::TempDir,
    input: PathBuf,
    victim: PathBuf,
    decoy: PathBuf,
    out: PathBuf,
    marks: Vec<PathBuf>,
}

fn setup(tag: &str) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let input = dir.join("in");
    let out = dir.join("out");
    std::fs::create_dir_all(&input).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let victim = input.join("victim.gds");
    std::fs::write(&victim, "x").unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o640)).unwrap();
    let decoy = dir.join("decoy");
    std::fs::write(&decoy, "SECRET").unwrap();
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()));
    let name = format!("aispice-{tag}-{}", std::process::id());
    let marks = vec![
        input.join(&name),
        home.join(&name),
        Path::new("/tmp").join(&name),
    ];
    Setup {
        _tmp: tmp,
        input,
        victim,
        decoy,
        out,
        marks,
    }
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
/// its own, so a write there works inside and leaves nothing outside; that
/// line is dropped.
async fn result(s: &Setup, log: &str) -> String {
    let private_tmp = tools::sandbox_description().await.starts_with("bubblewrap");
    let left: Vec<&PathBuf> = s.marks.iter().filter(|m| m.exists()).collect();
    for m in &left {
        let _ = std::fs::remove_file(m);
    }
    assert!(left.is_empty(), "wrote outside its folders: {left:?}");
    let mode = std::fs::metadata(&s.victim).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o640, "changed the input's mode");
    let text = std::fs::read_to_string(s.out.join("result"))
        .unwrap_or_else(|_| panic!("no result: {log}"));
    let tmp_mark = format!("wrote {}", s.marks[2].display());
    text.lines()
        .filter(|l| !(private_tmp && *l == tmp_mark))
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

/// A setup whose marks are beside the input and in its own folder only.
fn control_setup(tag: &str) -> Setup {
    let mut s = setup(tag);
    let spare = s.out.parent().unwrap().join("spare");
    std::fs::create_dir_all(&spare).unwrap();
    s.marks = vec![s.input.join("mark"), spare.join("mark")];
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
try:
    open(decoy).read()
    res.append('read the decoy')
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
        run.path(),
        &script,
        &[
            ("marks", marks(&s)),
            ("decoy", s.decoy.display().to_string()),
            ("victim", s.victim.display().to_string()),
            ("out", s.out.display().to_string()),
        ],
        &Access::new().read(&s.input).write(&s.out),
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
        ("decoy", c.decoy.display().to_string()),
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
if {![catch {set f [open $env(AISPICE_DECOY) r]; read $f; close $f}]} { lappend res {read the decoy} }\n\
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
        run.path(),
        &rc,
        &script,
        &[
            ("AISPICE_MARKS", marks(&s)),
            ("AISPICE_DECOY", s.decoy.display().to_string()),
            ("AISPICE_VICTIM", s.victim.display().to_string()),
            ("AISPICE_OUT", s.out.display().to_string()),
        ],
        &Access::new().read(&s.input).write(&s.out),
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
        .env("AISPICE_DECOY", &c.decoy)
        .env("AISPICE_VICTIM", &c.victim)
        .env("AISPICE_OUT", &c.out)
        .args(["-dnull", "-noconsole", "-rcfile"])
        .arg(&rc)
        .arg(&script)
        .stdin(std::process::Stdio::null());
    unconfined_control(cmd, &c);
}

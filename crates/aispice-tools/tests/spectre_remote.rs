//! What a run on Spectre sends to the server, through the runner, with a
//! stand-in remote: `ssh` and `scp` replaced by scripts that run the remote
//! side in a local folder and keep a copy of every deck they receive. The
//! project's own files go in the deck; a file under a registered PDK, a
//! file or a model from the LTspice library never does, unless the user
//! said where the server has its own copy. Unix only (the stand-ins are
//! shell scripts).
#![cfg(unix)]

use aispice_sim::backend::spectre::SpectreConfig;
use aispice_tools::pdk::Registry;
use aispice_tools::{Project, RunMods, Runner, RunnerConfig};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// A text that must never reach the server.
const SECRET: &str = "LICENSED-DO-NOT-SEND";

/// One `.op` result, as Spectre writes it in nutascii.
const RAW: &str = "Title: t\nDate: today\nPlotname: DC Analysis `op1'\nFlags: real\nNo. Variables: 2\nNo. Points: 1\nVariables:\n\t0\tin\tV\n\t1\tout\tV\nValues:\n 0\t1\n\t0.5\n";

struct Remote {
    _dir: tempfile::TempDir,
    root: PathBuf,
    /// Every deck the server received.
    received: PathBuf,
    /// A registered PDK (a fake IHP) outside the project.
    pdk: PathBuf,
    /// A fake LTspice installation, whose library is outside the project.
    ltspice_lib: PathBuf,
    runner: Runner,
}

fn script(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A project, a stand-in server, a registered PDK and an LTspice library,
/// with `remote_folders` as given.
fn remote(remote_folders: BTreeMap<PathBuf, String>) -> Remote {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let root = base.join("project");
    let server = base.join("server");
    let received = base.join("received");
    for d in [&root, &server, &received] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(base.join("op.raw"), RAW).unwrap();

    // The remote command runs here, in a shell, as ssh would run it there.
    let ssh = base.join("ssh");
    script(
        &ssh,
        "#!/bin/sh\nfor a; do last=$a; done\nexec /bin/sh -c \"$last\"\n",
    );
    // Copies with the host prefix taken off; a missing file is skipped.
    let scp = base.join("scp");
    script(
        &scp,
        "#!/bin/sh\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift\nfor a; do dst=$a; done\nn=$#\ni=0\nfor a; do i=$((i+1)); [ $i -eq $n ] && break; src=${a#stand-in:}; cp -r \"$src\" \"${dst#stand-in:}\" 2>/dev/null; done\nexit 0\n",
    );
    let mut cfg = SpectreConfig::new("stand-in", server.to_str().unwrap());
    cfg.command = format!(
        "cd {{dir}} && cp {{deck}} {} && cp {} {{raw}}",
        received.display(),
        base.join("op.raw").display()
    );
    cfg.ssh = Some(ssh);
    cfg.scp = Some(scp);
    cfg.remote_folders = remote_folders;

    aispice_sim::pdk::fake_ihp(&base);
    let pdk = base.join("IHP-Open-PDK/ihp-sg13g2");
    std::fs::write(
        pdk.join("libs.tech/ngspice/models/vendor.inc"),
        format!("* {SECRET}\n.param pdkval=2k\n"),
    )
    .unwrap();
    let mut pdks = Registry::default();
    pdks.add("ihp", &pdk).unwrap();

    let exe = base.join("ltspice/LTspice.exe");
    let ltspice_lib = base.join("ltspice/lib");
    std::fs::create_dir_all(ltspice_lib.join("sub")).unwrap();
    std::fs::create_dir_all(ltspice_lib.join("sym")).unwrap();
    std::fs::write(&exe, "not really").unwrap();
    std::fs::write(
        ltspice_lib.join("sub/vendoramp.sub"),
        format!("* {SECRET}\n.subckt vendoramp a b c\nR1 a c 1k\n.ends vendoramp\n"),
    )
    .unwrap();

    let runner = Runner::new(RunnerConfig {
        spectre: Some(cfg),
        pdks,
        ltspice_exe: Some(exe),
        ..RunnerConfig::default()
    });
    Remote {
        _dir: dir,
        root,
        received,
        pdk,
        ltspice_lib,
        runner,
    }
}

impl Remote {
    async fn run(&self, deck: &str) -> Result<std::sync::Arc<aispice_tools::StoredRun>, String> {
        std::fs::write(self.root.join("t.cir"), deck).unwrap();
        let project = Project::open(&self.root, Default::default()).unwrap();
        self.runner
            .run_circuit(
                &project,
                "t.cir",
                Some("spectre"),
                &RunMods::default(),
                &CancellationToken::new(),
            )
            .await
            .map_err(|e| e.to_string())
    }

    /// Everything the server received, decks and the files beside them.
    fn sent(&self) -> String {
        let mut all = String::new();
        for e in std::fs::read_dir(&self.received).unwrap() {
            all.push_str(&std::fs::read_to_string(e.unwrap().path()).unwrap());
        }
        all
    }
}

#[tokio::test]
async fn project_files_go_in_the_deck_and_are_named() {
    let r = remote(BTreeMap::new());
    std::fs::write(
        r.root.join("models.inc"),
        "* project models\n.param rval=1k\n",
    )
    .unwrap();
    let run = r
        .run("t\nV1 in 0 1\nR1 in out {rval}\nR2 out 0 1k\n.include models.inc\n.op\n")
        .await
        .unwrap();
    let sent = r.sent();
    assert!(sent.contains(".param rval=1k"), "{sent}");
    assert!(sent.contains("R1 in out 'rval'"), "{sent}");
    assert!(!sent.contains(".include"), "{sent}");
    assert!(
        run.notes
            .iter()
            .any(|n| n == "sent to the Spectre server inside the deck: models.inc"),
        "{:?}",
        run.notes
    );
    assert_eq!(run.deck, sent, "the stored deck is the one sent");
}

#[tokio::test]
async fn a_file_under_a_registered_pdk_is_never_sent() {
    let r = remote(BTreeMap::new());
    let vendor = r.pdk.join("libs.tech/ngspice/models/vendor.inc");
    let err = r
        .run(&format!(
            "t\nV1 in 0 1\nR1 in out {{pdkval}}\nR2 out 0 1k\n.include \"{}\"\n.op\n",
            vendor.display()
        ))
        .await
        .unwrap_err();
    assert!(
        err.contains(&format!("`{}` is outside the project", vendor.display())),
        "{err}"
    );
    assert!(
        err.contains("remote_folders") && !err.contains(SECRET),
        "{err}"
    );
    assert!(r.sent().is_empty(), "nothing reached the server");
}

#[tokio::test]
async fn a_file_from_the_ltspice_library_is_never_sent() {
    let r = remote(BTreeMap::new());
    let sub = r.ltspice_lib.join("sub/vendoramp.sub");
    let err = r
        .run(&format!(
            "t\nV1 in 0 1\nR1 in out 1k\nR2 out 0 1k\n.include \"{}\"\n.op\n",
            sub.display()
        ))
        .await
        .unwrap_err();
    assert!(
        err.contains("is outside the project") && !err.contains(SECRET),
        "{err}"
    );
    assert!(r.sent().is_empty());
}

#[tokio::test]
async fn a_model_from_the_ltspice_library_is_never_sent() {
    let r = remote(BTreeMap::new());
    let err = r
        .run("t\nV1 in 0 1\nX1 in out 0 vendoramp\nR2 out 0 1k\n.op\n")
        .await
        .unwrap_err();
    assert!(
        err.contains("vendoramp would come from your LTspice library"),
        "{err}"
    );
    assert!(!err.contains(SECRET), "{err}");
    assert!(r.sent().is_empty());
}

/// With the server's copy of the PDK configured, the include names that
/// copy and the file itself still stays here.
#[tokio::test]
async fn a_configured_server_copy_is_named_instead() {
    let mut r = remote(BTreeMap::new());
    let mut cfg = r.runner.config();
    cfg.spectre
        .as_mut()
        .unwrap()
        .remote_folders
        .insert(r.pdk.clone(), "/cad/ihp-sg13g2".into());
    r.runner = Runner::new(cfg);
    let run = r
        .run(&format!(
            "t\nV1 in 0 1\nR1 in out {{pdkval}}\nR2 out 0 1k\n.include \"{}\"\n.op\n",
            r.pdk.join("libs.tech/ngspice/models/vendor.inc").display()
        ))
        .await;
    let sent = r.sent();
    assert!(
        sent.contains(".include \"/cad/ihp-sg13g2/libs.tech/ngspice/models/vendor.inc\""),
        "{sent} {run:?}"
    );
    assert!(
        !sent.contains(SECRET) && !sent.contains(r.pdk.to_str().unwrap()),
        "{sent}"
    );
    let run = run.unwrap();
    assert!(
        run.notes.iter().any(|n| n.ends_with(
            "is read on the Spectre server as /cad/ihp-sg13g2/libs.tech/ngspice/models/vendor.inc"
        )),
        "{:?}",
        run.notes
    );
}

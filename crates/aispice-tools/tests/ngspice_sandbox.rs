//! An ngspice run that loads none of the project's code goes through the
//! runner into the sandbox where one can start. A project deck includes a
//! file that includes another through a symbolic link inside the project:
//! under bubblewrap, which shows each file the deck reads at its real path
//! only, the run is refused before ngspice starts, naming the link; under
//! Landlock ngspice follows the link to the file it was granted; and where
//! no sandbox can start, the run goes unconfined and its output says so.

#![cfg(unix)]

use aispice_sim::backend::{native_code, which};
use aispice_tools::runner::RunError;
use aispice_tools::{Project, RunMods, Runner, RunnerConfig};
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
async fn a_run_without_the_projects_code_goes_in_the_sandbox() {
    if which("ngspice").is_none() {
        eprintln!("skipped: ngspice is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("proj");
    std::fs::create_dir_all(root.join("models/sub")).unwrap();
    std::fs::create_dir_all(root.join("shared")).unwrap();
    std::fs::write(root.join("shared/vref.inc"), "V2 b 0 2\nR2 b 0 1k\n").unwrap();
    std::os::unix::fs::symlink("../../shared/vref.inc", root.join("models/sub/link.inc")).unwrap();
    std::fs::write(
        root.join("models/bias.inc"),
        "* bias\n.include sub/link.inc\n",
    )
    .unwrap();
    std::fs::write(
        root.join("top.cir"),
        "* plain\nV1 a 0 1\nR1 a 0 1k\n.include models/bias.inc\n.op\n.end\n",
    )
    .unwrap();
    let runner = Runner::new(RunnerConfig {
        embedded_models_only: true,
        ..RunnerConfig::default()
    });
    let project = Project::open(&root, Default::default()).unwrap();
    let result = runner
        .run_circuit(
            &project,
            "top.cir",
            Some("ngspice"),
            &RunMods::default(),
            &CancellationToken::new(),
        )
        .await;
    let read_b = |run: &aispice_tools::StoredRun| {
        run.output.datasets.iter().any(|d| d.vector("b").is_some())
    };
    match native_code::plain_sandbox().await.unwrap() {
        Ok(sandbox) if sandbox.is_bwrap() => {
            let e = result.unwrap_err();
            assert!(matches!(e, RunError::Netlist(_)), "{e:?}");
            let e = e.to_string();
            assert!(
                e.contains("the include `sub/link.inc`")
                    && e.contains("in the bubblewrap sandbox it runs in"),
                "{e}"
            );
        }
        Ok(sandbox) => {
            let what = sandbox.describe();
            let run = result.unwrap_or_else(|e| panic!("{what}: {e}"));
            assert!(read_b(&run), "{what}: {}", run.output.log);
            assert!(
                !run.output
                    .warnings
                    .iter()
                    .any(|w| w.contains("without a sandbox")),
                "{what}: {:?}",
                run.output.warnings
            );
        }
        Err(why) => {
            assert!(
                std::env::var_os("AISPICE_REQUIRE_SANDBOXES").is_none(),
                "a sandbox is required here: {why}"
            );
            let run = result.unwrap();
            assert!(read_b(&run), "{}", run.output.log);
            assert_eq!(
                run.output.warnings.first(),
                Some(&native_code::unconfined_warning(&why))
            );
        }
    }
}

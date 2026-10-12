//! The XSPICE models that load or run native code, against ngspice itself:
//! every spelling the netlist policy refuses is run through ngspice
//! unchecked, in both modes aispice runs it in, to show what ngspice would
//! do with it. A `d_cosim` card names a library to load and a `d_process`
//! card a program to start; here they name files that do not exist, so
//! ngspice can only report that it tried. Spellings ngspice 42 does not read
//! as the model are refused all the same, so a later ngspice that does
//! changes nothing. Skips when ngspice is missing.

use aispice_core::netlist::{Policy, check_policy};

/// How ngspice 42 reads a card: it loads the library or starts the program,
/// or it does not see the model at all.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ngspice42 {
    Loads,
    Starts,
    NoModel,
}

fn deck(card: &str, process: bool) -> String {
    // d_cosim takes inputs, outputs and inouts; d_process inputs, a clock,
    // a reset and outputs.
    let inst = if process {
        "A1 [ad] ad null [q] cnt"
    } else {
        "A1 [ad] [q] null cnt"
    };
    format!(
        "xspice probe\nVin a 0 PULSE(0 1 0 1n 1n 1u 2u)\nAbr [a] [ad] tobit\n.model tobit adc_bridge(in_low=0.4 in_high=0.6)\n{inst}\n{card}\n.tran 1u 10u\n.end\n"
    )
}

/// ngspice's output for `deck`, run as the backend runs it (batch, a raw
/// file, a private folder that is also `HOME`), in LTspice compatibility
/// mode or native mode. Run directly rather than through the backend: a
/// `d_process` failure makes ngspice exit mid-run, and the output is what
/// this test reads.
fn ngspice_says(exe: &std::path::Path, deck: &str, ltpsa: bool) -> String {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("probe.cir"), deck).unwrap();
    if ltpsa {
        std::fs::write(dir.path().join(".spiceinit"), "set ngbehavior=ltpsa\n").unwrap();
    }
    let out = std::process::Command::new(exe)
        .args(["-b", "-r", "probe.raw", "probe.cir"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[tokio::test]
async fn ngspice_would_load_or_run_what_the_policy_refuses() {
    let Some(exe) = aispice_sim::backend::which("ngspice") else {
        eprintln!("skipped: ngspice not found");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent");
    let lib = absent.join("probe.so");
    let prog = absent.join("probe-prog");
    let (lib, prog) = (lib.display().to_string(), prog.display().to_string());
    let cases = [
        (
            format!(".model cnt d_cosim simulation=\"{lib}\""),
            Ngspice42::Loads,
        ),
        (
            format!(".model cnt d_cosim,simulation=\"{lib}\""),
            Ngspice42::Loads,
        ),
        (
            format!(".model cnt (d_cosim simulation=\"{lib}\")"),
            Ngspice42::Loads,
        ),
        (
            format!(".model cnt d_cosim(simulation=\"{lib}\")"),
            Ngspice42::Loads,
        ),
        (
            format!(".model\tcnt\td_cosim\tsimulation = \"{lib}\""),
            Ngspice42::Loads,
        ),
        (
            format!(".model cnt\n+ d_cosim simulation=\"{lib}\""),
            Ngspice42::Loads,
        ),
        (
            format!(".model,cnt d_cosim simulation=\"{lib}\""),
            Ngspice42::NoModel,
        ),
        (
            format!(".model cnt=d_cosim simulation=\"{lib}\""),
            Ngspice42::NoModel,
        ),
        (
            format!(".model cnt d_process process_file=\"{prog}\""),
            Ngspice42::Starts,
        ),
        (
            format!(".model cnt d_process,process_file=\"{prog}\""),
            Ngspice42::Starts,
        ),
        (
            format!(".model cnt (d_process process_file=\"{prog}\")"),
            Ngspice42::Starts,
        ),
    ];
    let version = std::process::Command::new(&exe)
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let v42 = version.contains("ngspice-42");
    let policy = Policy {
        base_dir: dir.path().to_path_buf(),
        allowed_dirs: vec![dir.path().to_path_buf()],
        allow_control_blocks: false,
        trusted_dirs: Vec::new(),
    };
    for (card, expected) in &cases {
        let process = card.contains("d_process");
        let text = deck(card, process);
        let refused = check_policy(&text, &policy);
        assert!(
            refused.iter().any(|v| v.reason.contains("native code")),
            "the policy must refuse {card:?}: {refused:?}"
        );
        for (mode, ltpsa) in [("ltpsa", true), ("native", false)] {
            let log = ngspice_says(&exe, &text, ltpsa);
            // d_cosim says so before the loader's own message, which
            // differs between systems.
            let loaded = log
                .to_ascii_lowercase()
                .contains("failed to load simulation binary");
            let started = log.contains("failed to fork or start process");
            let no_model = log.contains("unable to find definition of model");
            let seen = match (loaded, started, no_model) {
                (true, _, _) => Ngspice42::Loads,
                (_, true, _) => Ngspice42::Starts,
                (_, _, true) => Ngspice42::NoModel,
                _ if *expected == Ngspice42::NoModel && !v42 => continue,
                _ => panic!("{mode} {card:?}: ngspice said neither\n{log}"),
            };
            eprintln!("{mode}: {card:?} -> {seen:?}");
            // What another release does with the spellings ngspice 42 does
            // not read is recorded, not asserted: they are refused anyway.
            if v42 || *expected != Ngspice42::NoModel {
                assert_eq!(seen, *expected, "{mode} {card:?}\n{log}");
            }
        }
    }
}

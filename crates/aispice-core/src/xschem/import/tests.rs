use super::*;
use crate::xschem::file;

fn builtins() -> Resolver {
    Resolver::new(Vec::new(), Vec::new())
}

fn run(sch: &str) -> Imported {
    let f = file::parse(sch).unwrap();
    import(
        Path::new("/w/tb.sch"),
        &f,
        &builtins(),
        &SymbolLibrary::builtin_only(),
        &ImportOptions::default(),
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

const RC: &str = r#"v {xschem version=3.4.4 file_version=1.2}
N 100 -200 200 -200 {lab=in}
N 260 -200 300 -200 {lab=out}
N 300 -200 300 -160 {lab=out}
N 100 -200 100 -170 {lab=in}
C {devices/vsource.sym} 100 -140 0 0 {name=V1 value="dc 0 ac 1"}
C {devices/res.sym} 230 -200 1 0 {name=R1 value=1k}
C {devices/capa.sym} 300 -130 0 0 {name=C1 value=1n}
C {devices/gnd.sym} 100 -110 0 0 {name=l1 lab=GND}
C {devices/gnd.sym} 300 -100 0 0 {name=l2 lab=GND}
C {devices/lab_pin.sym} 300 -200 0 0 {name=p1 lab=out}
C {devices/lab_pin.sym} 100 -200 0 0 {name=p2 lab=in}
C {devices/code_shown.sym} 400 -200 0 0 {name=s1 only_toplevel=false value=".ac dec 10 1 1meg"}
"#;

#[test]
fn rc_imports_and_proves_equal() {
    let r = run(RC);
    assert!(r.proof.equivalent, "{:?}", r.proof);
    assert!(r.findings.is_empty(), "{:?}", r.findings);
    let text = crate::netlist::write(&r.asc_netlist);
    assert!(text.contains("R1 out in 1k\n"), "{text}");
    assert!(text.contains("C1 out 0 1n\n"), "{text}");
    assert!(text.contains(".ac dec 10 1 1meg"), "{text}");
    // The drawing labels the named nets and grounds the ground.
    let labels: Vec<_> = r.schematic.flags().map(|f| f.label.clone()).collect();
    assert!(labels.contains(&"0".to_string()), "{labels:?}");
    assert!(r.notes.iter().any(|n| n.contains("GND")));
}

const BENCH: &str = r#"v {xschem version=3.4.4 file_version=1.2}
N 20 0 20 30 {}
N -20 0 -60 0 {}
C {sg13g2_pr/sg13_lv_nmos.sym} 0 0 0 0 {name=M1
l=0.45u
w=1.0u
ng=1
m=1
model=sg13_lv_nmos
spiceprefix=X
}
C {devices/vsource.sym} -60 30 0 0 {name=Vg value=0.8}
C {devices/ammeter.sym} 20 -60 0 0 {name=Vd}
C {devices/vsource.sym} 20 -120 0 0 {name=Vds value=1.2}
C {devices/gnd.sym} 20 30 0 0 {name=l1 lab=GND}
C {devices/gnd.sym} -60 60 0 0 {name=l2 lab=GND}
C {devices/gnd.sym} 20 -90 2 0 {name=l3 lab=GND}
C {devices/code_shown.sym} 0 100 0 0 {name=MODEL only_toplevel=true
format="tcleval( @value )"
value=".lib cornerMOSlv.lib mos_ff
"}
C {devices/code_shown.sym} 0 200 0 0 {name=NGSPICE only_toplevel=true
value="
.include tb.save
.param temp=27
.include /etc/passwd
.control
save all
op
dc Vds 0 1.2 0.01
write tb.raw
.endc
"}
"#;

#[test]
fn test_bench_drops_scripts_and_headers_with_findings() {
    let r = run(BENCH);
    assert!(r.proof.equivalent, "{:?}", r.proof);
    let has = |subject: &str, text: &str| {
        r.findings
            .iter()
            .any(|f| f.subject == subject && f.message.contains(text))
    };
    assert!(has(".control", "`.op`"), "{:?}", r.findings);
    assert!(has(".control", "`.dc Vds 0 1.2 0.01`"), "{:?}", r.findings);
    assert!(has(".include", "tb.save is not beside"), "{:?}", r.findings);
    assert!(has(".include", "is dropped"), "{:?}", r.findings);
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("mos_ff") && n.contains("mos_tt")),
        "{:?}",
        r.notes
    );
    // The transistor is drawn with the PDK symbol.
    assert!(
        r.schematic
            .symbols()
            .any(|s| s.name.eq_ignore_ascii_case("sg13g2\\sg13_lv_nmos")),
        "{:?}",
        r.schematic
            .symbols()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>()
    );
    let text = crate::netlist::write(&r.asc_netlist);
    assert!(
        text.contains("sg13_lv_nmos w=1.0u l=0.45u ng=1 m=1"),
        "{text}"
    );
    assert!(!text.to_ascii_lowercase().contains(".control"), "{text}");
    assert!(!text.contains("cornerMOSlv"), "{text}");
    assert!(text.contains(".save i(vd)"), "{text}");
}

#[test]
fn unknown_parts_are_findings_and_kept_as_lines() {
    let sch = r#"v {xschem version=3.4.4 file_version=1.2}
C {mystery.sym} 0 0 0 0 {name=X1}
C {sg13g2_pr/rppd.sym} 100 0 0 0 {name=R1 w=0.5e-6 l=2e-6 model=rppd body=sub! spiceprefix=X b=0 m=1}
C {devices/vsource.sym} 200 0 0 0 {name=V1 value=1}
C {devices/lab_pin.sym} 100 -30 0 0 {name=p1 lab=a}
C {devices/lab_pin.sym} 200 -30 0 0 {name=p2 lab=a}
C {devices/gnd.sym} 100 30 0 0 {name=l1 lab=GND}
C {devices/gnd.sym} 200 30 0 0 {name=l2 lab=GND}
C {sg13g2_pr/sub.sym} 300 0 0 0 {name=l3 lab=sub!}
C {devices/vsource.sym} 300 30 0 0 {name=Vsub value=0}
C {devices/gnd.sym} 300 60 0 0 {name=l4 lab=GND}
"#;
    let r = run(sch);
    assert!(r.proof.equivalent, "{:?}", r.proof);
    assert!(
        r.findings.iter().any(|f| f.subject == "X1"),
        "{:?}",
        r.findings
    );
    let text = crate::netlist::write(&r.asc_netlist);
    // The resistor's body is the global substrate net.
    assert!(
        text.contains("XR1 a 0 sub! rppd w=0.5e-6 l=2e-6 m=1 b=0"),
        "{text}"
    );
}

#[test]
fn clean_handles_unterminated_blocks_and_globals() {
    let c = clean(
        "** title\nR1 a 0 1k\n.GLOBAL GND\n.control\nop\n.end\n",
        None,
    );
    assert_eq!(c.text, "** title\nR1 a 0 1k\n");
    assert!(c.findings[0].message.contains("no .endc"));
    assert!(c.notes.iter().any(|n| n.contains(".GLOBAL")));
}

#[test]
fn renames_keep_nets_directives_name() {
    let mut deck = crate::netlist::parse(
        "t\nR1 net1 GND 1k m=1\nR2 net1 net2 1k m=2\nC1 net2 gnd 1p m=1\n.save v(net2)\n",
    );
    rename_for_ltspice(&mut deck);
    let text = crate::netlist::write(&deck);
    assert!(text.contains("R1 N001 0 1k\n"), "{text}");
    assert!(text.contains("R2 N001 net2 1k m=2\n"), "{text}");
    assert!(text.contains("C1 net2 0 1p\n"), "{text}");
}

/// Gated on `AISPICE_PDK_IHP` and an installed xschem: every IHP test bench
/// imports, the `.asc` netlists to the same circuit as the `.sch`, and no
/// source value carries text after it.
#[test]
fn ihp_test_benches_import() {
    let Some(pdk) = std::env::var_os("AISPICE_PDK_IHP").map(PathBuf::from) else {
        return;
    };
    let Some(share) = crate::xschem::installed_library() else {
        eprintln!("skipped: xschem is not installed");
        return;
    };
    let share = share.unwrap_or_else(|e| panic!("{e}"));
    let xschem = pdk.join("libs.tech/xschem");
    let r = Resolver::new(
        vec![share.join("devices"), share.clone(), xschem.clone()],
        vec![xschem.clone()],
    );
    let models = pdk.join("libs.tech/ngspice/models").display().to_string();
    let opts = ImportOptions {
        netlist: netlist::Options {
            env: crate::xschem::tcl::Env::new("ngspice")
                .with_var("MODELS_NGSPICE", &models)
                .with_var("SG13G2_MODELS", &models),
            ..Default::default()
        },
        layout: crate::layout::LayoutOptions {
            effort: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    let lib = SymbolLibrary::builtin_only();
    let mut problems = Vec::new();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(xschem.join("sg13g2_tests"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "sch"))
        .collect();
    paths.sort();
    for p in paths {
        let text = crate::xschem::symbol::read_limited(&p).unwrap();
        let f = file::parse(&text).unwrap();
        match import(&p, &f, &r, &lib, &opts) {
            Ok(i) => {
                if !i.proof.equivalent {
                    problems.push(format!("{}: {:?}", p.display(), i.proof.differences));
                }
                for bad in trailing_text(&i.asc_netlist) {
                    problems.push(format!("{}: source value {bad}", p.display()));
                }
            }
            Err(e) => problems.push(format!("{}: {e}", p.display())),
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Source lines (`V`, `I`, in subcircuits too) with a number that has text
/// after its scale and unit, as `0.75false` had when a format's
/// `@savecurrent` was read as the attribute's text. ngspice reads that one as
/// 0.75 f, so it would have simulated a different circuit without a word.
fn trailing_text(net: &crate::netlist::Netlist) -> Vec<String> {
    use crate::netlist::Line;
    fn walk(lines: &[Line], bad: &mut Vec<String>) {
        for line in lines {
            match line {
                Line::Element(e) if matches!(e.letter(), 'V' | 'I') => {
                    let words = e
                        .rest
                        .split(|c: char| c.is_whitespace() || "(),=".contains(c))
                        .filter(|w| !w.is_empty());
                    for w in words {
                        let lower = w.to_ascii_lowercase();
                        let numeric = lower
                            .trim_start_matches(['+', '-'])
                            .starts_with(|c: char| c.is_ascii_digit() || c == '.');
                        if lower.ends_with("true")
                            || lower.ends_with("false")
                            || (numeric && !plain_number(&lower))
                        {
                            bad.push(format!("{} `{}`", e.name, e.rest));
                            break;
                        }
                    }
                }
                Line::Subckt(s) => walk(&s.body, bad),
                _ => {}
            }
        }
    }
    let mut bad = Vec::new();
    walk(&net.items, &mut bad);
    bad
}

/// A SPICE number with nothing after it but a scale and a unit: `0.75`,
/// `-1.5e-3`, `10u`, `2meg`, `1.2v`, `5ma`, `100ns`.
fn plain_number(w: &str) -> bool {
    let w = w.trim_start_matches(['+', '-']);
    let mantissa = w
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(w.len());
    let mut rest = &w[mantissa..];
    if let Some(exp) = rest.strip_prefix('e') {
        let exp = exp.trim_start_matches(['+', '-']);
        let digits = exp.find(|c: char| !c.is_ascii_digit()).unwrap_or(exp.len());
        if digits == 0 {
            return false;
        }
        rest = &exp[digits..];
    }
    for scale in ["meg", "mil", "t", "g", "k", "m", "u", "n", "p", "f", ""] {
        if let Some(unit) = rest.strip_prefix(scale)
            && ["", "v", "a", "s", "hz"].contains(&unit)
        {
            return true;
        }
    }
    false
}

/// The check catches the old bug and passes what sources really hold.
#[test]
fn trailing_text_is_found() {
    for good in [
        "0.75", "-1.5e-3", "10u", "2Meg", "1.2V", "5mA", "100ns", "1e3", ".5",
    ] {
        assert!(plain_number(&good.to_ascii_lowercase()), "{good}");
    }
    for bad in ["0.75false", "1true", "1.2vv", "3x", "1e", "1kq"] {
        assert!(!plain_number(bad), "{bad}");
    }
    let net = crate::netlist::parse(
        "t\nV1 a 0 0.75false\nV2 b 0 dc 1 ac 1\nI1 c 0 SINE(0 1 1k)\nV3 d 0 pwl 0 0 1n 1.2\nV4 e 0 dc {vdd}\nV5 f 0 SINE(0 1 1k)true\n.end\n",
    );
    let found = trailing_text(&net);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].starts_with("V1 "), "{found:?}");
    assert!(found[1].starts_with("V5 "), "{found:?}");
}

/// A source value never carries the `savecurrent` flag or any other text
/// after it, with xschem 3.4.4's symbols (the built-ins, whose format reads
/// the flag in Tcl) and 3.4.8's (whose format has the `@savecurrent`
/// token).
#[test]
fn source_values_have_no_trailing_text() {
    let sch = r#"v {xschem version=3.4.8RC file_version=1.2}
C {devices/vsource.sym} 0 0 0 0 {name=V1 value=0.75}
C {devices/vsource.sym} 100 0 0 0 {name=V2 value=1.5 savecurrent=true}
C {devices/ammeter.sym} 200 0 0 0 {name=Vm}
C {devices/res.sym} 300 0 0 0 {name=R1 value=1k}
C {devices/gnd.sym} 0 30 0 0 {name=l1 lab=GND}
C {devices/gnd.sym} 100 30 0 0 {name=l2 lab=GND}
C {devices/lab_pin.sym} 300 -30 0 0 {name=p1 lab=b}
C {devices/gnd.sym} 300 30 0 0 {name=l3 lab=GND}
"#;
    let f = file::parse(sch).unwrap();
    let (_tmp, later) = crate::xschem::later_library();
    for r in [builtins(), later] {
        let i = import(
            Path::new("/w/tb.sch"),
            &f,
            &r,
            &SymbolLibrary::builtin_only(),
            &ImportOptions::default(),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            trailing_text(&i.asc_netlist).is_empty(),
            "{:?}",
            i.asc_netlist
        );
        assert!(trailing_text(&i.deck).is_empty(), "{:?}", i.deck);
        let net = &i.asc_netlist;
        assert_eq!(net.element("V1").map(|e| e.rest.as_str()), Some("0.75"));
        assert_eq!(net.element("V2").map(|e| e.rest.as_str()), Some("1.5"));
        assert_eq!(net.element("Vm").map(|e| e.rest.as_str()), Some("0"));
    }
}

/// The whole deck is checked by the netlist policy before anything is
/// drawn or written.
#[test]
fn the_policy_gates_the_whole_deck() {
    for bad in [
        "t\n.include /etc/passwd\n",
        "t\n.control\nshell ls\n.endc\n",
        "t\nR1 a 0 1k\n+ cmd=x\n",
    ] {
        let deck = crate::netlist::parse(bad);
        assert!(
            matches!(policy_gate(&deck), Err(ImportError::Policy(..))),
            "{bad}"
        );
    }
    assert_eq!(
        policy_gate(&crate::netlist::parse("t\nR1 a 0 1k\n.op\n")),
        Ok(())
    );
    // Code blocks that try anyway are dropped line by line, and what is
    // left passes as a whole.
    let r = run(
        "C {devices/res.sym} 0 0 0 0 {name=R1 value=1k}\nC {devices/gnd.sym} 0 30 0 0 {name=l1 lab=GND}\nC {devices/lab_pin.sym} 0 -30 0 0 {name=p1 lab=a}\nC {devices/code_shown.sym} 100 0 0 0 {name=s1 value=\".include /etc/passwd\n.include ../../x.lib\nB1 a 0 V=1 cmd=x\n.op\"}\n",
    );
    assert!(
        check_lexical_deck(&crate::netlist::write(&r.asc_netlist)).is_empty(),
        "{}",
        crate::netlist::write(&r.asc_netlist)
    );
    assert!(r.findings.len() >= 3, "{:?}", r.findings);
}

/// A path in the schematic never makes the import read a file outside the
/// project and the library folders.
#[test]
fn files_outside_the_project_are_never_read() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        root.join("outside.sch"),
        "C {devices/res.sym} 0 0 0 0 {name=RSECRET value=42}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("outside.sym"),
        "K {type=resistor format=\"@name @pinlist SECRET\"}\nB 5 0 0 1 1 {name=P}\n",
    )
    .unwrap();
    std::fs::write(
        project.join("blk.sym"),
        "K {type=subcircuit\nformat=\"@name @pinlist @symname\"\ntemplate=\"name=x1\"\nschematic=../outside.sch}\nB 5 -2.5 -2.5 2.5 2.5 {name=p}\n",
    )
    .unwrap();
    let outside = root.join("outside.sym").display().to_string();
    let sch = format!(
        "C {{blk.sym}} 0 0 0 0 {{name=x1}}\nC {{../outside.sym}} 100 0 0 0 {{name=R2}}\nC {{{outside}}} 200 0 0 0 {{name=R3}}\nC {{devices/res.sym}} 0 100 0 0 {{name=R1 value=1k}}\nC {{devices/gnd.sym}} 0 130 0 0 {{name=l1 lab=GND}}\nC {{devices/lab_pin.sym}} 0 70 0 0 {{name=p1 lab=a}}\n"
    );
    let f = file::parse(&sch).unwrap();
    let r = Resolver::new(Vec::new(), vec![project.clone()]);
    let out = import(
        &project.join("tb.sch"),
        &f,
        &r,
        &SymbolLibrary::builtin_only(),
        &ImportOptions::default(),
    )
    .unwrap();
    assert!(!out.xschem.text.contains("SECRET"), "{}", out.xschem.text);
    assert!(
        out.findings
            .iter()
            .filter(|f| f.message.contains("outside"))
            .count()
            >= 2,
        "{:?}",
        out.findings
    );
}

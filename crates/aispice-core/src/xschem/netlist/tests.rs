use super::*;
use crate::netlist::parse;

/// A resolver that only knows aispice's built-in symbols.
fn builtins() -> Resolver {
    Resolver {
        library: Vec::new(),
        roots: Vec::new(),
        builtins: true,
    }
}

fn run(sch: &str) -> XNetlist {
    let f = file::parse(sch).unwrap();
    netlist(Path::new("/w/tb.sch"), &f, &builtins(), &Options::default()).unwrap()
}

/// The element and directive lines, without xschem's comments.
fn lines(n: &XNetlist) -> Vec<String> {
    n.text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('*'))
        .map(|l| l.trim_end().to_string())
        .collect()
}

const RC: &str = r#"v {xschem version=3.4.4 file_version=1.2}
G {}
K {}
V {}
S {}
E {}
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
fn rc_low_pass() {
    let n = run(RC);
    assert!(n.findings.is_empty(), "{:?}", n.findings);
    assert_eq!(
        lines(&n),
        [
            "V1 in GND dc 0 ac 1",
            "R1 out in 1k m=1",
            "C1 out GND 1n m=1",
            ".ac dec 10 1 1meg",
            ".GLOBAL GND",
            ".end"
        ],
        "{}",
        n.text
    );
    assert!(n.text.starts_with("** sch_path: /w/tb.sch\n**.subckt tb\n"));
    assert!(n.text.contains("**** begin user architecture code\n"));
    assert_eq!(n.builtin_symbols().len(), 6);
}

#[test]
fn connectivity_rules() {
    // A pin in the middle of a wire joins it; wires crossing in their
    // middles do not join; a T joins; pins that touch join; an open pin gets
    // its own net, numbered after the wires.
    let sch = r#"v {xschem version=3.4.4 file_version=1.2}
N 0 -100 0 100 {}
N -100 0 100 0 {}
N 200 -100 200 100 {}
N 200 0 300 0 {}
C {devices/res.sym} 0 -130 0 0 {name=R1 value=1}
C {devices/res.sym} -130 0 1 0 {name=R2 value=2}
C {devices/res.sym} 200 -130 0 0 {name=R3 value=3}
C {devices/res.sym} 330 0 1 0 {name=R4 value=4}
C {devices/res.sym} 500 0 0 0 {name=R5 value=5}
C {devices/res.sym} 500 60 0 0 {name=R6 value=6}
"#;
    let n = run(sch);
    let net = parse(&n.text);
    let nodes = |name: &str| net.element(name).unwrap().nodes.clone();
    // R1's M pin is the vertical wire's end; R2's P pin (turned) is the
    // horizontal wire's end, and the two wires cross without joining.
    assert_eq!(nodes("R1")[1], "net1");
    assert_eq!(nodes("R2")[0], "net2");
    // The T at x=200 joins R3 and R4.
    assert_eq!(nodes("R3")[1], nodes("R4")[1]);
    // R5's M pin and R6's P pin meet at (500, 30).
    assert_eq!(nodes("R5")[1], nodes("R6")[0]);
    // The T joins the third and fourth wires into net3. Open pins are
    // numbered after the wires, in instance and pin order.
    assert_eq!(nodes("R3")[1], "net3");
    assert_eq!(nodes("R1")[0], "net4");
    assert_eq!(nodes("R2")[1], "net5");
    assert_eq!(nodes("R5"), ["net8", "net9"]);
}

#[test]
fn labels_name_nets_and_shorts_are_findings() {
    let sch = r#"v {xschem version=3.4.4 file_version=1.2}
N 0 -30 0 -60 {}
C {devices/res.sym} 0 0 0 0 {name=R1 value=1}
C {devices/lab_pin.sym} 0 -60 0 0 {name=p1 lab=a}
C {devices/lab_wire.sym} 0 -45 0 0 {name=p2 lab=b}
C {devices/lab_pin.sym} 0 30 0 0 {name=p3 lab=net1}
C {devices/res.sym} 100 0 0 0 {name=R2 value=1}
"#;
    let n = run(sch);
    let net = parse(&n.text);
    assert_eq!(net.element("R1").unwrap().nodes, ["a", "net1"]);
    // `net1` is taken by a label, so the first automatic name is net2.
    assert_eq!(net.element("R2").unwrap().nodes, ["net2", "net3"]);
    assert!(
        n.findings
            .iter()
            .any(|f| f.subject == "p2" && f.message.contains("short")),
        "{:?}",
        n.findings
    );
}

#[test]
fn rotation_and_flip_move_pins() {
    // xschem mirrors first, then turns: a flipped, quarter-turned res has
    // its P pin at (+30, 0) instead of (-30, 0).
    let inst = XInstance {
        symbol: "res.sym".into(),
        x: 0.0,
        y: 0.0,
        rot: 1,
        flip: false,
        props: String::new(),
        embedded: None,
    };
    assert_eq!(place(&inst, 0.0, -30.0), (30.0, 0.0));
    let flipped = XInstance {
        flip: true,
        ..inst.clone()
    };
    assert_eq!(place(&flipped, 20.0, -30.0), (30.0, -20.0));
    let r2 = XInstance {
        rot: 2,
        ..inst.clone()
    };
    assert_eq!(place(&r2, 20.0, -30.0), (-20.0, 30.0));
    let r3 = XInstance { rot: 3, ..inst };
    assert_eq!(place(&r3, 20.0, -30.0), (-30.0, -20.0));
}

const IHP: &str = r#"v {xschem version=3.4.4 file_version=1.2}
N 20 0 20 30 {}
C {sg13g2_pr/sg13_lv_nmos.sym} 0 0 0 0 {name=M1
l=0.45u
w=1.0u
ng=1
m=1
model=sg13_lv_nmos
spiceprefix=X
}
C {devices/vsource.sym} -20 30 0 0 {name=Vg value=1.2}
C {devices/ammeter.sym} 20 -60 0 0 {name=Vd}
C {devices/gnd.sym} 20 30 0 0 {name=l1 lab=GND}
C {devices/gnd.sym} -20 60 0 0 {name=l2 lab=GND}
C {devices/lab_pin.sym} 20 -90 0 0 {name=p1 lab=d}
C {devices/lab_pin.sym} -20 0 0 0 {name=p2 lab=g}
C {sg13g2_pr/ptap1.sym} 200 0 0 0 {name=R5
model=ptap1
spiceprefix=X
w=10.0e-6
l=1.0e-6}
C {sg13g2_pr/rppd.sym} 300 0 0 0 {name=R2
w=0.5e-6
l=2e-6
model=rppd
body=sub!
spiceprefix=X
b=0
m=1}
C {devices/code_shown.sym} 0 100 0 0 {name=MODEL only_toplevel=true
format="tcleval( @value )"
value=".lib $::SG13G2_MODELS/cornerMOSlv.lib mos_tt
"}
C {simulator_commands_shown.sym} 0 200 0 0 {name=COMMANDS
simulator=ngspice
only_toplevel=false
value="
.param temp=27
.control
op
.endc
"}
"#;

#[test]
fn ihp_devices_and_tcl() {
    let f = file::parse(IHP).unwrap();
    let opts = Options {
        env: Env::new("ngspice").with_var("SG13G2_MODELS", "/pdk/models"),
        ..Options::default()
    };
    let n = netlist(Path::new("/w/tb.sch"), &f, &builtins(), &opts).unwrap();
    assert!(n.findings.is_empty(), "{:?}", n.findings);
    let l = lines(&n);
    let has = |want: &str| assert!(l.iter().any(|x| x == want), "{want}\n{}", n.text);
    has("XM1 net1 g GND GND sg13_lv_nmos w=1.0u l=0.45u ng=1 m=1");
    has("Vg g GND 1.2");
    has("Vd d net1 0");
    has(".save i(vd)");
    has("XR5 net2 net3 ptap1 R=30.62 w=10.0e-6 l=1.0e-6");
    has("XR2 net4 net5 sub! rppd w=0.5e-6 l=2e-6 m=1 b=0");
    assert!(
        n.text
            .contains(" .lib /pdk/models/cornerMOSlv.lib mos_tt\n"),
        "{}",
        n.text
    );
    assert!(n.text.contains("\n.control\nop\n.endc\n"), "{}", n.text);
}

#[test]
fn tcl_outside_the_subset_leaves_the_element_out() {
    let sch = r#"v {xschem version=3.4.4 file_version=1.2}
C {devices/vsource.sym} 0 0 0 0 {name=V1 value="[exec touch /tmp/pwned]"}
C {devices/res.sym} 100 0 0 0 {name=R1 value="tcleval([open /etc/passwd])"}
C {devices/res.sym} 200 0 0 0 {name=R2 value=1k}
"#;
    let n = run(sch);
    let net = parse(&n.text);
    assert!(net.element("V1").is_none(), "{}", n.text);
    assert!(net.element("R1").is_none(), "{}", n.text);
    assert!(net.element("R2").is_some());
    assert!(
        n.findings
            .iter()
            .any(|f| f.subject == "V1" && f.message.contains("Tcl"))
    );
    assert!(
        n.findings
            .iter()
            .any(|f| f.subject == "R1" && f.message.contains("Tcl"))
    );
}

#[test]
fn unknown_symbols_and_bus_names_are_findings() {
    let sch = r#"v {xschem version=3.4.4 file_version=1.2}
C {mystery.sym} 0 0 0 0 {name=X1}
C {devices/res.sym} 100 0 0 0 {name=R1 value=1}
C {devices/lab_pin.sym} 100 -30 0 0 {name=p1 lab=D[3:0]}
"#;
    let n = run(sch);
    assert!(
        n.findings
            .iter()
            .any(|f| f.subject == "X1" && f.message.contains("not found"))
    );
    assert!(
        n.findings
            .iter()
            .any(|f| f.subject == "p1" && f.message.contains("bus"))
    );
}

#[test]
fn ignored_and_shorted_instances() {
    let sch = r#"v {xschem version=3.4.4 file_version=1.2}
C {devices/res.sym} 0 0 0 0 {name=R1 value=1}
C {devices/res.sym} 0 60 0 0 {name=R2 value=1 spice_ignore=true}
C {devices/res.sym} 0 120 0 0 {name=R3 value=1 spice_ignore=short}
C {devices/res.sym} 0 180 0 0 {name=R4 value=1}
C {devices/title.sym} 0 300 0 0 {name=l1}
"#;
    let n = run(sch);
    let net = parse(&n.text);
    assert!(net.element("R2").is_none());
    assert!(net.element("R3").is_none());
    // R3 is a short: R4's top pin (150) and R2... R2 is ignored, so R1's
    // bottom (30) meets nothing; R3 joins 90 and 150, which is R4's top.
    let r4 = &net.element("R4").unwrap().nodes;
    assert_eq!(net.elements().count(), 2);
    assert_ne!(r4[0], r4[1]);
}

#[test]
fn hierarchy_becomes_subcircuits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(
        dir.join("inv.sym"),
        r#"v {xschem version=3.4.4 file_version=1.2}
K {type=subcircuit
format="@name @pinlist @symname wn=@wn"
template="name=x1 wn=1u"}
B 5 -42.5 -2.5 -37.5 2.5 {name=A dir=in}
B 5 37.5 -2.5 42.5 2.5 {name=Y dir=out}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("inv.sch"),
        r#"v {xschem version=3.4.4 file_version=1.2}
C {devices/ipin.sym} 0 0 0 0 {name=p1 lab=A}
C {devices/opin.sym} 100 0 0 0 {name=p2 lab=Y}
C {devices/res.sym} 50 0 1 0 {name=R1 value=@wn}
C {devices/code_shown.sym} 0 100 0 0 {name=s1 value=".param inner=1"}
C {devices/code_shown.sym} 0 200 0 0 {name=s2 only_toplevel=true value=".param top=1"}
"#,
    )
    .unwrap();
    let top = r#"v {xschem version=3.4.4 file_version=1.2}
C {inv.sym} 0 0 0 0 {name=x1 wn=2u}
C {inv.sym} 0 100 0 0 {name=x2}
C {devices/lab_pin.sym} -40 0 0 0 {name=p1 lab=in}
C {devices/lab_pin.sym} 40 0 0 0 {name=p2 lab=out}
"#;
    let f = file::parse(top).unwrap();
    let r = Resolver::new(Vec::new(), vec![dir.clone()]);
    let n = netlist(&dir.join("top.sch"), &f, &r, &Options::default()).unwrap();
    let net = parse(&n.text);
    assert_eq!(
        net.element("x1").unwrap().nodes,
        ["in", "out"],
        "{}",
        n.text
    );
    assert_eq!(net.element("x1").unwrap().rest, "inv wn=2u");
    assert_eq!(net.element("x2").unwrap().rest, "inv wn=1u");
    let sub = net.subckts().next().unwrap();
    assert_eq!(sub.name, "inv");
    assert_eq!(sub.ports, ["A", "Y"]);
    assert!(sub.params.contains("wn=1u"), "{}", n.text);
    assert!(n.text.contains(".param inner=1"));
    assert!(!n.text.contains(".param top=1"));
    assert_eq!(n.sheets.len(), 2);
}

#[test]
fn recursive_and_deep_hierarchies_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let sym = |name: &str| {
        format!(
            "v {{xschem version=3.4.4 file_version=1.2}}\nK {{type=subcircuit\nformat=\"@name @pinlist @symname\"\ntemplate=\"name=x1\"}}\nB 5 -2.5 -2.5 2.5 2.5 {{name={name}_p}}\n"
        )
    };
    // a -> a (recursion) and a chain c0 -> c1 -> ... -> c20.
    std::fs::write(dir.join("a.sym"), sym("a")).unwrap();
    std::fs::write(dir.join("a.sch"), "C {a.sym} 0 0 0 0 {name=x1}\n").unwrap();
    for i in 0..21 {
        std::fs::write(dir.join(format!("c{i}.sym")), sym("c")).unwrap();
        std::fs::write(
            dir.join(format!("c{i}.sch")),
            format!("C {{c{}.sym}} 0 0 0 0 {{name=x1}}\n", i + 1),
        )
        .unwrap();
    }
    let f = file::parse("C {a.sym} 0 0 0 0 {name=x1}\nC {c0.sym} 0 100 0 0 {name=x2}\n").unwrap();
    let r = Resolver::new(Vec::new(), vec![dir.clone()]);
    let n = netlist(&dir.join("top.sch"), &f, &r, &Options::default()).unwrap();
    assert_eq!(n.text.matches(".subckt a ").count(), 1);
    assert!(
        n.findings.iter().any(|f| f.message.contains("nest deeper")),
        "{:?}",
        n.findings
    );
    assert!(n.sheets.len() <= MAX_DEPTH + 3);
}

#[test]
fn awk_post_processing() {
    let raw = "** sch_path: x\n**** spice_prefix X\nM1 ?1 d ?1 g ?1 s ?1 b nmos w=1u\n+ l=1u\n**** end_element\nR1 ?1 a ?1 b value=1k\nG1 ?1 a ?1 b ?1 c ?1 d value=2\n.save i( ?1 V1 ) v( ?1 out )\n.subckt inv a b m=1 w=2\n**** begin user architecture code\n.param x = 1\n  .save  v( ?1 n )\n**** end user architecture code\n\n";
    let out = postprocess(raw);
    assert_eq!(
        out,
        "** sch_path: x\nXM1 d g s b nmos w=1u l=1u \nR1 a b r=1k \nG1 a b c d cur=2 \n.save i(v1)\n.save v(out)\n.subckt inv a b  w=2\n**** begin user architecture code\n.param x = 1\n  .save  v( ?1 n )\n**** end user architecture code\n\n"
    );
}

#[test]
fn format_tokens() {
    let p = tokens("@name @pinlist @@P\\@x @#0:net_name @value\\\nm=@m%z");
    let toks: Vec<String> = p
        .iter()
        .filter_map(|p| match p {
            Piece::Token { text, .. } => Some(text.clone()),
            Piece::Char(_) => None,
        })
        .collect();
    assert_eq!(
        toks,
        [
            "@name",
            "@pinlist",
            "@@P@x",
            "@#0:net_name",
            "@value",
            "@m",
            "%z"
        ]
    );
    assert_eq!(
        pin_and_attr("@#A[3:0]:net_name"),
        ("A[3:0]".into(), "net_name".into())
    );
    assert_eq!(pin_and_attr("@#2"), ("2".into(), String::new()));
}

/// Random edits of real schematics give a netlist or an error, never a
/// panic, and finish quickly.
#[test]
fn random_schematics_never_panic() {
    let mut rng = crate::xschem::test_rng(0xa5_1ce5);
    let corpus = [RC, IHP];
    let alphabet: Vec<char> = "{}[]@#%\\\" \n0123456789.-eCNTLBK=?$;:,\u{e9}"
        .chars()
        .collect();
    for case in 0..600 {
        let base: Vec<char> = corpus[case % 2].chars().collect();
        let mut chars = base.clone();
        for _ in 0..1 + rng.below(8) {
            let at = rng.below(chars.len());
            match rng.below(3) {
                0 => chars[at] = alphabet[rng.below(alphabet.len())],
                1 => {
                    chars.insert(at, alphabet[rng.below(alphabet.len())]);
                }
                _ => {
                    chars.truncate(at.max(1));
                }
            }
        }
        let text: String = chars.into_iter().collect();
        if let Ok(f) = file::parse(&text) {
            let _ = netlist(Path::new("/w/tb.sch"), &f, &builtins(), &Options::default());
        }
    }
}

#[test]
fn dense_input_is_refused_or_finishes() {
    // Thousands of wires on one spot and one long diagonal each.
    let mut sch = String::new();
    for i in 0..3000 {
        sch.push_str(&format!("N 0 0 {} {} {{}}\n", 1_000_000 + i, 1_000_000 + i));
    }
    let start = std::time::Instant::now();
    let f = file::parse(&sch).unwrap();
    let _ = netlist(Path::new("/w/x.sch"), &f, &builtins(), &Options::default());
    assert!(start.elapsed() < std::time::Duration::from_secs(20));
}

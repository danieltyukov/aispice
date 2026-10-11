//! Whether xschem itself may be run on a schematic.
//!
//! aispice never runs Tcl, but xschem does: while netlisting it evaluates
//! `tcleval(...)` properties and `@tcleval` tokens, substitutes instance
//! values into its symbols' Tcl before evaluating it, and splices file names
//! into Tcl commands. A schematic from someone else could use any of these to
//! run commands on this machine. So before aispice asks xschem for a netlist
//! (to prove a conversion), it reads every file xschem would read, the way
//! xschem would find it, and refuses when:
//!
//! - a schematic, or a symbol outside the trusted folders (xschem's own
//!   library and the registered PDK), has `tcleval(` and a `[` (Tcl command
//!   substitution) in the same property string or text;
//! - an instance of a symbol whose properties use `tcleval` has a `[` or `]`
//!   in its own properties, which xschem would substitute into that Tcl. The
//!   one exception is the `value` of xschem's own `simulator_commands`
//!   symbols, which their Tcl wraps in `return {...}`: brackets there are
//!   text, as long as the value has no brace or backslash that could close
//!   the group (IHP's test benches put ngspice's `vec[0]` there);
//! - a symbol reference has `[`, `$`, `;` or a backslash, is not a `.sym` or
//!   `.sch` file, or leads outside the folders aispice may read;
//! - a file cannot be read, or the hierarchy is deeper or wider than
//!   aispice's netlister follows.
//!
//! A refusal means only that xschem is not run; the conversion itself does
//! not depend on it.

use super::file::{self, XFile};
use super::netlist::MAX_DEPTH;
use super::props;
use super::symbol::{Origin, ResolveError, Resolver, read_limited};
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

/// At most this many schematics are scanned.
const MAX_SHEETS: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Why xschem may not be run; empty when it may.
    pub refused: Vec<String>,
    /// Every file xschem would read: the schematics, then the symbols.
    pub files: Vec<PathBuf>,
    /// References only aispice's built-in definitions resolve. xschem would
    /// not find them, so its netlist would not be comparable.
    pub builtin: Vec<String>,
}

impl Verdict {
    pub fn allowed(&self) -> bool {
        self.refused.is_empty()
    }
}

/// Scan the schematic at `top` and everything it pulls in. `trusted` are
/// the folders whose symbols may hold Tcl: xschem's library and the
/// registered PDK's `libs.tech/xschem`.
pub fn scan(top: &Path, resolver: &Resolver, trusted: &[PathBuf]) -> Verdict {
    let mut v = Verdict::default();
    let trusted: Vec<PathBuf> = trusted
        .iter()
        .filter_map(|d| d.canonicalize().ok())
        .collect();
    let is_trusted = |p: &Path| trusted.iter().any(|d| p.starts_with(d));
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::from([(top.to_path_buf(), 0)]);
    let mut seen_sheets: BTreeSet<PathBuf> = BTreeSet::new();
    let mut seen_files: BTreeSet<PathBuf> = BTreeSet::new();
    while let Some((path, depth)) = queue.pop_front() {
        let shown = path.display().to_string();
        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !seen_sheets.insert(canon.clone()) {
            continue;
        }
        if seen_sheets.len() > MAX_SHEETS {
            v.refused.push(format!(
                "the hierarchy has more than {MAX_SHEETS} schematics"
            ));
            break;
        }
        let f = match read_limited(&canon)
            .map_err(|e| e.to_string())
            .and_then(|t| file::parse(&t).map_err(|e| e.to_string()))
        {
            Ok(f) => f,
            Err(e) => {
                v.refused.push(format!("{shown} could not be read: {e}"));
                continue;
            }
        };
        if seen_files.insert(canon.clone()) {
            v.files.push(canon.clone());
        }
        tcl_in_strings(&f, &shown, &mut v.refused);
        let dir = canon.parent().map(Path::to_path_buf).unwrap_or_default();
        for inst in &f.instances {
            let name = props::get(&inst.props, "name").unwrap_or_else(|| inst.symbol.clone());
            if inst
                .symbol
                .chars()
                .any(|c| matches!(c, '[' | ']' | '$' | ';' | '\\'))
            {
                v.refused.push(format!(
                    "{shown}: instance {name} refers to symbol `{}`, which xschem would evaluate as Tcl",
                    inst.symbol
                ));
                continue;
            }
            let sym = if let Some(e) = &inst.embedded {
                super::symbol::XSymbol::from_file(&inst.symbol, (**e).clone(), Origin::Embedded)
            } else {
                match resolver.resolve(&inst.symbol, &dir) {
                    Ok(s) => s,
                    Err(ResolveError::NotFound(_)) => continue,
                    Err(e) => {
                        v.refused.push(format!("{shown}: instance {name}: {e}"));
                        continue;
                    }
                }
            };
            match &sym.origin {
                Origin::File(p) => {
                    if seen_files.insert(p.clone()) {
                        v.files.push(p.clone());
                        if !is_trusted(p) {
                            tcl_in_strings(&sym.file, &p.display().to_string(), &mut v.refused);
                        }
                    }
                }
                Origin::Builtin => {
                    if !v.builtin.contains(&sym.reference) {
                        v.builtin.push(sym.reference.clone());
                    }
                }
                Origin::Embedded => {}
            }
            let symbol_tcl =
                has_tcleval(&sym.props) || sym.file.shapes.iter().any(|s| has_tcleval(&s.props));
            if symbol_tcl
                && inst.props.contains(['[', ']'])
                && !braced_value_only(&sym, &inst.props, &is_trusted)
            {
                v.refused.push(format!(
                    "{shown}: instance {name} has `[` or `]` in its properties, which xschem would substitute into the Tcl of symbol {}",
                    sym.reference
                ));
            }
            if sym.kind() == "subcircuit" && !props::is_true(&sym.props, "spice_primitive") {
                let sch = match sym.attr("schematic").filter(|s| !s.is_empty()) {
                    Some(r) => resolver.locate(&r, &dir),
                    None => match &sym.origin {
                        Origin::File(p) => resolver.locate_path(&p.with_extension("sch")),
                        _ => Ok(None),
                    },
                };
                match sch {
                    Ok(Some(p)) if depth < MAX_DEPTH => queue.push_back((p, depth + 1)),
                    Ok(Some(_)) => v.refused.push(format!(
                        "{shown}: subcircuits nest deeper than {MAX_DEPTH} levels"
                    )),
                    Ok(None) => {}
                    Err(e) => v.refused.push(format!("{shown}: instance {name}: {e}")),
                }
            }
        }
    }
    v.refused.dedup();
    v
}

fn has_tcleval(s: &str) -> bool {
    s.contains("tcleval(")
}

/// Whether the brackets in an instance's properties are only in a `value`
/// that a trusted `simulator_commands` symbol's Tcl puts inside braces.
fn braced_value_only(
    sym: &super::symbol::XSymbol,
    inst_props: &str,
    is_trusted: &dyn Fn(&Path) -> bool,
) -> bool {
    let Origin::File(path) = &sym.origin else {
        return false;
    };
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let format = norm(&sym.attr("format").unwrap_or_default());
    let known = [
        "devices/simulator_commands.sym",
        "devices/simulator_commands_shown.sym",
    ]
    .iter()
    .filter_map(|r| super::builtin::get(r))
    .filter_map(|(_, text)| file::parse(text).ok())
    .any(|f| norm(&props::get(f.symbol_props(), "format").unwrap_or_default()) == format);
    if !is_trusted(path) || !known || props::get(inst_props, "format").is_some() {
        return false;
    }
    props::pairs(inst_props).iter().all(|(k, v)| {
        let bracket = |s: &str| s.contains(['[', ']']);
        !bracket(k) && (!bracket(v) || (k == "value" && !v.contains(['{', '}', '\\'])))
    })
}

/// Every string of a file, embedded symbols included: refuse any that has
/// `tcleval(` and a `[`.
fn tcl_in_strings(f: &XFile, shown: &str, refused: &mut Vec<String>) {
    let mut strings: Vec<(&str, &str)> = vec![
        ("the version", f.version.as_str()),
        ("the VHDL properties", f.vhdl.as_str()),
        ("the symbol properties", f.symbol.as_str()),
        ("the Verilog properties", f.verilog.as_str()),
        ("the SPICE properties", f.spice.as_str()),
        ("the tEDAx properties", f.tedax.as_str()),
    ];
    for w in &f.wires {
        strings.push(("a wire", w.props.as_str()));
    }
    for t in &f.texts {
        strings.push(("a text", t.text.as_str()));
        strings.push(("a text", t.props.as_str()));
    }
    for s in &f.shapes {
        strings.push(("a shape", s.props.as_str()));
    }
    for i in &f.instances {
        strings.push(("an instance", i.symbol.as_str()));
        strings.push(("an instance", i.props.as_str()));
    }
    for (what, s) in strings {
        if has_tcleval(s) && s.contains('[') {
            refused.push(format!(
                "{shown}: {what} holds Tcl command substitution in tcleval(...), which xschem would run"
            ));
        }
    }
    for i in &f.instances {
        if let Some(e) = &i.embedded {
            tcl_in_strings(e, shown, refused);
        }
    }
}

/// Whether a path is safe to hand to xschem, which splices file names into
/// Tcl commands: letters, digits and `/ . _ - + , = @` only.
pub fn safe_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | ',' | '=' | '@')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        dir: PathBuf,
        lib: PathBuf,
        resolver: Resolver,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap().join("proj");
        let lib = tmp.path().canonicalize().unwrap().join("lib");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(lib.join("devices")).unwrap();
        // A trusted symbol whose format is Tcl, like xschem's vsource.
        std::fs::write(
            lib.join("devices/vsource.sym"),
            "K {type=vsource\nformat=\"tcleval([expr \\{@savecurrent ? \\\\\"@name @pinlist @value\\\\\" : \\\\\"@name @pinlist @value\\\\\"\\}])\"\ntemplate=\"name=V1 value=3 savecurrent=false\"}\nB 5 -2.5 -32.5 2.5 -27.5 {name=p}\nB 5 -2.5 27.5 2.5 32.5 {name=m}\n",
        )
        .unwrap();
        std::fs::write(
            lib.join("devices/res.sym"),
            "K {type=resistor\nformat=\"@name @pinlist @value\"\ntemplate=\"name=R1 value=1k\"}\nB 5 -2.5 -32.5 2.5 -27.5 {name=P}\nB 5 -2.5 27.5 2.5 32.5 {name=M}\n",
        )
        .unwrap();
        let (_, commands) =
            super::super::builtin::get("devices/simulator_commands_shown.sym").unwrap();
        std::fs::write(lib.join("devices/simulator_commands_shown.sym"), commands).unwrap();
        let resolver = Resolver::new(vec![lib.join("devices"), lib.clone()], vec![dir.clone()]);
        Fixture {
            _tmp: tmp,
            dir,
            lib,
            resolver,
        }
    }

    fn verdict(fx: &Fixture, sch: &str) -> Verdict {
        let path = fx.dir.join("tb.sch");
        std::fs::write(&path, sch).unwrap();
        scan(&path, &fx.resolver, std::slice::from_ref(&fx.lib))
    }

    #[test]
    fn plain_schematics_pass() {
        let fx = fixture();
        let v = verdict(
            &fx,
            "C {vsource.sym} 0 0 0 0 {name=V1 value=\"dc 1 ac 1\"}\nC {res.sym} 0 0 0 0 {name=R1 value=1k}\nC {devices/code_shown.sym} 0 0 0 0 {name=s1 format=\"tcleval( @value )\" value=\".lib $::MODELS_NGSPICE/x.lib tt\"}\n",
        );
        assert!(v.allowed(), "{:?}", v.refused);
        assert_eq!(v.files.len(), 3);
        assert_eq!(v.builtin, ["devices/code_shown.sym"]);
    }

    #[test]
    fn hostile_schematics_are_refused() {
        let fx = fixture();
        for (sch, why) in [
            (
                "C {res.sym} 0 0 0 0 {name=R1 value=\"tcleval([exec touch /tmp/x])\"}\n",
                "Tcl command substitution",
            ),
            (
                "C {res.sym} 0 0 0 0 {name=R1 format=\"@tcleval([exec ls])\"}\n",
                "Tcl command substitution",
            ),
            (
                "T {tcleval([exec ls])} 0 0 0 0 0.4 0.4 {}\nC {res.sym} 0 0 0 0 {name=R1}\n",
                "Tcl command substitution",
            ),
            ("S {tcleval([exec ls])}\n", "Tcl command substitution"),
            (
                "C {vsource.sym} 0 0 0 0 {name=V1 value=\"[exec touch /tmp/x]\"}\n",
                "would substitute",
            ),
            ("C {[exec ls].sym} 0 0 0 0 {name=X1}\n", "evaluate as Tcl"),
            ("C {/etc/passwd.sym} 0 0 0 0 {name=X1}\n", "not found"),
            (
                "C {x.sym} 0 0 0 0 {name=X1 embed=true}\n[\nK {type=resistor format=\"tcleval([exec ls])\"}\n]\n",
                "Tcl command substitution",
            ),
        ] {
            let v = verdict(&fx, sch);
            if why == "not found" {
                // Not found anywhere: xschem draws a missing symbol, no Tcl.
                assert!(v.allowed(), "{sch}: {:?}", v.refused);
                continue;
            }
            assert!(!v.allowed(), "{sch}");
            assert!(
                v.refused.iter().any(|r| r.contains(why)),
                "{sch}: {:?}",
                v.refused
            );
        }
    }

    #[test]
    fn brackets_in_simulator_commands_values() {
        let fx = fixture();
        let block = |props: &str| {
            format!(
                "C {{simulator_commands_shown.sym}} 0 0 0 0 {{name=COMMANDS simulator=ngspice {props}}}\n"
            )
        };
        assert!(verdict(&fx, &block("value=\"let g = gain[0]\"")).allowed());
        for bad in [
            "value=\"x \\} [exec ls] \\{\"",
            "value=\"a\\\\\\\\ [b]\"",
            "simulator=\"ngspice ][exec ls][\" value=x",
            "value=\"[x]\" format=\"tcleval(@value)\"",
        ] {
            assert!(!verdict(&fx, &block(bad)).allowed(), "{bad}");
        }
    }

    #[test]
    fn untrusted_symbols_with_tcl_are_refused_and_outside_files_too() {
        let fx = fixture();
        std::fs::write(
            fx.dir.join("evil.sym"),
            "K {type=resistor format=\"tcleval([exec ls])\"}\nB 5 0 0 1 1 {name=P}\n",
        )
        .unwrap();
        let v = verdict(&fx, "C {evil.sym} 0 0 0 0 {name=R1}\n");
        assert!(!v.allowed());
        // The same symbol in the trusted library is accepted.
        std::fs::write(
            fx.lib.join("devices/ok.sym"),
            "K {type=resistor format=\"tcleval([expr 1])\"}\nB 5 0 0 1 1 {name=P}\n",
        )
        .unwrap();
        assert!(verdict(&fx, "C {ok.sym} 0 0 0 0 {name=R1}\n").allowed());
        // A file outside the project and the library is refused, not read.
        let outside = fx.dir.parent().unwrap().join("outside.sym");
        std::fs::write(&outside, "K {type=resistor}\n").unwrap();
        let v = verdict(&fx, "C {../outside.sym} 0 0 0 0 {name=R1}\n");
        assert!(
            v.refused.iter().any(|r| r.contains("outside")),
            "{:?}",
            v.refused
        );
    }

    #[test]
    fn sub_schematics_are_scanned() {
        let fx = fixture();
        std::fs::write(
            fx.dir.join("blk.sym"),
            "K {type=subcircuit format=\"@name @pinlist @symname\" template=\"name=x1\"}\nB 5 0 0 1 1 {name=A}\n",
        )
        .unwrap();
        std::fs::write(
            fx.dir.join("blk.sch"),
            "C {res.sym} 0 0 0 0 {name=R1 value=\"tcleval([exec ls])\"}\n",
        )
        .unwrap();
        let v = verdict(&fx, "C {blk.sym} 0 0 0 0 {name=x1}\n");
        assert!(!v.allowed());
        assert!(v.refused[0].contains("blk.sch"), "{:?}", v.refused);
    }

    #[test]
    fn paths_xschem_may_see() {
        assert!(safe_path(Path::new(
            "/home/u/pdk/ihp-sg13g2/libs.tech/xschem/a_b-c+d,e=f@g.sch"
        )));
        for bad in [
            "/tmp/a b.sch",
            "/tmp/a$b.sch",
            "/tmp/a[1].sch",
            "/tmp/a;b",
            "/tmp/{x}",
            "",
            "/tmp/é",
        ] {
            assert!(!safe_path(Path::new(bad)), "{bad}");
        }
    }

    /// Gated on `AISPICE_PDK_IHP`: IHP's own test benches pass the scan.
    #[test]
    fn ihp_test_benches_pass() {
        let Some(pdk) = std::env::var_os("AISPICE_PDK_IHP").map(PathBuf::from) else {
            return;
        };
        let share = PathBuf::from("/usr/share/xschem/xschem_library");
        let xschem = pdk.join("libs.tech/xschem");
        let r = Resolver::new(
            vec![share.join("devices"), share.clone(), xschem.clone()],
            vec![xschem.clone()],
        );
        let mut refused = Vec::new();
        for e in std::fs::read_dir(xschem.join("sg13g2_tests")).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|e| e == "sch") {
                let v = scan(&p, &r, &[share.clone(), xschem.clone()]);
                refused.extend(v.refused);
            }
        }
        assert!(refused.is_empty(), "{refused:#?}");
    }
}

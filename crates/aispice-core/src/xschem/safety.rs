//! A scan of what xschem would read, run before aispice lets xschem netlist
//! a schematic. It is defense in depth, not a security boundary.
//!
//! aispice never runs Tcl, but xschem does: while netlisting it evaluates
//! `tcleval(...)` property values and `@tcleval` tokens, substitutes
//! instance values into its symbols' Tcl before evaluating it, and splices
//! file names into Tcl commands. Any of these can run commands. A scan of
//! the text cannot prove that xschem will not: xschem's Tcl reads the text
//! differently from any scanner in places (the reviewed case: a
//! `tcleval(...)` value whose braces close the word xschem wraps it in), it
//! loads symbols by library path and sources rc files. So what keeps a
//! machine safe is elsewhere:
//!
//! - xschem never runs on a schematic aispice did not write unless the
//!   user asks for it (a CLI flag, or the environment of the process they
//!   start); no tool input and no project file can ask;
//! - every xschem run is inside a bubblewrap sandbox with no network, a
//!   private home and `/tmp`, and read-only views of xschem, the PDK and the
//!   input (`aispice_sim::xschem`);
//! - aispice's own exports are written so that no value they hold is
//!   evaluated by xschem ([`super::export`]).
//!
//! The scan still refuses, before anything runs, what is known to be
//! dangerous or not comparable:
//!
//! - any property value, symbol attribute (`format=...`) or text that
//!   starts with `tcleval(` and holds `[` or `]`, has braces that do not
//!   balance the way Tcl counts them after xschem turns `\}` into `}`, or
//!   ends in an odd run of backslashes (any of these lets the value leave
//!   the Tcl word xschem wraps it in); symbols in the trusted folders
//!   (xschem's library and the registered PDK) are exempt;
//! - `tcleval(` together with `[` anywhere in a schematic's or untrusted
//!   symbol's string;
//! - a value xschem would paste into Tcl that could change that Tcl: the
//!   properties of an instance of a symbol that uses `tcleval`, label names
//!   (net names reach other instances' Tcl) and the properties of subcircuit
//!   instances (they reach the subcircuit's Tcl as parameters) must not hold
//!   brackets, braces, quotes, `;`, backslashes or line breaks (see
//!   [`substituted_values`] for the two cases that allow more);
//! - bus and vector notation in names and labels, which xschem expands
//!   (a range can ask for millions of copies) and aispice does not;
//! - a symbol reference with any of those characters or `$`, not a `.sym`
//!   or `.sch` file, or leading outside the folders aispice may read;
//! - a file that cannot be read, or a hierarchy deeper, wider or larger than
//!   aispice's netlister follows.
//!
//! A refusal means only that xschem is not run; the conversion itself does
//! not depend on it.

use super::file::{self, XFile};
use super::netlist::MAX_DEPTH;
use super::props;
use super::symbol::{Origin, ResolveError, Resolver};
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
    let mut cache: std::collections::HashMap<
        (String, PathBuf),
        Result<std::sync::Arc<super::symbol::XSymbol>, ResolveError>,
    > = std::collections::HashMap::new();
    'sheets: while let Some((path, depth)) = queue.pop_front() {
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
        let f = match resolver
            .read(&canon)
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
        expanded_names(&f, &shown, &mut v.refused);
        let dir = canon.parent().map(Path::to_path_buf).unwrap_or_default();
        for inst in &f.instances {
            let name = props::get(&inst.props, "name").unwrap_or_else(|| inst.symbol.clone());
            if !inert(&inst.symbol) || inst.symbol.contains('$') {
                v.refused.push(format!(
                    "{shown}: instance {name} refers to symbol `{}`, which xschem would evaluate as Tcl",
                    inst.symbol
                ));
                continue;
            }
            let sym = if let Some(e) = &inst.embedded {
                std::sync::Arc::new(super::symbol::XSymbol::from_file(
                    &inst.symbol,
                    (**e).clone(),
                    Origin::Embedded,
                ))
            } else {
                // Each file is read once however many instances use it.
                let key = (inst.symbol.trim().to_string(), dir.clone());
                let resolved = cache
                    .entry(key)
                    .or_insert_with(|| {
                        resolver
                            .resolve(&inst.symbol, &dir)
                            .map(std::sync::Arc::new)
                    })
                    .clone();
                match resolved {
                    Ok(s) => s,
                    Err(ResolveError::NotFound(_)) => continue,
                    Err(ResolveError::Budget(m)) => {
                        v.refused.push(format!("{shown}: {m}"));
                        break 'sheets;
                    }
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
            if let Some(problem) = substituted_values(&sym, &inst.props, &is_trusted) {
                v.refused
                    .push(format!("{shown}: instance {name}: {problem}"));
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

/// Whether a value stays one Tcl word when xschem evaluates it. xschem's
/// `tcl_hook2` takes a value that starts with `tcleval(`, turns every `\}`
/// into `}` and evaluates `tclpropeval2 {` + value + `}`. Tcl ends that
/// brace word at the `}` that balances its `{`, counting braces the way it
/// does (a backslash escapes the next character), so a value whose braces
/// close early, or that ends in a backslash escaping the closing brace,
/// puts the rest of itself on the command line, where `;` starts a command
/// of its own. A command substitution (`[...]`) runs inside the word. Values
/// that do not start with `tcleval(` are not evaluated this way.
pub fn tcleval_stays_one_word(value: &str) -> bool {
    if !value.starts_with("tcleval(") {
        return true;
    }
    if value.contains(['[', ']']) {
        return false;
    }
    let unescaped = value.replace("\\}", "}");
    let mut depth = 0usize;
    let mut chars = unescaped.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '{' => depth += 1,
            '}' => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return false,
            },
            _ => {}
        }
    }
    let trailing = unescaped.chars().rev().take_while(|c| *c == '\\').count();
    depth == 0 && trailing % 2 == 0
}

/// Text that cannot change the shape of a Tcl script it is pasted into: no
/// command substitution, no braces or quotes to close a word early, no
/// command separator, no backslash. `$` only reads a variable, and xschem
/// runs with an empty environment.
fn inert(s: &str) -> bool {
    !s.contains(['[', ']', '{', '}', '"', ';', '\\', '\n', '\r'])
}

/// Why an instance's own values are unsafe to hand to xschem, if they are.
///
/// xschem pastes an instance's values (and its name and net names) into
/// its symbol's `tcleval(...)` text before evaluating it. Inside a command
/// substitution that text is a script, where a value could close a brace or
/// quote and start a command of its own, so every value must be [`inert`].
/// In plain `tcleval( @value )` text, which Tcl only substitutes, a value
/// just must not hold a command substitution. The one exception is the
/// `value` of xschem's own `simulator_commands` symbols, which their Tcl
/// wraps in `return {...}`: anything but a brace or backslash stays text
/// there (IHP's benches put ngspice's `vec[0]` in it).
fn substituted_values(
    sym: &super::symbol::XSymbol,
    inst_props: &str,
    is_trusted: &dyn Fn(&Path) -> bool,
) -> Option<String> {
    let pairs = props::pairs(inst_props);
    let own_format = props::get(inst_props, "format");
    let format = own_format
        .clone()
        .or_else(|| sym.attr("format"))
        .unwrap_or_default();
    let symbol_tcl =
        has_tcleval(&sym.props) || sym.file.shapes.iter().any(|s| has_tcleval(&s.props));
    let kind = sym.kind();
    // Labels name nets, and net names reach other instances' Tcl; the
    // properties of a subcircuit instance reach its schematic's Tcl as
    // parameters.
    let reaches_tcl = matches!(
        kind.as_str(),
        "label" | "ipin" | "opin" | "iopin" | "subcircuit"
    );
    if !(symbol_tcl || has_tcleval(inst_props) || reaches_tcl) {
        return None;
    }
    let script = reaches_tcl || (symbol_tcl && sym.props.contains('[')) || format.contains('[');
    let commands = braced_commands(sym, is_trusted) && own_format.is_none();
    for (k, v) in &pairs {
        let ok = if !script {
            !k.contains(['[', ']']) && !v.contains(['[', ']'])
        } else if commands && k == "value" {
            !v.contains(['{', '}', '\\'])
        } else {
            inert(k) && inert(v)
        };
        if !ok {
            return Some(format!(
                "property `{k}` holds characters xschem would read as Tcl when it pastes the value into the Tcl of symbol {}",
                sym.reference
            ));
        }
    }
    None
}

/// Whether a symbol is a trusted `netlist_commands` symbol (xschem's own
/// `simulator_commands` family) whose format pastes `@value` only inside a
/// Tcl brace group. There a bracket in the value stays literal, so the value
/// may hold `[` as long as it holds no brace or backslash to close the group
/// (the caller checks that). The symbol must come from a trusted source:
/// aispice's own built-in definition, used when xschem's library is not on
/// disk, or a file in xschem's library or the registered PDK. A definition
/// embedded in the schematic is untrusted. The check is structural rather
/// than an exact format match, so a newer xschem's wrapping (an added
/// `catch`/`elseif`) is still recognised while the brace guarantee holds.
fn braced_commands(sym: &super::symbol::XSymbol, is_trusted: &dyn Fn(&Path) -> bool) -> bool {
    let trusted = match &sym.origin {
        Origin::Builtin => true,
        Origin::File(path) => is_trusted(path),
        Origin::Embedded => false,
    };
    trusted
        && sym.kind() == "netlist_commands"
        && value_in_braces(&sym.attr("format").unwrap_or_default())
}

/// Whether every `@value` token in a symbol format sits inside a Tcl brace
/// group (brace depth above zero). xschem writes the Tcl braces as `\{`/`\}`,
/// so the backslash is ignored and only the braces are counted. False when
/// there is no `@value` to place.
fn value_in_braces(format: &str) -> bool {
    let bytes = format.as_bytes();
    let mut depth = 0i32;
    let mut saw = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'@' if format[i..].starts_with("@value")
                && !format[i + 6..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                saw = true;
                if depth <= 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    saw
}

/// Every string of a file, embedded symbols included: refuse any text or
/// property value that would leave its Tcl word ([`tcleval_stays_one_word`])
/// and any string that has `tcleval(` and a `[`.
fn tcl_in_strings(f: &XFile, shown: &str, refused: &mut Vec<String>) {
    // Property strings, read the way xschem's get_tok_value reads them.
    let mut prop_strings: Vec<(&str, &str)> = vec![
        ("the VHDL properties", f.vhdl.as_str()),
        ("the symbol properties", f.symbol.as_str()),
        ("the Verilog properties", f.verilog.as_str()),
        ("the tEDAx properties", f.tedax.as_str()),
    ];
    for w in &f.wires {
        prop_strings.push(("a wire", w.props.as_str()));
    }
    for t in &f.texts {
        prop_strings.push(("a text", t.props.as_str()));
    }
    for s in &f.shapes {
        prop_strings.push(("a shape", s.props.as_str()));
    }
    for i in &f.instances {
        prop_strings.push(("an instance", i.props.as_str()));
    }
    // Strings xschem may evaluate whole.
    let mut whole: Vec<(&str, &str)> = vec![
        ("the version", f.version.as_str()),
        ("the SPICE properties", f.spice.as_str()),
    ];
    for t in &f.texts {
        whole.push(("a text", t.text.as_str()));
    }
    for i in &f.instances {
        whole.push(("an instance", i.symbol.as_str()));
    }
    for (what, s) in &prop_strings {
        for (k, value) in props::pairs(s) {
            if !tcleval_stays_one_word(&value) {
                refused.push(format!(
                    "{shown}: {what} has `{k}=tcleval(...)` whose Tcl xschem would not keep in one word, so it could run commands"
                ));
            }
        }
    }
    for (what, s) in &whole {
        if !tcleval_stays_one_word(s) {
            refused.push(format!(
                "{shown}: {what} is `tcleval(...)` text whose Tcl xschem would not keep in one word, so it could run commands"
            ));
        }
    }
    for (what, s) in prop_strings.iter().chain(&whole) {
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

/// Bus and vector notation xschem expands: `R1[3:0]`, `D[7..0]`, `a,b`,
/// `4*x`. aispice netlists such a name as one name, so the netlists would
/// not compare, and a range can ask xschem for millions of copies.
fn expands(name: &str) -> bool {
    name.contains(['[', ',', '*']) || name.contains("..")
}

/// Refuse names and labels xschem would expand ([`expands`]).
fn expanded_names(f: &XFile, shown: &str, refused: &mut Vec<String>) {
    let mut names: Vec<String> = Vec::new();
    for w in &f.wires {
        names.extend(props::get(&w.props, "lab"));
    }
    for i in &f.instances {
        names.extend(props::get(&i.props, "name"));
        names.extend(props::get(&i.props, "lab"));
    }
    if let Some(n) = names.iter().find(|n| expands(n)) {
        refused.push(format!(
            "{shown}: `{n}` uses bus or vector notation, which xschem expands and aispice does not, so their netlists are not compared"
        ));
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
        // A subcircuit symbol with a schematic beside it, so an instance's
        // properties reach the subcircuit's Tcl.
        std::fs::write(
            dir.join("blk.sym"),
            "K {type=subcircuit\nformat=\"@name @pinlist @symname w=@w\"\ntemplate=\"name=x1 w=1\"}\nB 5 0 0 1 1 {name=A}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("blk.sch"),
            "C {res.sym} 0 0 0 0 {name=R1 value=@w}\n",
        )
        .unwrap();
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
                "read as Tcl",
            ),
            // A value that closes vsource's `[expr {...}]` word and starts a
            // command, without any `[` of its own.
            (
                "C {vsource.sym} 0 0 0 0 {name=V1 value=\"x\\} ; exec ls ; set y \\{\"}\n",
                "read as Tcl",
            ),
            // A label names a net, and net names are pasted into other
            // instances' Tcl.
            (
                "C {res.sym} 0 0 0 0 {name=R1}\nC {lab_pin.sym} 0 -30 0 0 {name=p1 lab=\"a\\}[exec ls]\\{\"}\n",
                "read as Tcl",
            ),
            // A subcircuit instance's properties reach its schematic's Tcl.
            (
                "C {blk.sym} 0 0 0 0 {name=x1 w=\"1\\}[exec ls]\\{\"}\n",
                "read as Tcl",
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
    fn tcleval_words() {
        for ok in [
            "1k",
            "tcleval( @value )",
            "tcleval(.lib $::SG13G2_MODELS/cornerMOSlv.lib mos_tt)",
            "tcleval({a} {b {c}})",
            "tcleval(\\{)",
            "tcleval(x)\\\\",
            // Not evaluated: only values that start with tcleval( are.
            "1k} ; exec touch /tmp/x ; #{",
        ] {
            assert!(tcleval_stays_one_word(ok), "{ok}");
        }
        for bad in [
            // The reviewer's payload.
            "tcleval(1k} ; exec touch /tmp/x ; #{)",
            // The same with xschem's own `\}` unescaping.
            "tcleval(1k\\} ; exec touch /tmp/x ; #\\{)",
            // An escaped brace Tcl does not count, so a naive count balances.
            "tcleval(\\{ } ; exec touch /tmp/x ; #)",
            // A trailing backslash that escapes the closing brace.
            "tcleval(x)\\",
            "tcleval(1k} ; exec touch /tmp/x ; #{)\\",
            "tcleval({x)",
            "tcleval([exec touch /tmp/x])",
        ] {
            assert!(!tcleval_stays_one_word(bad), "{bad}");
        }
    }

    /// A schematic with one resistor whose properties are `props`, written
    /// with the file writer's own escaping.
    fn one_resistor(props: &str) -> String {
        file::write(&XFile {
            instances: vec![super::super::file::XInstance {
                symbol: "res.sym".into(),
                x: 0.0,
                y: 0.0,
                rot: 0,
                flip: false,
                props: props.into(),
                embedded: None,
            }],
            ..XFile::default()
        })
    }

    /// The payloads of the review, each with a command that would leave a
    /// file: refused before xschem could see them, wherever they sit.
    #[test]
    fn the_reviewed_payloads_are_refused() {
        let fx = fixture();
        let sentinel = fx.dir.join("SENTINEL");
        let cmd = format!("exec touch {}", sentinel.display());
        let payloads = [
            format!("tcleval(1k}} ; {cmd} ; #{{)"),
            format!("tcleval(1k}} ; {cmd} ; #{{)\\"),
            format!("tcleval(\\{{ }} ; {cmd} ; #)"),
        ];
        for payload in &payloads {
            let quoted = props::quote(payload);
            for props in [
                format!("name=R1 value={quoted}"),
                format!("name=R1 lab={quoted}"),
                format!("name=R1 spice_ignore={quoted}"),
            ] {
                let v = verdict(&fx, &one_resistor(&props));
                assert!(!v.allowed(), "{props}");
                assert!(
                    v.refused.iter().any(|r| r.contains("one word")),
                    "{props}: {:?}",
                    v.refused
                );
            }
            // In a wire label and in a text.
            let f = XFile {
                wires: vec![super::super::file::XWire {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 10.0,
                    y2: 0.0,
                    props: format!("lab={quoted}"),
                }],
                texts: vec![super::super::file::XText {
                    text: payload.clone(),
                    x: 0.0,
                    y: 0.0,
                    rot: 0,
                    flip: false,
                    xscale: 0.4,
                    yscale: 0.4,
                    props: String::new(),
                }],
                ..XFile::default()
            };
            let v = verdict(&fx, &file::write(&f));
            assert_eq!(
                v.refused.iter().filter(|r| r.contains("one word")).count(),
                2,
                "{:?}",
                v.refused
            );
            // In the format of a symbol from the project, beside the
            // schematic or embedded in it; the same symbol in the trusted
            // library is exempt.
            let sym = file::write(&XFile {
                symbol: format!("type=resistor\nformat={quoted}\ntemplate=\"name=R1\""),
                shapes: vec![super::super::file::XShape {
                    kind: super::super::file::ShapeKind::Box,
                    layer: 5,
                    coords: vec![-2.5, -32.5, 2.5, -27.5],
                    props: "name=P".into(),
                }],
                ..XFile::default()
            });
            std::fs::write(fx.dir.join("evil.sym"), &sym).unwrap();
            let v = verdict(&fx, "C {evil.sym} 0 0 0 0 {name=R1}\n");
            assert!(
                v.refused
                    .iter()
                    .any(|r| r.contains("evil.sym") && r.contains("one word")),
                "{:?}",
                v.refused
            );
            let embedded = format!(
                "C {{x.sym}} 0 0 0 0 {{name=X1 embed=true}}\n[\n{}]\n",
                sym.trim_start_matches(|c| c != '\n')
            );
            assert!(!verdict(&fx, &embedded).allowed(), "{embedded}");
            std::fs::write(fx.lib.join("devices/trusted.sym"), &sym).unwrap();
            assert!(verdict(&fx, "C {trusted.sym} 0 0 0 0 {name=R1}\n").allowed());
        }
        assert!(!sentinel.exists());
    }

    #[test]
    fn names_xschem_would_expand_are_refused() {
        let fx = fixture();
        for sch in [
            "C {res.sym} 0 0 0 0 {name=R1[999999999:0] value=1k}\n",
            "C {res.sym} 0 0 0 0 {name=R1 value=1k}\nC {lab_pin.sym} 0 -30 0 0 {name=p1 lab=D[3:0]}\n",
            "N 0 0 10 0 {lab=a,b}\n",
            "C {res.sym} 0 0 0 0 {name=4*R1 value=1k}\n",
        ] {
            let v = verdict(&fx, sch);
            assert!(
                v.refused.iter().any(|r| r.contains("bus or vector")),
                "{sch}: {:?}",
                v.refused
            );
        }
    }

    /// A schematic that pulls in more files than one conversion reads is
    /// refused once the budget runs out, not read to the end.
    #[test]
    fn the_read_budget_holds() {
        let fx = fixture();
        let n = super::super::symbol::MAX_READ_FILES + 10;
        let mut sch = String::new();
        for i in 0..n {
            std::fs::write(
                fx.dir.join(format!("s{i}.sym")),
                "K {type=resistor format=\"@name @pinlist @value\"}\nB 5 0 0 1 1 {name=P}\n",
            )
            .unwrap();
            sch.push_str(&format!("C {{s{i}.sym}} 0 {i} 0 0 {{name=R{i}}}\n"));
        }
        let v = verdict(&fx, &sch);
        assert!(
            v.refused.iter().any(|r| r.contains("more than")),
            "{:?}",
            v.refused
        );
        assert!(v.files.len() <= super::super::symbol::MAX_READ_FILES);
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

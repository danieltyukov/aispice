//! What an export may write, so that every value stays text when xschem
//! opens the file.
//!
//! xschem evaluates Tcl from a schematic: any property value that starts
//! with `tcleval(`, a symbol's `format` or text that does (after pasting the
//! instance's values and net names into it), `@tcleval(...)` tokens, and
//! attributes it runs as commands (IHP's `drc`, when the user checks a
//! design). The values an export writes come from the user's `.asc`, so the
//! writer accepts only what it can show stays literal:
//!
//! - symbols from a fixed list ([`allowed_symbol`]): xschem's `devices/`
//!   parts and IHP's `sg13g2_pr` devices, never a reference from user data;
//! - property keys aispice sets itself or that the symbol's own template
//!   declares, never one of xschem's evaluated or structural attributes
//!   ([`RESERVED`]); a `format` only of the one shape aispice writes;
//! - no value, label, text or code block holding `tcleval`, a carriage
//!   return or a NUL;
//! - every value and net name a symbol's Tcl would substitute ([`exposure`],
//!   read from the symbol xschem will load, and for IHP's devices at least
//!   what IHP's own symbols substitute) made of letters, digits and
//!   `. _ + - * / ( ) , : = ! < > % ^ '` only, which no Tcl context reads as
//!   anything but text.
//!
//! An instance that fails is written another way (a format of its own, or
//! its SPICE line in a code block) or not at all, with a finding. After the
//! file is written, [`verify`] reads it back as xschem reads it and fails
//! the export if anything differs from what the writer meant.

use super::super::file::{self, XFile};
use super::super::props;
use super::super::symbol::XSymbol;
use std::collections::BTreeSet;

/// Attributes an export never writes from user data: xschem evaluates
/// them, runs them, or reads them as structure (another schematic, a
/// symbol definition, netlisting switches).
pub const RESERVED: &[&str] = &[
    "schematic",
    "spice_ignore",
    "lvs_ignore",
    "spice_sym_def",
    "spice_primitive",
    "embed",
    "device_model",
    "lvs_format",
    "verilog_format",
    "vhdl_format",
    "tedax_format",
    "spectre_format",
    "drc",
    "tclcommand",
    "place",
    "type",
    "global",
    "floater",
    "highlight",
    "savecurrent",
    "simulator",
];

/// Keys aispice itself sets on the instances it writes.
const BASE_KEYS: &[&str] = &[
    "name",
    "value",
    "model",
    "w",
    "l",
    "m",
    "extra",
    "vnam",
    "VAR",
    "FUNC",
    "spiceprefix",
    "body",
    "format",
    "lab",
    "sig_type",
    "only_toplevel",
];

/// What IHP's `sg13g2_pr` device symbols substitute into Tcl (their `drc`
/// commands and `tcleval` texts), assumed even when the symbol xschem will
/// load is not on this machine.
const IHP_EXPOSED: &[&str] = &[
    "name", "symname", "model", "w", "l", "ng", "m", "b", "Nx", "El", "body",
];

/// The generic devices an export may place.
const DEVICES: &[&str] = &[
    "devices/res.sym",
    "devices/capa.sym",
    "devices/ind.sym",
    "devices/vsource.sym",
    "devices/isource.sym",
    "devices/diode.sym",
    "devices/npn.sym",
    "devices/pnp.sym",
    "devices/nmos4.sym",
    "devices/pmos4.sym",
    "devices/vcvs.sym",
    "devices/vccs.sym",
    "devices/cccs.sym",
    "devices/ccvs.sym",
    "devices/bsource.sym",
    "devices/gnd.sym",
    "devices/lab_pin.sym",
    "devices/code_shown.sym",
];

/// Whether an export may place this symbol.
pub fn allowed_symbol(reference: &str) -> bool {
    if DEVICES.contains(&reference) {
        return true;
    }
    let Some(device) = reference
        .strip_prefix("sg13g2_pr/")
        .and_then(|r| r.strip_suffix(".sym"))
    else {
        return false;
    };
    crate::pdk::KITS
        .iter()
        .flat_map(|k| k.devices.iter())
        .any(|d| d.symbol.is_some() && d.name == device)
}

/// Text no Tcl context reads as anything but text: not a command
/// substitution, a variable, a quote, a brace, a backslash, a separator or
/// a blank.
pub fn tcl_literal(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '.' | '_'
                        | '+'
                        | '-'
                        | '*'
                        | '/'
                        | '('
                        | ')'
                        | ','
                        | ':'
                        | '='
                        | '!'
                        | '<'
                        | '>'
                        | '%'
                        | '^'
                        | '\''
                )
        })
}

/// Why a string may not be written anywhere in an export, if it may not.
fn unwritable(s: &str) -> Option<&'static str> {
    if s.to_ascii_lowercase().contains("tcleval") {
        Some("it holds `tcleval`, which xschem would evaluate as Tcl")
    } else if s.contains(['\r', '\0']) {
        Some("it holds a carriage return or NUL, which xschem does not read back")
    } else {
        None
    }
}

/// The values and nets a symbol substitutes into Tcl.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exposure {
    /// Property keys whose values reach Tcl.
    pub keys: BTreeSet<String>,
    /// Net names reach Tcl (`@pinlist`, `@@pin`, `@#n:net_name`).
    pub nets: bool,
    /// Anything may reach Tcl (`@tcleval(...)`).
    pub all: bool,
}

impl Exposure {
    fn add_tokens(&mut self, text: &str) {
        if text.contains("@tcleval") {
            self.all = true;
        }
        let mut rest = text;
        while let Some(at) = rest.find('@') {
            rest = &rest[at + 1..];
            if rest.starts_with(['@', '#']) || rest.starts_with("pinlist") {
                self.nets = true;
                continue;
            }
            let key: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !key.is_empty() {
                self.keys.insert(key);
            }
        }
    }

    fn covers(&self, key: &str) -> bool {
        self.all || self.keys.contains(key)
    }
}

/// Whether xschem evaluates a symbol attribute as Tcl: a `tcleval(...)`
/// value, one with an `@tcleval` token, or an attribute it runs as a
/// command.
fn evaluated(key: &str, value: &str) -> bool {
    value.starts_with("tcleval(")
        || value.contains("@tcleval")
        || matches!(key, "drc" | "tclcommand")
}

/// What `sym` substitutes into Tcl for an instance that has a `format` of
/// its own (`own_format`) or uses the symbol's.
pub fn exposure(sym: &XSymbol, own_format: bool) -> Exposure {
    let mut e = Exposure::default();
    for (k, v) in props::pairs(&sym.props) {
        if k == "template" || (own_format && k == "format") {
            continue;
        }
        if evaluated(&k, &v) {
            e.add_tokens(&v);
        }
    }
    for t in &sym.file.texts {
        if t.text.starts_with("tcleval(") || t.text.contains("@tcleval") {
            e.add_tokens(&t.text);
        }
    }
    for s in &sym.file.shapes {
        for (k, v) in props::pairs(&s.props) {
            if evaluated(&k, &v) {
                e.add_tokens(&v);
            }
        }
    }
    if sym.reference.starts_with("sg13g2_pr/") {
        e.keys.extend(IHP_EXPOSED.iter().map(|k| k.to_string()));
    }
    e
}

/// The keys an instance of `sym` may carry besides aispice's own: its
/// template's, less the reserved ones.
fn template_keys(sym: &XSymbol) -> BTreeSet<String> {
    props::pairs(&sym.template())
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| !RESERVED.contains(&k.as_str()))
        .collect()
}

/// Whether a key is a plain attribute name.
fn plain_key(k: &str) -> bool {
    let mut c = k.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic())
        && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A `format` aispice writes: `@name @pinlist`, optionally after
/// `@spiceprefix`, then node names, then `@value`. Nodes must be Tcl
/// literals without `@`, so the format has no token of their making.
fn our_format(f: &str) -> bool {
    let rest = f
        .strip_prefix("@spiceprefix@name @pinlist")
        .or_else(|| f.strip_prefix("@name @pinlist"));
    let Some(middle) = rest.and_then(|r| r.strip_suffix(" @value")) else {
        return false;
    };
    middle.split(' ').skip(1).all(tcl_literal) && (middle.is_empty() || middle.starts_with(' '))
}

/// Check one instance before it is written: `reference` is the symbol,
/// `sym` its definition as xschem will load it, `pairs` the properties and
/// `nets` the nets on its pins.
pub fn check_instance(
    reference: &str,
    sym: &XSymbol,
    pairs: &[(String, String)],
    nets: &[String],
) -> Result<(), String> {
    if !allowed_symbol(reference) {
        return Err(format!("symbol `{reference}` is not one an export writes"));
    }
    let own_format = pairs.iter().any(|(k, _)| k == "format");
    let exposed = exposure(sym, own_format);
    let allowed = template_keys(sym);
    let mut seen = BTreeSet::new();
    for (k, v) in pairs {
        if !plain_key(k)
            || !(BASE_KEYS.contains(&k.as_str()) || allowed.contains(k))
            || RESERVED.contains(&k.as_str())
        {
            return Err(format!("`{k}` is not a property an export writes"));
        }
        if !seen.insert(k.clone()) {
            return Err(format!("`{k}` is written twice"));
        }
        if let Some(why) = unwritable(v) {
            return Err(format!("the value of `{k}` cannot be written: {why}"));
        }
        if k == "format" && !our_format(v) {
            return Err(format!(
                "the format `{v}` is not one aispice writes, or names a node that is not plain text"
            ));
        }
        if exposed.covers(k) && !tcl_literal(v) {
            return Err(format!(
                "`{k}={v}` would be pasted into the Tcl of {reference}, and only letters, digits and . _ + - * / ( ) , : = ! < > % ^ ' stay text there"
            ));
        }
    }
    if (exposed.nets || exposed.all)
        && let Some(n) = nets.iter().find(|n| !tcl_literal(n))
    {
        return Err(format!(
            "net `{n}` would be pasted into the Tcl of {reference}, and only letters, digits and . _ + - * / ( ) , : = ! < > % ^ ' stay text there"
        ));
    }
    Ok(())
}

/// Check a net name before it is written as a label.
pub fn check_label(net: &str) -> Result<(), String> {
    match unwritable(net) {
        Some(why) => Err(format!("net `{net}` cannot be written as a label: {why}")),
        None if net.is_empty() => Err("an empty net name cannot be written".into()),
        None => Ok(()),
    }
}

/// Check a code block's SPICE text before it is written.
pub fn check_code(text: &str) -> Result<(), String> {
    match unwritable(text) {
        Some(why) => Err(format!("this SPICE text cannot be written: {why}")),
        None => Ok(()),
    }
}

/// Check a comment before it is written as a text.
pub fn check_text(text: &str) -> Result<(), String> {
    match unwritable(text) {
        Some(why) => Err(format!("this comment cannot be written: {why}")),
        None => Ok(()),
    }
}

/// Read the written file back as xschem reads it and compare it with what
/// the writer meant: the same records, each instance's properties read
/// back (first occurrence wins, quotes and escapes removed) to exactly the
/// pairs written, nothing evaluated, and only allowed symbols.
pub fn verify(text: &str, meant: &XFile, pairs: &[Vec<(String, String)>]) -> Result<(), String> {
    let back = file::parse(text).map_err(|e| format!("the file does not read back: {e}"))?;
    if back != *meant {
        return Err("the file does not read back to what was written".into());
    }
    if !(back.vhdl.is_empty()
        && back.symbol.is_empty()
        && back.verilog.is_empty()
        && back.spice.is_empty()
        && back.tedax.is_empty())
    {
        return Err("the file has global properties an export never writes".into());
    }
    if back.instances.len() != pairs.len() {
        return Err("the instances do not match what was written".into());
    }
    for (inst, written) in back.instances.iter().zip(pairs) {
        if inst.embedded.is_some() || !allowed_symbol(&inst.symbol) {
            return Err(format!(
                "instance of `{}` is not one an export writes",
                inst.symbol
            ));
        }
        if props::pairs(&inst.props) != *written {
            return Err(format!(
                "the properties of an instance of {} do not read back as written",
                inst.symbol
            ));
        }
        for (k, v) in written {
            let read = props::get(&inst.props, k);
            if read.as_deref() != Some(v.as_str()) || v.starts_with("tcleval(") {
                return Err(format!(
                    "`{k}` of an instance of {} does not read back as written",
                    inst.symbol
                ));
            }
        }
    }
    for w in &back.wires {
        if !w.props.is_empty() {
            return Err("a wire has properties an export never writes".into());
        }
    }
    for t in &back.texts {
        if !t.props.is_empty() || unwritable(&t.text).is_some() {
            return Err("a text is not one an export writes".into());
        }
    }
    if !back.shapes.is_empty() {
        return Err("the file has graphics an export never writes".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xschem::symbol::Origin;

    fn builtin(reference: &str) -> XSymbol {
        let (_, text) = crate::xschem::builtin::get(reference).unwrap();
        XSymbol::parse(reference, text, Origin::Builtin).unwrap()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn literals() {
        for ok in ["1k", "0.13u", "sg13_lv_nmos", "2*(wn)", "a,b", "x'"] {
            assert!(tcl_literal(ok), "{ok}");
        }
        for bad in [
            "",
            "a b",
            "[exec ls]",
            "{x}",
            "$x",
            "a\\b",
            "\"q",
            "a;b",
            "a\nb",
            "a#b",
            "@w",
        ] {
            assert!(!tcl_literal(bad), "{bad}");
        }
    }

    #[test]
    fn symbols_with_tcl_expose_what_they_substitute() {
        // vsource's format is Tcl over the name, pins and value; a format
        // of the instance's own takes it out.
        let v = builtin("devices/vsource.sym");
        let e = exposure(&v, false);
        assert!(e.nets && e.keys.contains("value") && e.keys.contains("name"));
        assert_eq!(exposure(&v, true), Exposure::default());
        // A resistor substitutes nothing into Tcl.
        assert_eq!(
            exposure(&builtin("devices/res.sym"), false),
            Exposure::default()
        );
        // IHP's devices: at least what their drc commands substitute.
        let m = exposure(&builtin("sg13g2_pr/sg13_lv_nmos.sym"), false);
        assert!(m.keys.contains("w") && m.keys.contains("model") && !m.keys.contains("value"));
    }

    #[test]
    fn instances_are_checked() {
        let res = builtin("devices/res.sym");
        let nets = ["a".to_string(), "b".to_string()];
        let ok = pairs(&[("name", "R1"), ("value", "{R} tc1=1m")]);
        assert_eq!(check_instance("devices/res.sym", &res, &ok, &nets), Ok(()));
        for bad in [
            pairs(&[
                ("name", "R1"),
                ("value", "tcleval(1k} ; exec touch S ; #{)"),
            ]),
            pairs(&[("name", "R1"), ("value", "TclEval(x)")]),
            pairs(&[("name", "R1"), ("schematic", "x.sch")]),
            pairs(&[("name", "R1"), ("spice_ignore", "true")]),
            pairs(&[("name", "R1"), ("format", "@tcleval([exec ls])")]),
            pairs(&[("name", "R1"), ("format", "@name @pinlist [x] @value")]),
            pairs(&[("name", "R1"), ("value", "1k"), ("value", "2k")]),
            pairs(&[("name", "R1"), ("na me", "x")]),
            pairs(&[("name", "R1"), ("value", "a\rb")]),
        ] {
            assert!(
                check_instance("devices/res.sym", &res, &bad, &nets).is_err(),
                "{bad:?}"
            );
        }
        assert!(check_instance("evil/res.sym", &res, &ok, &nets).is_err());
        // vsource with its own Tcl format: values and nets must be literal.
        let v = builtin("devices/vsource.sym");
        assert!(
            check_instance(
                "devices/vsource.sym",
                &v,
                &pairs(&[("name", "V1"), ("value", "3")]),
                &nets
            )
            .is_ok()
        );
        assert!(
            check_instance(
                "devices/vsource.sym",
                &v,
                &pairs(&[("name", "V1"), ("value", "dc 0 ac 1")]),
                &nets
            )
            .is_err()
        );
        let bus = ["d[0]".to_string(), "0".to_string()];
        assert!(
            check_instance(
                "devices/vsource.sym",
                &v,
                &pairs(&[("name", "V1"), ("value", "3")]),
                &bus
            )
            .is_err()
        );
        // With a plain format of its own, anything but tcleval stays text.
        let own = pairs(&[
            ("name", "V1"),
            ("format", "@name @pinlist @value"),
            ("value", "dc 0 ac 1 {x} [y] $z"),
        ]);
        assert_eq!(
            check_instance("devices/vsource.sym", &v, &own, &bus),
            Ok(())
        );
        // IHP: drc substitutes w; a parameter outside the template is refused.
        let m = builtin("sg13g2_pr/sg13_lv_nmos.sym");
        let mnets = ["d", "g", "s", "b"].map(String::from);
        assert!(
            check_instance(
                "sg13g2_pr/sg13_lv_nmos.sym",
                &m,
                &pairs(&[("name", "M1"), ("w", "1u")]),
                &mnets
            )
            .is_ok()
        );
        assert!(
            check_instance(
                "sg13g2_pr/sg13_lv_nmos.sym",
                &m,
                &pairs(&[("name", "M1"), ("w", "1u;exec")]),
                &mnets
            )
            .is_err()
        );
        assert!(
            check_instance(
                "sg13g2_pr/sg13_lv_nmos.sym",
                &m,
                &pairs(&[("name", "M1"), ("drc", "x")]),
                &mnets
            )
            .is_err()
        );
        assert!(
            check_instance(
                "sg13g2_pr/sg13_lv_nmos.sym",
                &m,
                &pairs(&[("name", "M1"), ("as", "0")]),
                &mnets
            )
            .is_err()
        );
        assert!(
            check_instance("sg13g2_pr/nope.sym", &m, &pairs(&[("name", "M1")]), &mnets).is_err()
        );
    }

    #[test]
    fn formats_are_only_ours() {
        for ok in [
            "@name @pinlist @value",
            "@spiceprefix@name @pinlist @value",
            "@name @pinlist 0 @value",
            "@spiceprefix@name @pinlist sub n1 @value",
        ] {
            assert!(our_format(ok), "{ok}");
        }
        for bad in [
            "tcleval(@name @pinlist @value)",
            "@name @pinlist @@x @value",
            "@name @pinlist a@b @value",
            "@name @pinlist [exec ls] @value",
            "@name @pinlist  @value",
            "@name @pinlist @value @tcleval(x)",
        ] {
            assert!(!our_format(bad), "{bad}");
        }
    }

    #[test]
    fn labels_code_and_texts() {
        assert!(check_label("out").is_ok());
        assert!(check_label("tcleval(x)").is_err());
        assert!(check_label("").is_err());
        assert!(check_code(".param r={1k*2} ; x").is_ok());
        assert!(check_code(".param x=1\n.op").is_ok());
        assert!(check_code("tcleval([exec ls])").is_err());
        assert!(check_text("a note [with] {braces}").is_ok());
        assert!(check_text("tcleval(x)").is_err());
    }
}

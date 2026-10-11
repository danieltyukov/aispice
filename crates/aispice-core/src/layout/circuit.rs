//! From netlist lines to things that can be drawn: each element becomes a
//! symbol with its pins on nets, or, when no symbol can represent it exactly,
//! a SPICE line kept inside a directive. Then the nets are sorted into
//! ground, supply rails and signals, and the signal flow is worked out.

use super::geom::{Dir, pin_facing_r0};
use crate::geometry::Point;
use crate::netlist::{Element, Line, Netlist};
use crate::symbol::{SymbolDef, SymbolLibrary};
use std::collections::HashMap;
use std::sync::Arc;

/// What a part is, as far as drawing conventions care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// R, C, L: two terminals, drawn along the signal path or as a shunt.
    Passive,
    Diode,
    /// Independent V or I source.
    Source,
    /// Two-terminal dependent or behavioural source (B, F, H, W).
    Source2,
    Bjt {
        p: bool,
    },
    /// MOSFET or JFET.
    Fet {
        p: bool,
    },
    OpAmp,
    OpAmp5,
    /// E or G with a control pair.
    Controlled,
    Switch,
    Tline,
    /// A library symbol matched by subcircuit name.
    Sub,
}

impl Kind {
    pub fn two_terminal(self) -> bool {
        matches!(
            self,
            Kind::Passive | Kind::Diode | Kind::Source | Kind::Source2
        )
    }

    pub fn transistor(self) -> bool {
        matches!(self, Kind::Bjt { .. } | Kind::Fet { .. })
    }

    /// Parts with an input side and an output side.
    pub fn active(self) -> bool {
        matches!(
            self,
            Kind::Bjt { .. } | Kind::Fet { .. } | Kind::OpAmp | Kind::OpAmp5 | Kind::Controlled
        )
    }
}

/// How a pin takes part in the signal flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PinRole {
    /// Both ends of a two-terminal part.
    Pass,
    /// Base, gate, op-amp input, control pin.
    Input,
    /// Collector, drain, op-amp output.
    Output,
    /// Emitter or source: an output too, but the one tied toward the lower
    /// rail.
    Common,
    /// Supply pin of an op-amp, bulk of a MOSFET.
    Supply,
}

#[derive(Debug, Clone)]
pub(crate) struct DevPin {
    /// Position in the symbol's own frame.
    pub at: Point,
    pub facing: Dir,
    pub net: usize,
    pub role: PinRole,
}

#[derive(Debug, Clone)]
pub(crate) struct Device {
    /// The InstName written into the schematic (`U1`).
    pub inst: String,
    pub symbol: String,
    pub def: Arc<SymbolDef>,
    pub kind: Kind,
    pub attrs: Vec<(String, String)>,
    pub pins: Vec<DevPin>,
    /// For sources: driven by something other than a constant.
    pub signal: bool,
    /// For DC sources, the value when it is a plain number.
    pub dc: Option<f64>,
}

impl Device {
    pub fn nets(&self) -> impl Iterator<Item = usize> + '_ {
        self.pins.iter().map(|p| p.net)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NetKind {
    Ground,
    /// A supply rail, drawn with labels at every pin. `positive` rails go at
    /// the top, negative ones at the bottom.
    Rail {
        positive: bool,
    },
    Signal,
}

#[derive(Debug, Clone)]
pub(crate) struct NetInfo {
    pub name: String,
    pub kind: NetKind,
    /// The schematic must carry this name on a label for the netlist to
    /// round-trip.
    pub named: bool,
    /// Signal-flow distance from the input, if reached.
    pub depth: Option<u32>,
}

/// An element carried as a SPICE line in a directive.
#[derive(Debug, Clone)]
pub(crate) struct Fallback {
    pub line: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Circuit {
    pub devices: Vec<Device>,
    pub nets: Vec<NetInfo>,
    pub fallbacks: Vec<Fallback>,
    /// Directive texts in netlist order, already grouped.
    pub texts: Vec<String>,
    pub warnings: Vec<String>,
    /// The main input source, if any.
    pub input: Option<usize>,
    /// The output net.
    pub output: Option<usize>,
    /// Two-terminal parts that close a loop around an active part (op-amp
    /// feedback, Miller capacitors). They are placed after both ends exist.
    pub feedback: Vec<bool>,
}

impl Circuit {
    pub fn is_signal(&self, net: usize) -> bool {
        self.nets[net].kind == NetKind::Signal
    }

    /// Whether `n` is a rail called `upper` (an upper-cased label).
    pub fn is_rail_name(&self, n: &NetInfo, upper: &str) -> bool {
        matches!(n.kind, NetKind::Rail { .. }) && n.name.eq_ignore_ascii_case(upper)
    }
}

/// Split on whitespace keeping parenthesised and braced groups whole.
pub(crate) fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' | '{' | '[' => {
                depth += 1;
                cur.push(c);
            }
            ')' | '}' | ']' => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && depth <= 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Value text as the comparison sees it.
pub(crate) fn norm_value(s: &str) -> String {
    s.replace(['\u{b5}', '\u{3bc}'], "u")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Net names LTspice generates for unlabelled nets.
pub(crate) fn auto_named(n: &str) -> bool {
    let up = n.to_ascii_uppercase();
    (up.starts_with('N') || up.starts_with('P'))
        && up.len() == 4
        && up[1..].bytes().all(|b| b.is_ascii_digit())
        || up.starts_with("NC_")
}

fn is_ground_name(n: &str) -> bool {
    n == "0" || n.eq_ignore_ascii_case("gnd")
}

/// Supply-rail names, with their polarity.
fn rail_name(n: &str) -> Option<bool> {
    let l = n.to_ascii_lowercase();
    let pos = [
        "vcc", "vdd", "v+", "vpos", "vp", "vplus", "vsupply", "vs+", "vbat", "vin+",
    ];
    let neg = ["vee", "vss", "v-", "vneg", "vn", "vminus", "vs-"];
    if pos.contains(&l.as_str()) || l.starts_with("vcc") || l.starts_with("vdd") {
        Some(true)
    } else if neg.contains(&l.as_str()) || l.starts_with("vee") || l.starts_with("vss") {
        Some(false)
    } else {
        None
    }
}

/// `.model` names to their device type, upper-cased (`NPN`, `PMOS`, ...).
fn model_types(netlist: &Netlist) -> HashMap<String, (String, String)> {
    let mut out = HashMap::new();
    for d in netlist.directives() {
        let t = tokens(d);
        if t.len() >= 3 && t[0].eq_ignore_ascii_case(".model") {
            let ty: String = t[2].split('(').next().unwrap_or("").to_ascii_uppercase();
            out.insert(t[1].to_ascii_uppercase(), (ty, d.to_ascii_lowercase()));
        }
    }
    out
}

/// The rest the netlister will write for a symbol with these attributes:
/// each of Value, Value2, SpiceLine and SpiceLine2 from the instance or the
/// symbol default, joined. Mirrors `netlist::build`, which mirrors LTspice.
fn built_rest(attrs: &[(String, String)], def: &SymbolDef) -> String {
    let field = |key: &str| {
        attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
            .or_else(|| def.attr(key))
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    ["Value", "Value2", "SpiceLine", "SpiceLine2"]
        .into_iter()
        .filter_map(field)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a source value is a constant (a number, `DC x`, or a parameter),
/// possibly with series resistance or capacitance settings.
fn source_is_dc(rest: &str) -> (bool, Option<f64>) {
    let t = tokens(rest);
    let mut value = None;
    let mut dc = true;
    let mut i = 0;
    while i < t.len() {
        let w = t[i].to_ascii_lowercase();
        if w == "dc" {
            if let Some(v) = t.get(i + 1) {
                value = crate::units::parse(v);
            }
            i += 2;
            continue;
        }
        if w.contains('=') {
            // Rser=, Cpar=, and the like.
            i += 1;
            continue;
        }
        if let Some(v) = crate::units::parse(&w) {
            value = value.or(Some(v));
        } else if w.starts_with('{') {
            // A parameter: constant, value unknown.
        } else {
            dc = false;
        }
        i += 1;
    }
    (dc, value)
}

fn diode_symbol(model: &str) -> &'static str {
    let m = model.to_ascii_lowercase();
    let starts = |ps: &[&str]| ps.iter().any(|p| m.starts_with(p));
    if m.contains("zener") || starts(&["bzx", "bzt", "1n47", "1n52", "1n53", "dz", "mmsz", "zd"]) {
        "zener"
    } else if m.contains("schottky") || starts(&["bat", "1n58", "mbr", "ss1", "ss2", "ss3", "stps"])
    {
        "schottky"
    } else if m.contains("led") {
        "LED"
    } else {
        "diode"
    }
}

/// Whether a subcircuit name or its ports look like an op-amp.
fn looks_like_opamp(name: &str, ports: Option<&[String]>) -> bool {
    let n = name.to_ascii_lowercase();
    if n.contains("op") || n.contains("amp") {
        return true;
    }
    if let Some(ports) = ports {
        let p: Vec<String> = ports.iter().map(|p| p.to_ascii_lowercase()).collect();
        let has = |s: &str| p.iter().any(|x| x.contains(s));
        return has("out") && (has("in") || has("+"));
    }
    false
}

struct Mapping<'a> {
    lib: &'a SymbolLibrary,
    models: HashMap<String, (String, String)>,
    subckts: HashMap<String, Vec<String>>,
}

enum Mapped {
    Device {
        symbol: String,
        kind: Kind,
        attrs: Vec<(String, String)>,
        /// Element node indices, in the symbol's SPICE pin order.
        nodes: Vec<usize>,
        roles: Vec<PinRole>,
        inst: String,
    },
    Fallback(String),
}

impl Mapping<'_> {
    fn model(&self, name: &str) -> Option<&(String, String)> {
        self.models.get(&name.to_ascii_uppercase())
    }

    fn map(&self, e: &Element) -> Mapped {
        use PinRole::*;
        let letter = e.letter();
        let rest = e.rest.trim().to_string();
        let first = e.first_word().unwrap_or("").to_string();
        let n = e.nodes.len();
        let all: Vec<usize> = (0..n).collect();
        let value = |v: &str| vec![("Value".to_string(), v.to_string())];
        let dev = |symbol: &str, kind: Kind, attrs, nodes: Vec<usize>, roles: Vec<PinRole>| {
            Mapped::Device {
                symbol: symbol.to_string(),
                kind,
                attrs,
                nodes,
                roles,
                inst: e.name.clone(),
            }
        };
        match letter {
            'R' | 'C' | 'L' if n == 2 => {
                let symbol = match letter {
                    'R' => "res",
                    'C' => "cap",
                    _ => "ind",
                };
                dev(symbol, Kind::Passive, value(&rest), all, vec![Pass, Pass])
            }
            'V' | 'I' if n == 2 => {
                let t = tokens(&rest);
                let ac = t
                    .iter()
                    .position(|w| w.eq_ignore_ascii_case("ac"))
                    .filter(|&i| i > 0);
                let mut attrs = match ac {
                    Some(i) => vec![
                        ("Value".to_string(), t[..i].join(" ")),
                        ("Value2".to_string(), t[i..].join(" ")),
                    ],
                    None => value(&rest),
                };
                if attrs[0].1.is_empty() {
                    attrs = value(&rest);
                }
                let sine = letter == 'V'
                    && t.first().is_some_and(|w| {
                        let l = w.to_ascii_lowercase();
                        l.starts_with("sin") || l.starts_with("sffm")
                    });
                let symbol = match (letter, sine) {
                    ('V', true) => "Misc\\signal",
                    ('V', false) => "voltage",
                    _ => "current",
                };
                dev(symbol, Kind::Source, attrs, all, vec![Pass, Pass])
            }
            'D' if n == 2 => dev(
                diode_symbol(&first),
                Kind::Diode,
                value(&rest),
                all,
                vec![Pass, Pass],
            ),
            'Q' if n == 3 || (n == 4 && e.nodes[3] == "0") => {
                let p = self.model(&first).is_some_and(|(t, _)| t == "PNP")
                    || first.eq_ignore_ascii_case("pnp");
                dev(
                    if p { "pnp" } else { "npn" },
                    Kind::Bjt { p },
                    value(&rest),
                    vec![0, 1, 2],
                    vec![Output, Input, Common],
                )
            }
            'M' if n == 4 => {
                let p = match self.model(&first) {
                    Some((t, text)) => t == "PMOS" || (t == "VDMOS" && text.contains("pchan")),
                    None => {
                        let l = first.to_ascii_lowercase();
                        l.contains("pmos") || l.starts_with("p")
                    }
                };
                let three = e.nodes[3].eq_ignore_ascii_case(&e.nodes[2]);
                let (symbol, nodes, roles) = match (p, three) {
                    (false, true) => ("nmos", vec![0, 1, 2], vec![Output, Input, Common]),
                    (true, true) => ("pmos", vec![0, 1, 2], vec![Output, Input, Common]),
                    (false, false) => (
                        "nmos4",
                        vec![0, 1, 2, 3],
                        vec![Output, Input, Common, Supply],
                    ),
                    (true, false) => (
                        "pmos4",
                        vec![0, 1, 2, 3],
                        vec![Output, Input, Common, Supply],
                    ),
                };
                dev(symbol, Kind::Fet { p }, value(&rest), nodes, roles)
            }
            'J' if n == 3 => {
                let p = self.model(&first).is_some_and(|(t, _)| t == "PJF")
                    || first.eq_ignore_ascii_case("pjf");
                dev(
                    if p { "pjf" } else { "njf" },
                    Kind::Fet { p },
                    value(&rest),
                    all,
                    vec![Output, Input, Common],
                )
            }
            'E' | 'G' if n == 4 => dev(
                if letter == 'E' { "e" } else { "g" },
                Kind::Controlled,
                value(&rest),
                all,
                vec![Output, Output, Input, Input],
            ),
            'F' | 'H' if n == 2 => dev(
                if letter == 'F' { "f" } else { "h" },
                Kind::Source2,
                value(&rest),
                all,
                vec![Pass, Pass],
            ),
            'B' if n == 2 => {
                let symbol = if rest.to_ascii_lowercase().starts_with('i') {
                    "bi"
                } else {
                    "bv"
                };
                dev(symbol, Kind::Source2, value(&rest), all, vec![Pass, Pass])
            }
            'W' if n == 2 => dev("csw", Kind::Source2, value(&rest), all, vec![Pass, Pass]),
            'S' if n == 4 => dev(
                "sw",
                Kind::Switch,
                value(&rest),
                all,
                vec![Pass, Pass, Input, Input],
            ),
            'T' if n == 4 => dev(
                "tline",
                Kind::Tline,
                value(&rest),
                all,
                vec![Pass, Pass, Pass, Pass],
            ),
            'X' => self.map_sub(e, &first, &rest),
            'K' => Mapped::Fallback("a coupling has no symbol; it is kept as a SPICE line".into()),
            _ => Mapped::Fallback(format!(
                "no symbol for `{}` lines with {n} nodes; kept as a SPICE line",
                letter
            )),
        }
    }

    fn map_sub(&self, e: &Element, sub: &str, rest: &str) -> Mapped {
        use PinRole::*;
        let n = e.nodes.len();
        let inst = e.name[1..].to_string();
        if inst.is_empty() {
            return Mapped::Fallback("a subcircuit call needs a name after X".into());
        }
        let params: Vec<String> = tokens(rest).into_iter().skip(1).collect();
        let ports = self.subckts.get(&sub.to_ascii_uppercase());
        if n == 3 && sub.eq_ignore_ascii_case("opamp") {
            let mut attrs = vec![("Value".to_string(), sub.to_string())];
            if params.len() >= 2 {
                attrs.push(("SpiceLine".to_string(), params[0].clone()));
                attrs.push(("SpiceLine2".to_string(), params[1..].join(" ")));
            }
            return Mapped::Device {
                symbol: "OpAmps\\opamp".into(),
                kind: Kind::OpAmp,
                attrs,
                nodes: vec![0, 1, 2],
                roles: vec![Input, Input, Output],
                inst,
            };
        }
        if n == 5 && looks_like_opamp(sub, ports.map(Vec::as_slice)) {
            return Mapped::Device {
                symbol: "OpAmps\\opamp2".into(),
                kind: Kind::OpAmp5,
                attrs: vec![("Value".to_string(), rest.to_string())],
                nodes: (0..5).collect(),
                roles: vec![Input, Input, Supply, Supply, Output],
                inst,
            };
        }
        // A PDK device: its own symbol, the parameters in SpiceLine, as the
        // netlister writes them back.
        if let Some(device) = crate::pdk::device(sub)
            && let Some(symbol) = device.symbol
            && device.pins.len() == n
        {
            return Mapped::Device {
                symbol: symbol.replace('/', "\\"),
                kind: Kind::Sub,
                attrs: vec![("SpiceLine".to_string(), params.join(" "))],
                nodes: (0..n).collect(),
                roles: vec![Pass; n],
                inst,
            };
        }
        // An LTspice library part: a symbol named after the subcircuit.
        if let Ok((def, _)) = self.lib.resolve(sub)
            && def.prefix().to_ascii_uppercase().starts_with('X')
            && def.pins.len() == n
            && def.kind == crate::symbol::SymbolType::Cell
        {
            return Mapped::Device {
                symbol: sub.to_string(),
                kind: Kind::Sub,
                attrs: vec![("Value".to_string(), rest.to_string())],
                nodes: (0..n).collect(),
                roles: vec![Pass; n],
                inst,
            };
        }
        Mapped::Fallback(format!(
            "no symbol for subcircuit `{sub}` with {n} pins; the call is kept as a SPICE line"
        ))
    }
}

/// LTspice's text encoding for a directive: line breaks as `\n`, backslashes
/// doubled, no control characters.
pub(crate) fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace("\r\n", "\n")
        .replace(['\r', '\u{85}', '\u{2028}', '\u{2029}'], "\n")
        .replace('\t', " ")
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect::<String>()
        .replace('\n', "\\n")
}

fn element_line(e: &Element) -> String {
    let mut s = e.name.clone();
    for n in &e.nodes {
        s.push(' ');
        s.push_str(n);
    }
    if !e.rest.is_empty() {
        s.push(' ');
        s.push_str(&e.rest);
    }
    s
}

/// A subcircuit definition as directive text.
pub(crate) fn subckt_text(s: &crate::netlist::Subckt) -> String {
    let wrapped = Netlist {
        title: String::new(),
        items: vec![Line::Subckt(s.clone())],
    };
    let text = crate::netlist::write(&wrapped);
    text.lines()
        .skip(1)
        .filter(|l| *l != ".end")
        .collect::<Vec<_>>()
        .join("\n")
}

/// Map every element and directive of a netlist.
pub(crate) fn build(netlist: &Netlist, lib: &SymbolLibrary) -> Circuit {
    let mapping = Mapping {
        lib,
        models: model_types(netlist),
        subckts: netlist
            .subckts()
            .map(|s| (s.name.to_ascii_uppercase(), s.ports.clone()))
            .collect(),
    };
    let mut c = Circuit::default();
    let mut net_ids: HashMap<String, usize> = HashMap::new();
    let mut net_of = |name: &str, c: &mut Circuit| -> usize {
        let name = if is_ground_name(name) { "0" } else { name };
        let key = name.to_ascii_uppercase();
        *net_ids.entry(key).or_insert_with(|| {
            c.nets.push(NetInfo {
                name: name.to_string(),
                kind: if name == "0" {
                    NetKind::Ground
                } else {
                    NetKind::Signal
                },
                named: false,
                depth: None,
            });
            c.nets.len() - 1
        })
    };
    let mut models = Vec::new();
    for item in &netlist.items {
        match item {
            Line::Element(e) => {
                let ids: Vec<usize> = e.nodes.iter().map(|n| net_of(n, &mut c)).collect();
                match mapping.map(e) {
                    Mapped::Device {
                        symbol,
                        kind,
                        attrs,
                        nodes,
                        roles,
                        inst,
                    } => {
                        let def = match lib.resolve(&symbol) {
                            Ok((d, _)) => d,
                            Err(err) => {
                                c.warnings
                                    .push(format!("{}: {err}; kept as a SPICE line", e.name));
                                c.fallbacks.push(Fallback {
                                    line: element_line(e),
                                });
                                continue;
                            }
                        };
                        let order = def.pins_in_spice_order();
                        let rest_ok = norm_value(&built_rest(&attrs, &def)) == norm_value(&e.rest);
                        if order.len() != nodes.len() || !rest_ok {
                            c.warnings.push(format!(
                                "{}: the `{symbol}` symbol cannot reproduce this line exactly; kept as a SPICE line",
                                e.name
                            ));
                            c.fallbacks.push(Fallback {
                                line: element_line(e),
                            });
                            continue;
                        }
                        let pins = order
                            .iter()
                            .zip(nodes.iter().zip(&roles))
                            .map(|(p, (&node, &role))| DevPin {
                                at: p.at,
                                facing: pin_facing_r0(&def, p),
                                net: ids[node],
                                role,
                            })
                            .collect();
                        let (dc, dcv) = if kind == Kind::Source {
                            source_is_dc(&e.rest)
                        } else {
                            (false, None)
                        };
                        c.devices.push(Device {
                            inst,
                            symbol,
                            def,
                            kind,
                            attrs,
                            pins,
                            signal: kind == Kind::Source && !dc,
                            dc: dcv,
                        });
                    }
                    Mapped::Fallback(reason) => {
                        c.warnings.push(format!("{}: {reason}", e.name));
                        c.fallbacks.push(Fallback {
                            line: element_line(e),
                        });
                    }
                }
            }
            Line::Directive { text } => {
                if text.to_ascii_lowercase().starts_with(".model") {
                    models.push(text.clone());
                } else if !text.eq_ignore_ascii_case(".backanno") {
                    c.texts.push(text.clone());
                }
            }
            Line::Subckt(s) => c.texts.push(subckt_text(s)),
            Line::Comment { .. } => {}
        }
    }
    if !models.is_empty() {
        c.texts.push(models.join("\n"));
    }
    for f in &c.fallbacks {
        c.texts.push(f.line.clone());
    }
    analyse(&mut c);
    c
}

/// Sort nets into ground, rails and signals; decide which need labels; find
/// the input, the output and the signal-flow depth; mark feedback parts.
fn analyse(c: &mut Circuit) {
    let ground = c.nets.iter().position(|n| n.kind == NetKind::Ground);

    // Rails: nets held by a constant source to ground that are named like a
    // supply or feed transistor and op-amp supply pins.
    let mut feeds_supply = vec![false; c.nets.len()];
    for d in &c.devices {
        for p in &d.pins {
            let supply_pin = match d.kind {
                Kind::Bjt { .. } | Kind::Fet { .. } => {
                    matches!(p.role, PinRole::Output | PinRole::Common | PinRole::Supply)
                }
                Kind::OpAmp5 => p.role == PinRole::Supply,
                _ => false,
            };
            if supply_pin {
                feeds_supply[p.net] = true;
            }
        }
    }
    for i in 0..c.devices.len() {
        let d = &c.devices[i];
        if d.kind != Kind::Source || d.signal || !d.symbol.eq_ignore_ascii_case("voltage") {
            continue;
        }
        let (a, b) = (d.pins[0].net, d.pins[1].net);
        let (rail, plus_at_rail) = match (Some(a) == ground, Some(b) == ground) {
            (false, true) => (a, true),
            (true, false) => (b, false),
            _ => continue,
        };
        let by_name = rail_name(&c.nets[rail].name);
        if by_name.is_none() && !feeds_supply[rail] {
            continue;
        }
        let positive = by_name.unwrap_or_else(|| {
            let v = d.dc.unwrap_or(1.0);
            if plus_at_rail { v >= 0.0 } else { v < 0.0 }
        });
        c.nets[rail].kind = NetKind::Rail { positive };
    }

    // Names that must appear on the sheet. If a directive or a kept SPICE
    // line refers to a net by an LTspice-style automatic name, every net is
    // labelled so LTspice's own numbering cannot collide with it.
    let mut texts_upper = c.texts.join("\n").to_ascii_uppercase();
    texts_upper.push('\n');
    let word_in_texts = |name: &str| {
        let up = name.to_ascii_uppercase();
        texts_upper.match_indices(&up).any(|(i, _)| {
            let before = texts_upper[..i].chars().next_back();
            let after = texts_upper[i + up.len()..].chars().next();
            let sep =
                |ch: Option<char>| ch.is_none_or(|ch| !(ch.is_ascii_alphanumeric() || ch == '_'));
            sep(before) && sep(after)
        })
    };
    let force_all = c
        .nets
        .iter()
        .any(|n| n.kind != NetKind::Ground && auto_named(&n.name) && word_in_texts(&n.name));
    for n in c.nets.iter_mut() {
        n.named = n.kind != NetKind::Ground && (force_all || !auto_named(&n.name));
    }

    // The input: a source to ground driving a signal net, preferring names
    // like `in`, then time-varying sources, then the one reaching most parts.
    let signal_sources: Vec<usize> = (0..c.devices.len())
        .filter(|&i| {
            let d = &c.devices[i];
            matches!(d.kind, Kind::Source)
                && d.pins.iter().any(|p| Some(p.net) == ground)
                && d.pins.iter().any(|p| c.is_signal(p.net))
        })
        .collect();
    let reach = |start: usize, c: &Circuit| -> usize {
        let dist = depths(c, &[start]);
        dist.iter().filter(|d| d.is_some()).count()
    };
    let input = signal_sources.iter().copied().max_by_key(|&i| {
        let d = &c.devices[i];
        let net = d
            .pins
            .iter()
            .find(|p| c.is_signal(p.net))
            .map(|p| p.net)
            .expect("filtered");
        let name = c.nets[net].name.to_ascii_lowercase();
        let named_in = ["in", "vin", "input", "inp", "in+", "vi", "sig"].contains(&name.as_str())
            || name.starts_with("in");
        (
            named_in as usize,
            d.signal as usize,
            reach(net, c),
            usize::MAX - i,
        )
    });
    c.input = input;
    let start: Vec<usize> = match input {
        Some(i) => c.devices[i]
            .pins
            .iter()
            .filter(|p| c.is_signal(p.net))
            .map(|p| p.net)
            .collect(),
        None => c
            .nets
            .iter()
            .position(|n| n.kind == NetKind::Signal)
            .into_iter()
            .collect(),
    };
    let dist = depths(c, &start);
    for (n, d) in c.nets.iter_mut().zip(&dist) {
        n.depth = *d;
    }

    // The output: a net named like one, else the deepest signal net.
    let named_out = c.nets.iter().position(|n| {
        let l = n.name.to_ascii_lowercase();
        n.kind == NetKind::Signal
            && (l == "out" || l == "vout" || l == "output" || l.starts_with("out"))
    });
    c.output = named_out.or_else(|| {
        (0..c.nets.len())
            .filter(|&n| c.is_signal(n) && c.nets[n].depth.is_some())
            .max_by_key(|&n| (c.nets[n].depth, usize::MAX - n))
    });

    c.feedback = (0..c.devices.len()).map(|d| is_feedback(c, d)).collect();
}

/// Breadth-first distance over signal nets, crossing every part. Ground and
/// rails are not crossed: everything touches them.
pub(crate) fn depths(c: &Circuit, start: &[usize]) -> Vec<Option<u32>> {
    let mut dist = vec![None; c.nets.len()];
    let mut queue = std::collections::VecDeque::new();
    for &s in start {
        if dist[s].is_none() {
            dist[s] = Some(0);
            queue.push_back(s);
        }
    }
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); c.nets.len()];
    for d in &c.devices {
        let nets: Vec<usize> = d.nets().filter(|&n| c.is_signal(n)).collect();
        for &a in &nets {
            for &b in &nets {
                if a != b {
                    adj[a].push(b);
                }
            }
        }
    }
    while let Some(n) = queue.pop_front() {
        let dn = dist[n].expect("queued nets have a distance");
        for &m in &adj[n] {
            if dist[m].is_none() {
                dist[m] = Some(dn + 1);
                queue.push_back(m);
            }
        }
    }
    dist
}

/// A two-terminal part between signal nets `a` and `b` closes a loop around
/// an active part when one end is that part's output and the other end lies
/// upstream of its input, reached without going through the part itself.
fn is_feedback(c: &Circuit, d: usize) -> bool {
    let dev = &c.devices[d];
    if !dev.kind.two_terminal() || dev.kind == Kind::Source {
        return false;
    }
    let (a, b) = (dev.pins[0].net, dev.pins[1].net);
    if a == b || !c.is_signal(a) || !c.is_signal(b) {
        return false;
    }
    for (out, other) in [(a, b), (b, a)] {
        for (i, act) in c.devices.iter().enumerate() {
            if i == d || !act.kind.active() {
                continue;
            }
            let is_out = act
                .pins
                .iter()
                .any(|p| p.net == out && p.role == PinRole::Output);
            if !is_out {
                continue;
            }
            let inputs: Vec<usize> = act
                .pins
                .iter()
                .filter(|p| p.role == PinRole::Input && c.is_signal(p.net))
                .map(|p| p.net)
                .collect();
            if inputs.contains(&other) || upstream(c, &inputs, other, &[d, i], 3) {
                return true;
            }
        }
    }
    false
}

/// Whether `target` can be reached from `from` within `hops` parts, walking
/// through passive parts and backwards through active ones, avoiding `skip`.
fn upstream(c: &Circuit, from: &[usize], target: usize, skip: &[usize], hops: u32) -> bool {
    let mut frontier: Vec<usize> = from.to_vec();
    let mut seen: Vec<usize> = from.to_vec();
    for _ in 0..hops {
        let mut next = Vec::new();
        for &n in &frontier {
            for (i, dev) in c.devices.iter().enumerate() {
                if skip.contains(&i) || !dev.nets().any(|x| x == n) {
                    continue;
                }
                let others: Vec<usize> =
                    if dev.kind.active() {
                        // Backwards only: from an output to the inputs.
                        if dev.pins.iter().any(|p| {
                            p.net == n && matches!(p.role, PinRole::Output | PinRole::Common)
                        }) {
                            dev.pins
                                .iter()
                                .filter(|p| p.role == PinRole::Input)
                                .map(|p| p.net)
                                .collect()
                        } else {
                            Vec::new()
                        }
                    } else if dev.kind == Kind::Source {
                        Vec::new()
                    } else {
                        dev.nets().filter(|&x| x != n).collect()
                    };
                for m in others {
                    if !c.is_signal(m) || seen.contains(&m) {
                        continue;
                    }
                    if m == target {
                        return true;
                    }
                    seen.push(m);
                    next.push(m);
                }
            }
        }
        frontier = next;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netlist::parse;

    fn circuit(text: &str) -> Circuit {
        build(&parse(text), &SymbolLibrary::builtin_only())
    }

    #[test]
    fn maps_common_parts() {
        let c = circuit(
            "t\nV1 in 0 SINE(0 1 1k) AC 1\nR1 in out 1k\nC1 out 0 1u\nQ1 c b e 2N3906\nM1 d g s s NMOS\nM2 d g s b PMOS\nXU1 a b c opamp Aol=100K GBW=10Meg\nK1 L1 L2 1\n.model 2N3906 PNP\n.tran 1m\n",
        );
        let sym = |n: &str| {
            c.devices
                .iter()
                .find(|d| d.inst == n)
                .map(|d| d.symbol.clone())
                .unwrap_or_default()
        };
        assert_eq!(sym("V1"), "Misc\\signal");
        assert_eq!(sym("Q1"), "pnp");
        assert_eq!(sym("M1"), "nmos");
        assert_eq!(sym("M2"), "pmos4");
        assert_eq!(sym("U1"), "OpAmps\\opamp");
        let v1 = c.devices.iter().find(|d| d.inst == "V1").unwrap();
        assert_eq!(v1.attrs[1], ("Value2".to_string(), "AC 1".to_string()));
        assert_eq!(c.fallbacks.len(), 1);
        assert!(c.texts.iter().any(|t| t == "K1 L1 L2 1"));
        assert_eq!(c.nets[c.output.unwrap()].name, "out");
    }

    #[test]
    fn opamp_without_parameters_is_kept_as_a_line() {
        // LTspice would append the symbol's default Aol and GBW.
        let c = circuit("t\nXU1 a b c opamp\n");
        assert!(c.devices.is_empty());
        assert_eq!(c.fallbacks.len(), 1);
    }

    #[test]
    fn rails_and_feedback() {
        let c = circuit(
            "t\nV1 vcc 0 15\nV2 0 vee 15\nV3 in 0 SINE(0 1 1k)\nR1 in inv 1k\nR2 inv out 10k\nXU1 0 inv vcc vee out myop\n.subckt myop p n vp vn o\n.ends myop\n",
        );
        let kind = |n: &str| c.nets.iter().find(|x| x.name == n).unwrap().kind;
        assert_eq!(kind("vcc"), NetKind::Rail { positive: true });
        assert_eq!(kind("vee"), NetKind::Rail { positive: false });
        assert_eq!(kind("in"), NetKind::Signal);
        let r2 = c.devices.iter().position(|d| d.inst == "R2").unwrap();
        let r1 = c.devices.iter().position(|d| d.inst == "R1").unwrap();
        assert!(c.feedback[r2]);
        assert!(!c.feedback[r1]);
        assert_eq!(c.devices[c.input.unwrap()].inst, "V3");
    }
}

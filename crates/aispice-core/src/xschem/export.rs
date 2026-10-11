//! LTspice `.asc` to xschem schematic.
//!
//! The `.asc` is netlisted, and the netlist is redrawn by the schematic
//! layout engine with a symbol set shaped like xschem's: each supported
//! symbol keeps its LTspice attributes, pin names and pin order, but has
//! xschem's pin positions (scaled by 1.6, so xschem's 10-unit grid lands on
//! LTspice's 16-unit one) and xschem's body. The router then ends every wire
//! exactly on an xschem pin, and the layout's own check proves the drawing
//! netlists back. Coordinates are scaled by 0.625 into the `.sch`.
//!
//! Redrawing rather than copying the `.asc` coordinates is deliberate:
//! LTspice and xschem symbols differ in proportions, so patching pins with
//! stub wires would land them mid-wire or on other parts. The cost is that
//! the xschem drawing does not keep the `.asc` arrangement.
//!
//! Generic parts use xschem's `devices/` symbols, IHP SG13G2 parts IHP's
//! `sg13g2_pr` symbols (a poly resistor's body terminal goes in `body=`,
//! with a label on the sheet). Directives become code blocks. Anything else
//! (op-amp macro models, switches, library parts) stays a SPICE line in a
//! code block, with a finding; the layout labels every net that line uses,
//! so it stays connected.
//!
//! Every instance is checked as it is written: the `.sch` is netlisted with
//! aispice's xschem netlister and compared with the drawing. An instance
//! whose symbol's `format` cannot reproduce its line (a MOSFET without `w`
//! and `l`, which the symbol's template would add) gets a `format` of its
//! own that can, which xschem honours.
//!
//! xschem evaluates Tcl from the files it opens, and an export's values
//! come from the user's `.asc`. So everything is written through [`guard`],
//! which lets a value reach the file only where it stays text: an instance
//! whose values or nets a symbol's Tcl would read gets a plain format of
//! its own, or becomes its SPICE line in a code block, and anything with
//! `tcleval` is left out with a finding. The written file is read back as
//! xschem reads it and the export fails if anything differs.

use super::file::{self, XFile, XInstance, XText, XWire};
use super::import::Proof;
use super::netlist::{self, Finding, XNetlist};
use super::props;
use super::symbol::{Origin, Resolver, XSymbol};
use crate::geometry::{Orient, Point};
use crate::layout::{LayoutError, LayoutOptions, Quality};
use crate::netlist::{CompareOptions, Element, Netlist, compare_with, instance_name};
use crate::schematic::{Item, Schematic, TextKind, normalize_symbol_name};
use crate::symbol::{Graphic, SymbolDef, SymbolLibrary};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub mod guard;

/// xschem units per LTspice unit.
const TO_XSCHEM: f64 = 0.625;
/// LTspice units per xschem unit.
const TO_LTSPICE: f64 = 1.6;
/// Where a poly resistor's body terminal is drawn, in xschem units: to the
/// right of the body, which IHP's symbol has no pin for.
const BODY_PIN: (f64, f64) = (20.0, 0.0);

#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    pub layout: LayoutOptions,
    /// Lines for a `MODEL` code block, as IHP's test benches write the PDK's
    /// model header: `.lib cornerMOSlv.lib mos_tt`.
    pub model_lines: Vec<String>,
    pub netlist: netlist::Options,
    /// Refuse circuits with more elements than this rather than redraw them.
    pub max_elements: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Exported {
    pub file: XFile,
    /// The `.sch` text.
    pub text: SchText,
    pub findings: Vec<Finding>,
    pub notes: Vec<String>,
    /// The `.asc`'s netlist.
    pub asc_netlist: Netlist,
    /// How a netlist of the `.sch` is compared with the `.asc`'s.
    pub compare: CompareOptions,
    /// aispice's xschem netlist of the new `.sch`.
    pub xschem: XNetlist,
    /// The `.asc`'s netlist against aispice's xschem netlist of the `.sch`.
    pub proof: Proof,
    pub quality: Quality,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ExportError {
    #[error("the circuit could not be redrawn for xschem: {0}")]
    Layout(LayoutError),
    #[error(transparent)]
    Netlist(#[from] netlist::NetlistError),
    #[error("the circuit has {0} elements; the layout handles up to {1}")]
    TooLarge(usize, usize),
    /// The written file did not read back as the writer meant it.
    #[error("the xschem file was not written: {0}")]
    Unverified(String),
}

/// How the netlister's line for an instance is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    /// `value=` holds everything after the nodes.
    Value,
    /// Like [`Style::Value`], for symbols whose format is Tcl: values Tcl
    /// would interpret get a plain format instead.
    Source,
    /// `model=`.
    Diode,
    /// xschem's BJTs have no substrate pin; always a plain format.
    Bjt,
    /// `model=`, `w=`, `l=`, `m=` and `extra=`; `bulk` ties the bulk to the
    /// source with a wire (a three-pin LTspice MOSFET).
    Mos { bulk: bool },
    /// `vnam=` empty, `value=` the rest.
    Vnam,
    /// `VAR=` and `FUNC=`.
    Bsource,
    /// An IHP device: `spiceprefix=X`, the name without its X, `model=` and
    /// one property per parameter.
    Pdk,
}

/// How one LTspice symbol is drawn in xschem.
#[derive(Debug, Clone)]
struct Map {
    xschem: String,
    /// The xschem pin for each LTspice pin, in SPICE order. `None`: not a
    /// pin of the xschem symbol (a poly resistor's body).
    pins: Vec<Option<&'static str>>,
    style: Style,
}

/// The symbols the export draws, by the names the layout uses.
const GENERIC: &[&str] = &[
    "res",
    "cap",
    "ind",
    "voltage",
    "Misc\\signal",
    "current",
    "diode",
    "zener",
    "schottky",
    "LED",
    "npn",
    "pnp",
    "nmos",
    "pmos",
    "nmos4",
    "pmos4",
    "e",
    "g",
    "f",
    "h",
    "bv",
    "bi",
];

fn map_symbol(name: &str) -> Option<Map> {
    let key = normalize_symbol_name(name);
    let m = |x: &str, pins: &[&'static str], style| {
        Some(Map {
            xschem: x.to_string(),
            pins: pins.iter().map(|p| Some(*p)).collect(),
            style,
        })
    };
    match key.as_str() {
        "res" => m("devices/res.sym", &["P", "M"], Style::Value),
        "cap" => m("devices/capa.sym", &["p", "m"], Style::Value),
        "ind" => m("devices/ind.sym", &["p", "m"], Style::Value),
        "voltage" | "misc/signal" => m("devices/vsource.sym", &["p", "m"], Style::Source),
        "current" => m("devices/isource.sym", &["p", "m"], Style::Value),
        "diode" | "zener" | "schottky" | "led" => m("devices/diode.sym", &["p", "m"], Style::Diode),
        "npn" => m("devices/npn.sym", &["C", "B", "E"], Style::Bjt),
        "pnp" => m("devices/pnp.sym", &["C", "B", "E"], Style::Bjt),
        "nmos4" => m(
            "devices/nmos4.sym",
            &["d", "g", "s", "b"],
            Style::Mos { bulk: false },
        ),
        "pmos4" => m(
            "devices/pmos4.sym",
            &["d", "g", "s", "b"],
            Style::Mos { bulk: false },
        ),
        "nmos" => m(
            "devices/nmos4.sym",
            &["d", "g", "s"],
            Style::Mos { bulk: true },
        ),
        "pmos" => m(
            "devices/pmos4.sym",
            &["d", "g", "s"],
            Style::Mos { bulk: true },
        ),
        "e" => m("devices/vcvs.sym", &["p", "m", "cp", "cm"], Style::Value),
        "g" => m("devices/vccs.sym", &["p", "m", "cp", "cm"], Style::Value),
        "f" => m("devices/cccs.sym", &["p", "m"], Style::Vnam),
        "h" => m("devices/ccvs.sym", &["p", "m"], Style::Vnam),
        "bv" | "bi" => m("devices/bsource.sym", &["p", "m"], Style::Bsource),
        k if k.starts_with("sg13g2/") => {
            use crate::pdk::Family;
            let dev = crate::pdk::device_for_symbol(k)?;
            let x = format!("sg13g2_pr/{}.sym", dev.name);
            match dev.family {
                Family::Mos => m(&x, &["D", "G", "S", "B"], Style::Pdk),
                Family::Hbt => m(&x, &["C", "B", "E", "S"], Style::Pdk),
                Family::Cap => m(&x, &["c0", "c1"], Style::Pdk),
                Family::Res => Some(Map {
                    xschem: x,
                    pins: vec![Some("P"), Some("M"), None],
                    style: Style::Pdk,
                }),
                Family::Dio => None,
            }
        }
        _ => None,
    }
}

/// The built-in definition of an xschem symbol, for its pin positions and
/// body.
fn xschem_symbol(reference: &str) -> Option<XSymbol> {
    let (_, text) = super::builtin::get(reference)?;
    XSymbol::parse(reference, text, Origin::Builtin).ok()
}

/// An LTspice symbol definition with xschem's pin positions and body.
fn shaped(def: &SymbolDef, map: &Map, x: &XSymbol) -> Option<SymbolDef> {
    let order = def.pins_in_spice_order();
    if order.len() != map.pins.len() {
        return None;
    }
    let scale = |v: f64| (v * TO_LTSPICE).round() as i32;
    let mut out = def.clone();
    for (pin, xname) in order.iter().zip(&map.pins) {
        let (px, py) = match xname {
            Some(n) => {
                let p = x.pins.iter().find(|p| p.name == *n)?;
                (p.x, p.y)
            }
            None => BODY_PIN,
        };
        let target = out
            .pins
            .iter_mut()
            .find(|p| p.name == pin.name && p.spice_order == pin.spice_order)?;
        target.at = Point::new(scale(px), scale(py));
    }
    let body = x
        .file
        .shapes
        .iter()
        .find(|s| s.kind == file::ShapeKind::Box && s.layer == 4 && s.coords.len() == 4)
        .map(|s| (s.coords[0], s.coords[1], s.coords[2], s.coords[3]))
        .unwrap_or((-10.0, -10.0, 10.0, 10.0));
    out.graphics = vec![Graphic::Rect {
        a: Point::new(scale(body.0), scale(body.1)),
        b: Point::new(scale(body.2), scale(body.3)),
    }];
    out.windows.clear();
    Some(out)
}

/// The symbol set the layout draws with: the caller's LTspice symbols with
/// xschem's shapes, and nothing else, so parts xschem has no symbol for are
/// kept as SPICE lines.
fn xschem_shaped_library(lib: &SymbolLibrary) -> SymbolLibrary {
    let mut defs = Vec::new();
    let pdk = crate::pdk::KITS
        .iter()
        .flat_map(|k| k.devices.iter())
        .filter_map(|d| d.symbol)
        .map(|s| s.replace('/', "\\"));
    for name in GENERIC.iter().map(|s| s.to_string()).chain(pdk) {
        let (Some(map), Ok((def, _))) = (map_symbol(&name), lib.resolve(&name)) else {
            continue;
        };
        let Some(x) = xschem_symbol(&map.xschem) else {
            continue;
        };
        if let Some(d) = shaped(&def, &map, &x) {
            defs.push((name, d));
        }
    }
    SymbolLibrary::from_defs(defs)
}

/// xschem's rotation and flip for an LTspice orientation: the pair whose
/// transform matches. LTspice turns then mirrors; xschem mirrors then turns.
fn xschem_orient(o: Orient) -> (u8, bool) {
    let probe = [Point::new(3, 7), Point::new(-5, 2)];
    for rot in 0..4u8 {
        for flip in [false, true] {
            let inst = XInstance {
                symbol: String::new(),
                x: 0.0,
                y: 0.0,
                rot,
                flip,
                props: String::new(),
                embedded: None,
            };
            if probe.iter().all(|p| {
                let (x, y) = place(&inst, f64::from(p.x), f64::from(p.y));
                let q = o.apply(*p);
                (x, y) == (f64::from(q.x), f64::from(q.y))
            }) {
                return (rot, flip);
            }
        }
    }
    (0, false)
}

/// A pin's position on the sheet, as xschem computes it.
fn place(inst: &XInstance, x: f64, y: f64) -> (f64, f64) {
    let xf = if inst.flip { -x } else { x };
    let (rx, ry) = match inst.rot {
        0 => (xf, y),
        1 => (-y, xf),
        2 => (-xf, -y),
        _ => (y, -xf),
    };
    (inst.x + rx, inst.y + ry)
}

fn to_x(p: Point) -> (f64, f64) {
    (f64::from(p.x) * TO_XSCHEM, f64::from(p.y) * TO_XSCHEM)
}

/// One written instance and what it stands for.
struct Placed {
    /// Index in the file's instances.
    index: usize,
    map: Map,
    element: Element,
    overridden: bool,
    /// The instance as placed, for its pin positions.
    inst: XInstance,
}

/// The `.sch` text of an export: written by the export's own writer, every
/// instance checked by [`guard`], and read back unchanged. It is the only
/// text aispice lets xschem open without the user asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchText(String);

impl SchText {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for SchText {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SchText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<[u8]> for SchText {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// The file being written, with what each instance's properties must read
/// back as. Everything goes in through [`guard`]'s checks.
struct Writer<'r> {
    f: XFile,
    pairs: Vec<Vec<(String, String)>>,
    labels: usize,
    code: usize,
    resolver: &'r Resolver,
    dir: PathBuf,
    /// Symbols as xschem will load them, by reference.
    loaded: HashMap<String, Option<XSymbol>>,
    findings: Vec<Finding>,
}

impl Writer<'_> {
    /// The definition xschem will load for a reference: the installed file,
    /// else aispice's built-in one.
    fn loaded(&mut self, reference: &str) -> Option<XSymbol> {
        let (resolver, dir) = (self.resolver, &self.dir);
        self.loaded
            .entry(reference.to_string())
            .or_insert_with(|| resolver.resolve(reference, dir).ok())
            .clone()
    }

    /// Check an instance's properties against the symbol xschem will load.
    fn check(
        &mut self,
        reference: &str,
        pairs: &[(String, String)],
        nets: &[String],
    ) -> Result<(), String> {
        let sym = self
            .loaded(reference)
            .ok_or_else(|| format!("symbol `{reference}` was not found"))?;
        guard::check_instance(reference, &sym, pairs, nets)
    }

    /// Add an instance, or say why it may not be written.
    fn instance(
        &mut self,
        mut inst: XInstance,
        pairs: Vec<(String, String)>,
        nets: &[String],
    ) -> Result<usize, String> {
        self.check(&inst.symbol, &pairs, nets)?;
        inst.props = props::write(&pairs);
        self.f.instances.push(inst);
        self.pairs.push(pairs);
        Ok(self.f.instances.len() - 1)
    }

    /// Give an instance other properties.
    fn replace(
        &mut self,
        index: usize,
        pairs: Vec<(String, String)>,
        nets: &[String],
    ) -> Result<(), String> {
        let reference = self.f.instances[index].symbol.clone();
        self.check(&reference, &pairs, nets)?;
        self.f.instances[index].props = props::write(&pairs);
        self.pairs[index] = pairs;
        Ok(())
    }

    fn at(symbol: &str, (x, y): (f64, f64)) -> XInstance {
        XInstance {
            symbol: symbol.into(),
            x,
            y,
            rot: 0,
            flip: false,
            props: String::new(),
            embedded: None,
        }
    }

    /// A ground symbol or a net label at a point.
    fn label(&mut self, at: (f64, f64), net: &str) {
        let checked = guard::check_label(net).and_then(|()| {
            self.labels += 1;
            let n = self.labels;
            let (symbol, pairs) = if net == "0" || net.eq_ignore_ascii_case("gnd") {
                (
                    "devices/gnd.sym",
                    vec![
                        ("name".into(), format!("l{n}")),
                        ("lab".into(), "GND".into()),
                    ],
                )
            } else {
                (
                    "devices/lab_pin.sym",
                    vec![
                        ("name".into(), format!("p{n}")),
                        ("sig_type".into(), "std_logic".into()),
                        ("lab".into(), net.to_string()),
                    ],
                )
            };
            self.instance(Self::at(symbol, at), pairs, &[net.to_string()])
        });
        if let Err(why) = checked {
            self.findings.push(Finding {
                subject: net.to_string(),
                message: format!("{why}; the net is not labelled"),
            });
        }
    }

    /// The properties of a code block holding `text`.
    fn code_pairs(
        &mut self,
        name: Option<&str>,
        toplevel: bool,
        text: &str,
    ) -> Vec<(String, String)> {
        let name = match name {
            Some(n) => n.to_string(),
            None => {
                self.code += 1;
                format!("s{}", self.code)
            }
        };
        vec![
            ("name".into(), name),
            ("only_toplevel".into(), toplevel.to_string()),
            ("value".into(), text.to_string()),
        ]
    }

    /// A code block. `name` defaults to `s<n>`.
    fn code(
        &mut self,
        at: (f64, f64),
        name: Option<&str>,
        toplevel: bool,
        text: &str,
    ) -> Result<usize, String> {
        guard::check_code(text)?;
        let pairs = self.code_pairs(name, toplevel, text);
        self.instance(Self::at("devices/code_shown.sym", at), pairs, &[])
    }

    /// Write an instance that may not be written as its symbol as its SPICE
    /// line in a code block instead, labelling the nets at its pins so the
    /// wires that end there stay connected. `index` is the instance to
    /// replace, if it was written already.
    fn as_code(&mut self, index: Option<usize>, p: &Placed, why: &str) {
        let e = &p.element;
        let line = format!("{} {} {}", e.name, e.nodes.join(" "), e.rest.trim());
        if let Err(more) = guard::check_code(&line) {
            self.findings.push(Finding {
                subject: e.name.clone(),
                message: format!("{why}, and {more}; the element is left out"),
            });
            if let Some(i) = index {
                // Leave nothing of it that xschem would read.
                let pairs = self.code_pairs(None, false, "");
                self.f.instances[i] = Self::at("devices/code_shown.sym", (p.inst.x, p.inst.y));
                self.f.instances[i].props = props::write(&pairs);
                self.pairs[i] = pairs;
            }
            return;
        }
        self.findings.push(Finding {
            subject: e.name.clone(),
            message: format!("{why}; its SPICE line goes into a code block"),
        });
        let pairs = self.code_pairs(None, false, &line);
        match index {
            Some(i) => {
                self.f.instances[i] = Self::at("devices/code_shown.sym", (p.inst.x, p.inst.y));
                self.f.instances[i].props = props::write(&pairs);
                self.pairs[i] = pairs;
            }
            None => {
                if let Err(w) = self.instance(
                    Self::at("devices/code_shown.sym", (p.inst.x, p.inst.y)),
                    pairs,
                    &[],
                ) {
                    self.findings.push(Finding {
                        subject: e.name.clone(),
                        message: format!("{w}; the element is left out"),
                    });
                    return;
                }
            }
        }
        if let Some(xs) = xschem_symbol(&p.map.xschem) {
            for (k, pin) in p.map.pins.iter().enumerate() {
                let (Some(pin), Some(net)) = (pin, e.nodes.get(k)) else {
                    continue;
                };
                if let Some(xp) = xs.pins.iter().find(|x| x.name == *pin) {
                    let at = place(&p.inst, xp.x, xp.y);
                    self.label(at, net);
                }
            }
        }
    }

    /// A comment.
    fn text(&mut self, at: (f64, f64), text: String) {
        if let Err(why) = guard::check_text(&text) {
            self.findings.push(Finding {
                subject: "comment".into(),
                message: format!("{why}; it is left out"),
            });
            return;
        }
        self.f.texts.push(XText {
            text,
            x: at.0,
            y: at.1,
            rot: 0,
            flip: false,
            xscale: 0.4,
            yscale: 0.4,
            props: String::new(),
        });
    }
}

/// Export an `.asc`. `out` is where the `.sch` will be written: the netlist
/// check resolves symbols from there, as xschem will.
pub fn export(
    asc: &Schematic,
    lib: &SymbolLibrary,
    title: &str,
    out: &Path,
    resolver: &Resolver,
    opts: &ExportOptions,
) -> Result<Exported, ExportError> {
    let (built, _) = crate::netlist::build(asc, lib, title);
    let asc_netlist = built.netlist;
    let elements = asc_netlist.elements().count();
    if let Some(max) = opts.max_elements.filter(|m| elements > *m) {
        return Err(ExportError::TooLarge(elements, max));
    }
    let mut notes: Vec<String> = built.warnings.clone();

    // The generic model cards the netlister adds for parts at a default
    // model (`.model NPN NPN`) would be added again when the drawing is
    // netlisted; they are drawn once, as their own code block.
    let mut layout_input = asc_netlist.clone();
    let mut default_cards = Vec::new();
    layout_input.items.retain(|item| match item {
        crate::netlist::Line::Directive { text } if is_default_card(text) => {
            default_cards.push(text.clone());
            false
        }
        _ => true,
    });
    let xlib = xschem_shaped_library(lib);
    let laid = crate::layout::from_netlist(&layout_input, &xlib, &opts.layout)
        .map_err(ExportError::Layout)?;
    let mut w = Writer {
        f: XFile {
            version: "xschem version=3.4.4 file_version=1.2".into(),
            ..XFile::default()
        },
        pairs: Vec::new(),
        labels: 0,
        code: 0,
        resolver,
        dir: out.parent().map(Path::to_path_buf).unwrap_or_default(),
        loaded: HashMap::new(),
        findings: Vec::new(),
    };
    for warning in &laid.warnings {
        if warning.contains("SPICE line") || warning.contains("cannot reproduce") {
            let (subject, message) = warning.split_once(": ").unwrap_or(("", warning.as_str()));
            w.findings.push(Finding {
                subject: subject.to_string(),
                message: format!("{message}; it goes into a code block"),
            });
        } else {
            notes.push(warning.clone());
        }
    }
    let drawing = laid.schematic;
    let (drawn, _) = crate::netlist::build(&drawing, &xlib, title);
    let drawn = drawn.netlist;

    let mut placed: Vec<Placed> = Vec::new();
    let mut right = 0.0f64;
    let mut top = f64::MAX;
    for item in &drawing.items {
        match item {
            Item::Wire(wire) => {
                let (a, b) = (to_x(wire.a), to_x(wire.b));
                right = right.max(a.0).max(b.0);
                top = top.min(a.1).min(b.1);
                w.f.wires.push(XWire {
                    x1: a.0,
                    y1: a.1,
                    x2: b.0,
                    y2: b.1,
                    props: String::new(),
                });
            }
            Item::Flag(fl) => w.label(to_x(fl.at), &fl.label),
            Item::Symbol(s) => {
                let Some(map) = map_symbol(&s.name) else {
                    w.findings.push(Finding {
                        subject: s.inst_name().unwrap_or_default().to_string(),
                        message: format!("symbol `{}` has no xschem counterpart", s.name),
                    });
                    continue;
                };
                let Ok((def, _)) = xlib.resolve(&s.name) else {
                    continue;
                };
                let prefix = crate::netlist::build::part_prefix(s, &def);
                let name = instance_name(prefix, s.inst_name().unwrap_or_default());
                let Some(element) = drawn.element(&name).cloned() else {
                    continue;
                };
                let (x, y) = to_x(s.at);
                right = right.max(x);
                top = top.min(y);
                let (rot, flip) = xschem_orient(s.orient);
                let inst = XInstance {
                    symbol: map.xschem.clone(),
                    x,
                    y,
                    rot,
                    flip,
                    props: String::new(),
                    embedded: None,
                };
                let xs = xschem_symbol(&map.xschem);
                // A three-pin MOSFET's bulk is its source: tie them.
                if let (Style::Mos { bulk: true }, Some(xs)) = (map.style, &xs)
                    && let (Some(b), Some(src)) = (
                        xs.pins.iter().find(|p| p.name == "b"),
                        xs.pins.iter().find(|p| p.name == "s"),
                    )
                {
                    let (bx, by) = place(&inst, b.x, b.y);
                    let (sx, sy) = place(&inst, src.x, src.y);
                    w.f.wires.push(XWire {
                        x1: bx,
                        y1: by,
                        x2: sx,
                        y2: sy,
                        props: String::new(),
                    });
                }
                // A poly resistor's body: a label on the sheet where the
                // layout connected it.
                for (k, xp) in map.pins.iter().enumerate() {
                    if xp.is_none()
                        && let Some(node) = element.nodes.get(k)
                    {
                        let at = place(&inst, BODY_PIN.0, BODY_PIN.1);
                        w.label(at, node);
                    }
                }
                // The symbol's own format when it reproduces the line and
                // the values stay text in it, else a format of the
                // instance's own.
                let nets = element.nodes.clone();
                let first = first_props(&map, &element, xs.as_ref())
                    .filter(|p| w.check(&map.xschem, p, &nets).is_ok());
                let (pairs, overridden) = match first {
                    Some(p) => (p, false),
                    None => (override_props(&map, &element), true),
                };
                let mut p = Placed {
                    index: 0,
                    map,
                    element,
                    overridden,
                    inst: inst.clone(),
                };
                match w.instance(inst, pairs, &nets) {
                    Ok(index) => {
                        p.index = index;
                        placed.push(p);
                    }
                    Err(why) => w.as_code(None, &p, &why),
                }
            }
            Item::Text(t) => {
                let at = to_x(t.at);
                let text = t.lines().join("\n");
                match t.kind {
                    TextKind::Directive => {
                        if let Err(why) = w.code(at, None, false, &text) {
                            w.findings.push(Finding {
                                subject: text.lines().next().unwrap_or("").to_string(),
                                message: format!("{why}; the directive is left out"),
                            });
                        }
                    }
                    TextKind::Comment => w.text(at, text),
                }
            }
            _ => {}
        }
    }
    let side = (right + 100.0).round();
    if !default_cards.is_empty() {
        let y = if top == f64::MAX {
            -200.0
        } else {
            top.round() - 200.0
        };
        if let Err(why) = w.code((side, y), None, false, &default_cards.join("\n")) {
            w.findings.push(Finding {
                subject: ".model".into(),
                message: format!("{why}; the model cards are left out"),
            });
        }
    }
    if !opts.model_lines.is_empty() {
        // Plain text: the lines name the kit's libraries without a path,
        // so nothing in them needs Tcl.
        let mut value = opts.model_lines.join("\n");
        value.push('\n');
        let y = if top == f64::MAX { 0.0 } else { top.round() };
        match w.code((side, y), Some("MODEL"), true, &value) {
            Ok(_) => notes.push(format!(
                "the PDK model header is a MODEL code block: {}",
                opts.model_lines.join("; ")
            )),
            Err(why) => w.findings.push(Finding {
                subject: "MODEL".into(),
                message: format!("{why}; the model header is left out"),
            }),
        }
    }

    // Check every instance with aispice's xschem netlister; give the ones
    // their symbol cannot reproduce a format of their own, or their SPICE
    // line in a code block when even that would not stay text.
    let check = CompareOptions {
        auto_names: laid.renames.iter().map(|(_, to)| to.clone()).collect(),
        ..CompareOptions::xschem()
    };
    let mut xn = netlist::netlist(out, &w.f, resolver, &opts.netlist)?;
    for _ in 0..2 {
        let x = crate::netlist::parse(&xn.text);
        let Err(problems) = compare_with(&drawn, &x, &check) else {
            break;
        };
        let mut changed = false;
        for p in placed.iter_mut() {
            if p.overridden {
                continue;
            }
            let named = problems.iter().any(|m| {
                m.0.split([':', ' '])
                    .next()
                    .is_some_and(|n| n.eq_ignore_ascii_case(&p.element.name))
            });
            if named {
                let pairs = override_props(&p.map, &p.element);
                if let Err(why) = w.replace(p.index, pairs, &p.element.nodes) {
                    w.as_code(Some(p.index), p, &why);
                }
                p.overridden = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
        xn = netlist::netlist(out, &w.f, resolver, &opts.netlist)?;
    }
    let overrides = placed
        .iter()
        .filter(|p| w.pairs[p.index].iter().any(|(k, _)| k == "format"))
        .count();
    if overrides > 0 {
        notes.push(format!(
            "{overrides} instance(s) carry a format of their own, because their xschem symbol's format would write a different line or paste a value into Tcl"
        ));
    }
    let mut findings = std::mem::take(&mut w.findings);
    findings.extend(xn.findings.iter().cloned());
    let compare = check;
    let x = crate::netlist::parse(&xn.text);
    let proof = Proof::new(
        "the .asc's netlist against aispice's xschem netlist of the .sch",
        compare_with(&asc_netlist, &x, &compare),
    );
    let text = file::write(&w.f);
    guard::verify(&text, &w.f, &w.pairs).map_err(ExportError::Unverified)?;
    Ok(Exported {
        file: w.f,
        text: SchText(text),
        findings,
        notes,
        asc_netlist,
        compare,
        xschem: xn,
        proof,
        quality: laid.quality,
    })
}

/// A generic model card LTspice's netlister adds by itself.
fn is_default_card(text: &str) -> bool {
    let words: Vec<String> = text
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    matches!(words.as_slice(), [kw, name, kind]
        if kw == ".model"
            && name == kind
            && matches!(name.as_str(), "d" | "npn" | "pnp" | "nmos" | "pmos" | "njf" | "pjf"))
}

/// Words of the rest of a line, keeping parenthesised groups whole.
fn words(rest: &str) -> Vec<String> {
    crate::layout::tokens(rest)
}

/// The properties an instance starts with, or `None` when only a format of
/// its own can reproduce the line. `xs` is the xschem symbol, whose
/// template lists the parameters an IHP device may carry by name.
fn first_props(map: &Map, e: &Element, xs: Option<&XSymbol>) -> Option<Vec<(String, String)>> {
    let rest = e.rest.trim();
    let mut p: Vec<(String, String)> = vec![("name".into(), e.name.clone())];
    let w = words(rest);
    match map.style {
        Style::Value | Style::Source => p.push(("value".into(), rest.into())),
        Style::Diode => p.push(("model".into(), rest.into())),
        Style::Bjt => return None,
        Style::Mos { .. } => {
            let (model, params) = w.split_first()?;
            if model.contains('=') {
                return None;
            }
            p.push(("model".into(), model.clone()));
            let mut extra = Vec::new();
            for kv in params {
                let (k, v) = kv.split_once('=')?;
                match k.to_ascii_lowercase().as_str() {
                    "w" | "l" | "m" => p.push((k.to_ascii_lowercase(), v.to_string())),
                    _ => extra.push(kv.clone()),
                }
            }
            // Without w or l the template would add its own: no defaults
            // for those. m=1 is SPICE's default anyway.
            for (k, default) in [("w", None), ("l", None), ("m", Some("1"))] {
                if !p.iter().any(|(pk, _)| pk == k) {
                    let d: &str = default?;
                    p.push((k.into(), d.into()));
                }
            }
            p.push(("extra".into(), extra.join(" ")));
        }
        Style::Vnam => {
            p.push(("vnam".into(), String::new()));
            p.push(("value".into(), rest.into()));
        }
        Style::Bsource => {
            let (var, func) = rest.split_once('=')?;
            let var = var.trim();
            if !(var.eq_ignore_ascii_case("v") || var.eq_ignore_ascii_case("i")) {
                return None;
            }
            p.push(("VAR".into(), var.into()));
            p.push(("FUNC".into(), func.trim().into()));
        }
        Style::Pdk => {
            let name = e.name.strip_prefix(['X', 'x']).filter(|n| !n.is_empty())?;
            p[0].1 = name.to_string();
            p.push(("spiceprefix".into(), "X".into()));
            let (model, params) = w.split_first()?;
            p.push(("model".into(), model.clone()));
            // Only the parameters the symbol declares go in by name; the
            // rest need a format of the instance's own.
            let template: Vec<String> = props::pairs(&xs?.template())
                .into_iter()
                .map(|(k, _)| k)
                .filter(|k| !matches!(k.as_str(), "name" | "spiceprefix" | "model" | "body"))
                .collect();
            for kv in params {
                let (k, v) = kv.split_once('=')?;
                if !template.iter().any(|t| t == k) {
                    return None;
                }
                p.push((k.to_string(), v.to_string()));
            }
            if let Some(body) = body_node(map, e) {
                p.push(("body".into(), body));
            }
        }
    }
    Some(p)
}

/// The net a poly resistor's body is on, as xschem will name it.
fn body_node(map: &Map, e: &Element) -> Option<String> {
    let k = map.pins.iter().position(Option::is_none)?;
    let n = e.nodes.get(k)?;
    Some(if n == "0" { "GND".into() } else { n.clone() })
}

/// Properties with a format of their own: the name, the pins, any nodes the
/// xschem symbol has no pin for, then the rest of the line as `value`.
fn override_props(map: &Map, e: &Element) -> Vec<(String, String)> {
    let pdk = map.style == Style::Pdk && e.name.len() > 1 && e.name.starts_with(['X', 'x']);
    let name = if pdk {
        e.name[1..].to_string()
    } else {
        e.name.clone()
    };
    let mut format = String::from(if pdk {
        "@spiceprefix@name @pinlist"
    } else {
        "@name @pinlist"
    });
    for (k, node) in e.nodes.iter().enumerate() {
        let missing = match map.pins.get(k) {
            Some(Some(_)) => false,
            Some(None) | None => true,
        };
        // A three-pin MOSFET's fourth node is the bulk pin, wired to source.
        if missing && !(matches!(map.style, Style::Mos { bulk: true }) && k == 3) {
            format.push(' ');
            format.push_str(if node == "0" { "0" } else { node });
        }
    }
    format.push_str(" @value");
    let mut p = vec![("name".to_string(), name)];
    if pdk {
        p.push(("spiceprefix".into(), "X".into()));
    }
    p.push(("format".into(), format));
    p.push(("value".into(), e.rest.trim().to_string()));
    p
}

#[cfg(test)]
mod tests;

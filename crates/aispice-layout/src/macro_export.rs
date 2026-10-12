//! A block as a macro package (`macro/v1`): the views aispice writes itself
//! (liberty, Verilog, SPICE, the spec table), what each pin does, read from
//! the circuit, and the LEF abstract from Magic.
//!
//! `aispice_tools` runs the checks and simulations and assembles the
//! package; everything here is a function of its inputs, except [`lef`],
//! which runs Magic in a private folder.

use crate::lvs::LvsNetlist;
use crate::tools::{self, LayoutError, PdkLayoutFiles, Programs, RunDir};
use aispice_core::macropkg::lef::{LefMacro, parse_lef};
use aispice_core::macropkg::{Direction, Manifest, Pin, PinKind};
use aispice_core::netlist::{Element, Line, Netlist};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use tokio_util::sync::CancellationToken;

const LEF_SCRIPT: &str = include_str!("../magic/lef.tcl");
/// A LEF abstract larger than this is not read back.
const MAX_LEF_BYTES: u64 = 64 << 20;

/// Whether a name is a `macro/v1` identifier, valid as a GDS cell, LEF
/// macro, liberty cell, Verilog module and SPICE subcircuit alike: letters,
/// digits and underscores, starting with a letter or underscore, at most 64.
pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The PDK's cell libraries under `libs.ref` whose cells a chip, or its
/// netlist, may hold beside a block.
const PDK_CELL_LIBS: [&str; 3] = ["sg13g2_stdcell", "sg13g2_io", "sg13g2_sram"];
/// At most this many LEF files are read per library.
const MAX_PDK_LEFS: usize = 256;

/// The cells of the PDK's standard-cell, IO and SRAM libraries, lowercased,
/// as their LEF files name them (`MACRO name`). A library the PDK checkout
/// lacks is skipped: none of its cells can then meet a block.
pub fn pdk_cell_names(root: &Path) -> Result<BTreeSet<String>, String> {
    let mut out = BTreeSet::new();
    for lib in PDK_CELL_LIBS {
        let dir = root.join("libs.ref").join(lib).join("lef");
        let Ok(list) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut lefs: Vec<_> = list
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lef")))
            .collect();
        if lefs.len() > MAX_PDK_LEFS {
            return Err(format!(
                "{} holds more than {MAX_PDK_LEFS} LEF files",
                dir.display()
            ));
        }
        lefs.sort();
        for lef in lefs {
            let text = read_lef(&lef)?;
            for line in text.lines() {
                let mut words = line.split_whitespace();
                if words.next() == Some("MACRO")
                    && let Some(name) = words.next()
                {
                    out.insert(name.to_ascii_lowercase());
                }
            }
        }
    }
    Ok(out)
}

/// A LEF file of the PDK as text: a regular file (links followed, as in a
/// PDK checkout), at most [`MAX_LEF_BYTES`].
fn read_lef(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let bad = |why: &str| format!("{}: {why}", path.display());
    let meta = std::fs::metadata(path).map_err(|e| bad(&e.to_string()))?;
    if !meta.is_file() {
        return Err(bad("not a regular file"));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_LEF_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| bad(&e.to_string()))?;
    if bytes.len() as u64 > MAX_LEF_BYTES {
        return Err(bad("larger than a LEF file aispice reads"));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Why a block may not take this name, if it may not: names compare without
/// case, as SPICE compares them, so `SG13G2_INV_1` is IHP's `sg13g2_inv_1`.
/// The PDK's `sg13g2_` prefix is refused whether or not the checkout holds
/// the library, and so is any name of a cell in `pdk_cells`
/// ([`pdk_cell_names`]).
pub fn pdk_name_clash(name: &str, pdk_cells: &BTreeSet<String>) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("sg13g2_") {
        return Some(format!(
            "`{name}` starts with the PDK's prefix sg13g2_, which names IHP's own cells"
        ));
    }
    pdk_cells
        .contains(&lower)
        .then(|| format!("`{name}` is the name of a cell of the PDK's libraries (names compare without case, as in SPICE)"))
}

/// Net names that mean a supply: `vdd`, `avdd`, `vcc`, `vpwr` and their
/// suffixed forms (`vdd_core`).
pub fn power_like(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("vdd")
        || n.starts_with("vcc")
        || ["avdd", "dvdd", "vpwr", "vpp"]
            .iter()
            .any(|p| n == *p || n.starts_with(&format!("{p}_")))
}

/// Net names that mean ground: `vss`, `gnd`, `agnd`, `vgnd` and their
/// suffixed forms.
pub fn ground_like(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "0"
        || n.starts_with("vss")
        || n.starts_with("gnd")
        || ["agnd", "dgnd", "avss", "dvss", "vgnd"]
            .iter()
            .any(|p| n == *p || n.starts_with(&format!("{p}_")))
}

/// The circuit with ground (net `0`) renamed at the top level, so that it
/// can be a pin of the macro. Refused when the circuit already has a net of
/// that name, which would merge the two.
pub fn rename_ground(netlist: &Netlist, to: &str) -> Result<Netlist, String> {
    if netlist.nodes().iter().any(|n| n.eq_ignore_ascii_case(to)) {
        return Err(format!(
            "the circuit already has a net called {to}; name the ground pin something else"
        ));
    }
    let mut out = netlist.clone();
    for item in &mut out.items {
        if let Line::Element(e) = item {
            for n in &mut e.nodes {
                if n == "0" {
                    *n = to.to_string();
                }
            }
        }
    }
    Ok(out)
}

fn named(e: &Element, devices: &[String]) -> bool {
    let bare = e.name.strip_prefix(['X', 'x']).unwrap_or(&e.name);
    devices.iter().any(|d| {
        d.eq_ignore_ascii_case(&e.name)
            || d.strip_prefix(['X', 'x'])
                .unwrap_or(d)
                .eq_ignore_ascii_case(bare)
    })
}

/// What each port of a block does, from the block's devices (`devices`,
/// netlist names). Supplies are named like supplies (or are `ground`); a
/// port that reaches a transistor's drain, other than through a
/// diode-connected transistor, is an output; one that reaches only gates
/// (and diode-connected transistors) is an input; anything else is inout.
/// Signal ports are analog, in domain `core`. Supplies come last, power
/// before ground, each group in name order.
pub fn infer_pins(
    netlist: &Netlist,
    devices: &[String],
    ports: &[String],
    ground: &str,
) -> Vec<Pin> {
    #[derive(Default)]
    struct Seen {
        gate: bool,
        drain: bool,
        diode: bool,
        other: bool,
    }
    let mut seen: BTreeMap<&str, Seen> = BTreeMap::new();
    for e in netlist.elements().filter(|e| named(e, devices)) {
        let mos = e
            .first_word()
            .and_then(aispice_core::pdk::device)
            .is_some_and(|d| d.is_mosfet())
            && e.nodes.len() == 4;
        for p in ports {
            let s = seen.entry(p.as_str()).or_default();
            if !mos {
                s.other |= e.nodes.iter().any(|n| n == p);
                continue;
            }
            let (d, g, src, b) = (&e.nodes[0], &e.nodes[1], &e.nodes[2], &e.nodes[3]);
            let diode = d == g;
            if g == p {
                s.gate = true;
            }
            if d == p {
                if diode {
                    s.diode = true;
                } else {
                    s.drain = true;
                }
            }
            if src == p || b == p {
                s.other = true;
            }
        }
    }
    let mut signals = Vec::new();
    let mut power = Vec::new();
    let mut grounds = Vec::new();
    for p in ports {
        if p == ground || ground_like(p) {
            grounds.push(supply(p, PinKind::Ground));
            continue;
        }
        if power_like(p) {
            power.push(supply(p, PinKind::Power));
            continue;
        }
        let s = seen.get(p.as_str());
        let direction = match s {
            Some(s) if s.drain => Direction::Output,
            Some(s) if (s.gate || s.diode) && !s.other => Direction::Input,
            _ => Direction::Inout,
        };
        signals.push(Pin {
            name: p.clone(),
            direction,
            kind: PinKind::Analog,
            domain: Some("core".into()),
            width: None,
            layer: None,
            description: None,
        });
    }
    for group in [&mut signals, &mut power, &mut grounds] {
        group.sort_by(|a, b| a.name.cmp(&b.name));
    }
    signals.into_iter().chain(power).chain(grounds).collect()
}

fn supply(name: &str, kind: PinKind) -> Pin {
    Pin {
        name: name.to_string(),
        direction: Direction::Inout,
        kind,
        domain: None,
        width: None,
        layer: None,
        description: None,
    }
}

/// The value of a DC voltage source between `net` and `ground` (or `0`),
/// if the circuit has one.
pub fn supply_voltage(netlist: &Netlist, net: &str, ground: &str) -> Option<f64> {
    netlist.elements().find_map(|e| {
        if e.letter() != 'V' || e.nodes.len() != 2 {
            return None;
        }
        let (a, b) = (&e.nodes[0], &e.nodes[1]);
        let low = |n: &str| n == "0" || n.eq_ignore_ascii_case(ground);
        if !(a.eq_ignore_ascii_case(net) && low(b)) {
            return None;
        }
        let mut words = e.rest.split_whitespace();
        let mut w = words.next()?;
        if w.eq_ignore_ascii_case("dc") {
            w = words.next()?;
        }
        aispice_core::units::parse(w).filter(|v| v.is_finite() && *v > 0.0)
    })
}

/// A corner name that is an identifier: `tt_27`, `ss_125`, `ff_m40`,
/// `ss_125_vdd_1v08` (a supply in its name with `v` for the point).
pub fn corner_ident(process: &str, temperature: f64, supplies: &[(String, f64)]) -> String {
    fn clean(s: &str) -> String {
        let mut out: String = s
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        while out.contains("__") {
            out = out.replace("__", "_");
        }
        out.trim_matches('_').to_ascii_lowercase()
    }
    fn number(v: f64) -> String {
        format!("{}", (v * 1000.0).round() / 1000.0)
            .replace('-', "m")
            .replace('.', "v")
    }
    let mut name = clean(process);
    if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        name = format!("p_{name}");
    }
    name.push('_');
    name.push_str(&number(temperature).replace('v', "p"));
    for (target, v) in supplies {
        let _ = write!(name, "_{}_{}", clean(target), number(*v));
    }
    let name = name.trim_matches('_').to_string();
    if name.len() > 64 {
        name[..64].trim_end_matches('_').to_string()
    } else {
        name
    }
}

/// The capacitance on each pin's net in an extracted subcircuit, in pF:
/// every capacitor with a terminal on the net, to ground or coupling.
pub fn pin_caps(extracted: &str, pins: &[String]) -> BTreeMap<String, f64> {
    let parsed = aispice_core::netlist::parse(&format!("* extracted\n{extracted}"));
    let mut caps: BTreeMap<String, f64> = pins.iter().map(|p| (p.clone(), 0.0)).collect();
    let elements = parsed
        .subckts()
        .flat_map(|s| s.body.iter())
        .chain(parsed.items.iter())
        .filter_map(|l| match l {
            Line::Element(e) if e.letter() == 'C' => Some(e),
            _ => None,
        });
    for e in elements {
        let farads = e
            .first_word()
            .and_then(aispice_core::units::parse)
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(0.0);
        let on: BTreeSet<&String> = e.nodes.iter().collect();
        for n in on {
            if let Some(c) = caps
                .iter_mut()
                .find(|(p, _)| p.eq_ignore_ascii_case(n))
                .map(|(_, c)| c)
            {
                *c += farads * 1e12;
            }
        }
    }
    caps
}

fn domain_of<'a>(m: &'a Manifest, pin: &Pin) -> Option<&'a aispice_core::macropkg::Domain> {
    match &pin.domain {
        Some(d) => m.domains.iter().find(|x| &x.name == d),
        None => m.domains.first(),
    }
}

/// The liberty view: supplies as `pg_pin`s with their voltages, every other
/// pin with its direction and capacitance (pF, from `caps`), analog pins
/// marked `is_analog`, and no timing arcs: an analog block has none to
/// claim. Characterized at the first corner. Leakage is left out on
/// purpose: an analog block's quiescent current is not leakage.
pub fn liberty(m: &Manifest, caps: &BTreeMap<String, f64>, version: &str) -> String {
    let corner = m.corners.first();
    let corner_name = corner.map_or("nominal", |c| c.name.as_str());
    let temperature = corner.map_or(27.0, |c| c.temperature);
    let nominal = m.domains.first().map_or(1.2, |d| d.voltage);
    let mut t = String::new();
    let _ = writeln!(
        t,
        "/* {}: liberty view written by aispice {version}, at corner {corner_name}.\n   Pin capacitance is the extracted capacitance of each pin's net. An analog\n   block has no timing arcs. */",
        m.name
    );
    let _ = writeln!(t, "library ({}_{corner_name}) {{", m.name);
    for line in [
        "technology (cmos) ;",
        "delay_model : table_lookup ;",
        "time_unit : \"1ns\" ;",
        "voltage_unit : \"1V\" ;",
        "current_unit : \"1uA\" ;",
        "capacitive_load_unit (1, pf) ;",
        "pulling_resistance_unit : \"1kohm\" ;",
        "leakage_power_unit : \"1nW\" ;",
        "nom_process : 1 ;",
    ] {
        let _ = writeln!(t, "  {line}");
    }
    let _ = writeln!(t, "  nom_temperature : {} ;", num(temperature));
    let _ = writeln!(t, "  nom_voltage : {} ;", num(nominal));
    for p in m.pins.iter().filter(|p| p.is_supply()) {
        let v = match p.kind {
            PinKind::Power => m
                .domains
                .iter()
                .find(|d| d.power.contains(&p.name))
                .map_or(nominal, |d| d.voltage),
            _ => 0.0,
        };
        let _ = writeln!(t, "  voltage_map ({}, {}) ;", p.name, num(v));
    }
    let _ = writeln!(t, "  cell ({}) {{", m.name);
    let _ = writeln!(
        t,
        "    area : {} ;",
        num((m.size.width_um * m.size.height_um * 1e4).round() / 1e4)
    );
    let _ = writeln!(t, "    is_macro_cell : true ;");
    let _ = writeln!(t, "    dont_touch : true ;");
    let _ = writeln!(t, "    dont_use : true ;");
    for p in m.pins.iter().filter(|p| p.is_supply()) {
        let kind = if p.kind == PinKind::Power {
            "primary_power"
        } else {
            "primary_ground"
        };
        let _ = writeln!(
            t,
            "    pg_pin ({}) {{\n      pg_type : {kind} ;\n      voltage_name : {} ;\n    }}",
            p.name, p.name
        );
    }
    for p in m.pins.iter().filter(|p| !p.is_supply()) {
        let _ = writeln!(t, "    pin ({}) {{", p.name);
        let _ = writeln!(t, "      direction : {} ;", p.direction.as_str());
        if p.kind == PinKind::Analog {
            let _ = writeln!(t, "      is_analog : true ;");
        }
        if p.kind == PinKind::Clock {
            let _ = writeln!(t, "      clock : true ;");
        }
        let c = caps.get(&p.name).copied().unwrap_or(0.0);
        let _ = writeln!(t, "      capacitance : {c:.6} ;");
        if let Some(d) = domain_of(m, p) {
            if let Some(pw) = d.power.first() {
                let _ = writeln!(t, "      related_power_pin : {pw} ;");
            }
            if let Some(gn) = d.ground.first() {
                let _ = writeln!(t, "      related_ground_pin : {gn} ;");
            }
        }
        let _ = writeln!(t, "    }}");
    }
    t.push_str("  }\n}\n");
    t
}

/// A number as liberty and Verilog write it: no exponent for ordinary
/// values, no trailing zeros.
fn num(v: f64) -> String {
    let t = format!("{v}");
    if t.contains('e') || t.contains("inf") || t.contains("NaN") {
        format!("{v:.6}")
    } else {
        t
    }
}

/// The Verilog view: a blackbox module (supplies behind `USE_POWER_PINS`).
/// With `wreal`, analog ports are also declared `wreal`, behind
/// `AISPICE_WREAL`, for simulators that read Verilog-AMS; others never see
/// it.
pub fn verilog(m: &Manifest, wreal: bool, version: &str) -> String {
    let mut t = format!(
        "// {}: Verilog model written by aispice {version}. The block is analog, so\n// a digital simulator sees only its ports.\n",
        m.name
    );
    let signals: Vec<&Pin> = m.pins.iter().filter(|p| !p.is_supply()).collect();
    let supplies: Vec<&Pin> = m.pins.iter().filter(|p| p.is_supply()).collect();
    let decl = |p: &Pin| match p.width {
        Some(w) => format!(
            "{} [{}:0] {}",
            p.direction.as_str(),
            w.saturating_sub(1),
            p.name
        ),
        None => format!("{} {}", p.direction.as_str(), p.name),
    };
    if !wreal {
        let _ = writeln!(t, "(* blackbox *)\nmodule {} (", m.name);
        if !supplies.is_empty() {
            t.push_str("`ifdef USE_POWER_PINS\n");
            for p in &supplies {
                let _ = writeln!(t, "    inout {},", p.name);
            }
            t.push_str("`endif\n");
        }
        for (i, p) in signals.iter().enumerate() {
            let comma = if i + 1 < signals.len() { "," } else { "" };
            let _ = writeln!(t, "    {}{comma}", decl(p));
        }
        t.push_str(");\nendmodule\n");
        return t;
    }
    let names: Vec<&str> = signals.iter().map(|p| p.name.as_str()).collect();
    let _ = writeln!(t, "(* blackbox *)\nmodule {} ({}", m.name, names.join(", "));
    if !supplies.is_empty() {
        let s: Vec<&str> = supplies.iter().map(|p| p.name.as_str()).collect();
        let _ = writeln!(t, "`ifdef USE_POWER_PINS\n    , {}\n`endif", s.join(", "));
    }
    t.push_str(");\n");
    if !supplies.is_empty() {
        t.push_str("`ifdef USE_POWER_PINS\n");
        for p in &supplies {
            let _ = writeln!(t, "    inout {};", p.name);
        }
        t.push_str("`endif\n");
    }
    for p in &signals {
        let _ = writeln!(t, "    {};", decl(p));
    }
    let analog: Vec<&&Pin> = signals
        .iter()
        .filter(|p| p.kind == PinKind::Analog)
        .collect();
    if !analog.is_empty() {
        t.push_str("`ifdef AISPICE_WREAL\n");
        for p in analog {
            let _ = writeln!(t, "    wreal {};", p.name);
        }
        t.push_str("`endif\n");
    }
    t.push_str("endmodule\n");
    t
}

/// The SPICE view: the LVS netlist as `.subckt <name> <pins>` with a
/// `*.PININFO` line (I, O or B per pin). Its ports must be exactly the
/// manifest's pin bits.
pub fn spice(lvs: &LvsNetlist, m: &Manifest, version: &str) -> Result<String, String> {
    let bits: BTreeSet<String> = m
        .pins
        .iter()
        .flat_map(|p| p.bits())
        .map(|b| b.to_ascii_lowercase())
        .collect();
    let ports: BTreeSet<String> = lvs.ports.iter().map(|p| p.to_ascii_lowercase()).collect();
    if bits != ports {
        return Err(format!(
            "the netlist's ports ({}) are not the macro's pins ({})",
            lvs.ports.join(", "),
            bits.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    let info: Vec<String> = m
        .pins
        .iter()
        .flat_map(|p| {
            let d = match (p.is_supply(), p.direction) {
                (true, _) | (false, Direction::Inout) => 'B',
                (false, Direction::Input) => 'I',
                (false, Direction::Output) => 'O',
            };
            p.bits().into_iter().map(move |b| format!("{b}:{d}"))
        })
        .collect();
    let mut t = format!(
        "* {}: netlist for LVS written by aispice {version}, in the CDL form IHP's\n* KLayout LVS deck reads. The extracted netlist is {}.pex.spice.\n",
        m.name, m.name
    );
    let mut wrote_info = false;
    for line in lvs.text.lines() {
        if line.starts_with('*') && !wrote_info {
            continue;
        }
        t.push_str(line);
        t.push('\n');
        if !wrote_info && line.to_ascii_lowercase().starts_with(".subckt") {
            let _ = writeln!(t, "*.PININFO {}", info.join(" "));
            wrote_info = true;
        }
    }
    if !wrote_info {
        return Err("the LVS netlist has no .subckt line".into());
    }
    Ok(t)
}

/// One spec of the spec table.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpecEntry {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub unit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Value per corner; corners without a finite value are left out.
    pub measured: BTreeMap<String, f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

/// `<name>.specs.toml` text (`macro.specs/v1`).
pub fn specs_toml(entries: &[SpecEntry], version: &str) -> Result<String, String> {
    #[derive(Serialize)]
    struct File {
        schema: &'static str,
        specs: Vec<SpecEntry>,
    }
    let specs: Vec<SpecEntry> = entries
        .iter()
        .map(|e| SpecEntry {
            measured: e
                .measured
                .iter()
                .filter(|(_, v)| v.is_finite())
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            min: e.min.filter(|v| v.is_finite()),
            max: e.max.filter(|v| v.is_finite()),
            ..e.clone()
        })
        .collect();
    if let Some(e) = specs.iter().find(|e| e.measured.is_empty()) {
        return Err(format!(
            "spec {} has no measured value at any corner",
            e.name
        ));
    }
    let body = toml::to_string(&File {
        schema: aispice_core::macropkg::SPECS_SCHEMA,
        specs,
    })
    .map_err(|e| e.to_string())?;
    Ok(format!(
        "# Measured by aispice {version}: each spec at every corner after layout, and\n# <spec>_schematic before layout, for comparison.\n{body}"
    ))
}

/// The magic port class and use of a pin.
fn port_words(p: &Pin) -> String {
    let use_ = match p.kind {
        PinKind::Signal => "signal",
        PinKind::Clock => "clock",
        PinKind::Analog => "analog",
        PinKind::Power => "power",
        PinKind::Ground => "ground",
    };
    format!("{}:{}:{use_}", p.name, p.direction.as_str())
}

/// Write the LEF abstract of the manifest's cell in `gds` with Magic
/// (`lef write -hide`): pins on their layers with the manifest's directions
/// and uses, everything else obstruction. Returns the text and the parsed
/// macro.
pub async fn lef(
    programs: &Programs,
    files: &PdkLayoutFiles,
    gds: &Path,
    m: &Manifest,
    cancel: &CancellationToken,
) -> Result<(String, LefMacro), LayoutError> {
    tools::check_path(gds)?;
    files.check_magic()?;
    if !is_identifier(&m.name) {
        return Err(LayoutError::Input(format!(
            "{} is not a macro name",
            m.name
        )));
    }
    for p in &m.pins {
        if p.width.is_some() || !is_identifier(&p.name) {
            return Err(LayoutError::Input(format!(
                "pin {} cannot be written by Magic's LEF writer (a bus or not an identifier)",
                p.name
            )));
        }
    }
    let run = RunDir::new()?;
    let script = run.script("lef.tcl", LEF_SCRIPT)?;
    let out = run.path().join(format!("{}.lef", m.name));
    let pins: Vec<String> = m.pins.iter().map(port_words).collect();
    let env = [
        ("AISPICE_GDS", gds.display().to_string()),
        ("AISPICE_CELL", m.name.clone()),
        ("AISPICE_LEF", out.display().to_string()),
        ("AISPICE_PINS", pins.join(" ")),
    ];
    let res = tools::magic(programs, run.path(), &files.magicrc, &script, &env, cancel).await?;
    if !res.success() || !out.is_file() {
        return Err(LayoutError::Failed {
            program: "Magic (writing the LEF abstract)".into(),
            message: tools::tail(&res.combined(), 10),
        });
    }
    let size = std::fs::metadata(&out).map(|x| x.len()).unwrap_or(0);
    if size > MAX_LEF_BYTES {
        return Err(LayoutError::Input("the LEF abstract is too large".into()));
    }
    let text = std::fs::read_to_string(&out).map_err(|e| LayoutError::Io(e.to_string()))?;
    let macros = parse_lef(&text).map_err(|e| LayoutError::Input(format!("Magic's LEF: {e}")))?;
    let found = macros
        .get(&m.name)
        .cloned()
        .ok_or_else(|| LayoutError::Input(format!("Magic's LEF has no macro {}", m.name)))?;
    for p in &m.pins {
        if !found.pins.contains_key(&p.name) {
            return Err(LayoutError::Input(format!(
                "Magic's LEF has no pin {}; is it labelled in the layout?",
                p.name
            )));
        }
    }
    Ok((text, found))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aispice_core::macropkg::{
        Corner, Domain, Pdk, Producer, SCHEMA_VERSION, Size, liberty as lib, spice as sp,
        verilog as vl,
    };
    use aispice_core::netlist::parse;

    const OTA: &str = "* 5T OTA\nVDD vdd 0 1.2\nVP inp 0 0.6 AC 0.5\nVM inm 0 0.6 AC -0.5\nIB vdd vbn 20u\nCL out 0 1p\nXM1 x inp tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM2 out inm tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM3 x x vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM4 out x vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM5 tail vbn 0 0 sg13_lv_nmos w=4u l=1u ng=1 m=1\nXM6 vbn vbn 0 0 sg13_lv_nmos w=2u l=1u ng=1 m=1\n.ac dec 20 1 1G\n";

    fn devices() -> Vec<String> {
        ["XM1", "XM2", "XM3", "XM4", "XM5", "XM6"]
            .map(String::from)
            .to_vec()
    }

    fn manifest(pins: Vec<Pin>) -> Manifest {
        Manifest {
            schema: SCHEMA_VERSION.into(),
            name: "ota".into(),
            kind: aispice_core::macropkg::MacroKind::Analog,
            description: None,
            pdk: Pdk {
                name: "ihp-sg13g2".into(),
                commit: "22f2a25".into(),
            },
            producer: Producer {
                tool: "aispice".into(),
                version: "0.2.0".into(),
                source: None,
            },
            size: Size {
                width_um: 24.39,
                height_um: 25.62,
            },
            domains: vec![Domain {
                name: "core".into(),
                voltage: 1.2,
                power: vec!["vdd".into()],
                ground: vec!["vss".into()],
            }],
            pins,
            corners: vec![Corner {
                name: "tt_27".into(),
                process: "tt".into(),
                temperature: 27.0,
                voltages: BTreeMap::new(),
                liberty: None,
            }],
            extra: None,
        }
    }

    fn ota_pins() -> Vec<Pin> {
        let n = rename_ground(&parse(OTA), "vss").unwrap();
        let ports: Vec<String> = ["inm", "inp", "out", "vbn", "vdd", "vss"]
            .map(String::from)
            .to_vec();
        infer_pins(&n, &devices(), &ports, "vss")
    }

    #[test]
    fn block_names_that_are_or_look_like_pdk_cells_are_refused() {
        let pdk = tempfile::tempdir().unwrap();
        for (lib, file, text) in [
            (
                "sg13g2_stdcell",
                "sg13g2_stdcell.lef",
                "MACRO sg13g2_inv_1\n  CLASS CORE ;\nEND sg13g2_inv_1\n",
            ),
            (
                "sg13g2_io",
                "sg13g2_io.lef",
                "MACRO sg13g2_Corner\nEND sg13g2_Corner\n",
            ),
            (
                "sg13g2_sram",
                "RM_IHPSG13_1P_256x8_c3_bm_bist.lef",
                "MACRO RM_IHPSG13_1P_256x8_c3_bm_bist\nEND RM_IHPSG13_1P_256x8_c3_bm_bist\n",
            ),
        ] {
            let dir = pdk.path().join("libs.ref").join(lib).join("lef");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), text).unwrap();
        }
        let cells = pdk_cell_names(pdk.path()).unwrap();
        assert!(cells.contains("sg13g2_inv_1") && cells.contains("sg13g2_corner"));
        for bad in [
            "sg13g2_inv_1",
            "SG13G2_INV_1",
            "Sg13G2_mine",
            "sg13g2_",
            "rm_ihpsg13_1p_256x8_c3_bm_bist",
            "RM_IHPSG13_1P_256X8_C3_BM_BIST",
        ] {
            assert!(pdk_name_clash(bad, &cells).is_some(), "{bad}");
        }
        for good in ["ota5t", "sg13_ota", "my_sg13g2_block", "rm_ihpsg13_mine"] {
            assert!(pdk_name_clash(good, &cells).is_none(), "{good}");
        }
        // A checkout without the libraries still refuses the prefix.
        let empty = tempfile::tempdir().unwrap();
        let none = pdk_cell_names(empty.path()).unwrap();
        assert!(none.is_empty());
        assert!(pdk_name_clash("SG13G2_INV_1", &none).is_some());
    }

    #[test]
    fn identifiers_and_supply_names() {
        for ok in ["ota", "_x", "a1_b", &"a".repeat(64)] {
            assert!(is_identifier(ok), "{ok}");
        }
        for bad in ["", "0", "1a", "a-b", "a b", "a[0]", &"a".repeat(65)] {
            assert!(!is_identifier(bad), "{bad}");
        }
        for p in ["vdd", "VDD", "avdd", "vdd_core", "vcc", "vpwr"] {
            assert!(power_like(p), "{p}");
        }
        for g in ["vss", "gnd", "VGND", "agnd", "0", "vss_io"] {
            assert!(ground_like(g), "{g}");
        }
        assert!(!power_like("out") && !ground_like("vbn"));
    }

    #[test]
    fn pins_follow_what_the_devices_do() {
        let pins = ota_pins();
        let got: Vec<(&str, Direction, PinKind)> = pins
            .iter()
            .map(|p| (p.name.as_str(), p.direction, p.kind))
            .collect();
        use Direction::*;
        use PinKind::*;
        assert_eq!(
            got,
            [
                ("inm", Input, Analog),
                ("inp", Input, Analog),
                ("out", Output, Analog),
                // The bias diode's drain is its own gate: an input.
                ("vbn", Input, Analog),
                ("vdd", Inout, Power),
                ("vss", Inout, Ground),
            ]
        );
        assert_eq!(pins[0].domain.as_deref(), Some("core"));
        assert!(pins[4].domain.is_none());
    }

    #[test]
    fn ground_is_renamed_only_where_it_is_free() {
        let n = rename_ground(&parse(OTA), "vss").unwrap();
        assert!(n.nodes().contains(&"vss".to_string()));
        assert!(!n.nodes().contains(&"0".to_string()));
        assert!(
            rename_ground(&parse(OTA), "vbn")
                .unwrap_err()
                .contains("vbn")
        );
        assert_eq!(supply_voltage(&n, "vdd", "vss"), Some(1.2));
        assert_eq!(
            supply_voltage(&parse("t\nV1 avdd 0 DC 3.3\n"), "avdd", "vss"),
            Some(3.3)
        );
        assert_eq!(supply_voltage(&n, "out", "vss"), None);
    }

    #[test]
    fn corner_names_are_identifiers() {
        assert_eq!(corner_ident("tt", 27.0, &[]), "tt_27");
        assert_eq!(corner_ident("ff", -40.0, &[]), "ff_m40");
        assert_eq!(
            corner_ident("ss", 125.0, &[("VDD".into(), 1.08)]),
            "ss_125_vdd_1v08"
        );
        assert_eq!(
            corner_ident("mos=ss res=wcs", 27.5, &[]),
            "mos_ss_res_wcs_27p5"
        );
        for c in [
            corner_ident("", 27.0, &[]),
            corner_ident("9x", 0.0, &[]),
            corner_ident(&"x".repeat(100), 1.0, &[]),
        ] {
            assert!(is_identifier(&c), "{c}");
        }
    }

    #[test]
    fn pin_capacitance_sums_every_capacitor_on_the_net() {
        let ex = ".subckt ota inm inp out vss\nX0 a inp b vss sg13_lv_nmos w=1u l=1u\nC0 inp inm 0.2f\nC1 inp vss 1.5f\nC2 out vss 2f\nC3 a vss 3f\n.ends\n";
        let pins: Vec<String> = ["inp", "inm", "out", "vbn"].map(String::from).to_vec();
        let caps = pin_caps(ex, &pins);
        assert!((caps["inp"] - 0.0017).abs() < 1e-12, "{caps:?}");
        assert!((caps["inm"] - 0.0002).abs() < 1e-12);
        assert!((caps["out"] - 0.002).abs() < 1e-12);
        assert_eq!(caps["vbn"], 0.0);
    }

    #[test]
    fn the_liberty_view_reads_back_with_its_pins() {
        let m = manifest(ota_pins());
        let mut caps = BTreeMap::new();
        caps.insert("inp".to_string(), 0.0123);
        let text = liberty(&m, &caps, "0.2.0");
        let cells = lib::parse_liberty(&text).unwrap();
        let cell = cells.get("ota").expect("cell ota");
        let names: BTreeSet<&str> = cell.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            BTreeSet::from(["inm", "inp", "out", "vbn", "vdd", "vss"])
        );
        assert!(cell["vdd"].pg && cell["vdd"].pg_type.ends_with("_power"));
        assert_eq!(cell["out"].direction, "output");
        assert!(text.contains("pg_type : primary_power"), "{text}");
        assert!(text.contains("capacitance : 0.012300"), "{text}");
        assert!(text.contains("voltage_map (vdd, 1.2)"), "{text}");
        assert!(!text.contains("timing ("), "{text}");
    }

    #[test]
    fn the_verilog_views_declare_every_signal_pin() {
        let m = manifest(ota_pins());
        for wreal in [false, true] {
            let text = verilog(&m, wreal, "0.2.0");
            let ports = vl::verilog_ports(&text, "ota").unwrap().unwrap();
            let got: Vec<(&str, &str)> = ports
                .iter()
                .map(|p| (p.name.as_str(), p.direction.as_str()))
                .collect();
            for want in [("inm", "input"), ("out", "output"), ("vdd", "inout")] {
                assert!(got.contains(&want), "{want:?} in {got:?}\n{text}");
            }
            assert_eq!(text.contains("wreal"), wreal, "{text}");
        }
    }

    #[test]
    fn the_spice_view_has_the_pins_and_their_directions() {
        let m = manifest(ota_pins());
        let n = rename_ground(&parse(OTA), "vss").unwrap();
        let lvs = crate::lvs::lvs_netlist(&n, "ota", Some(&devices()), &[]).unwrap();
        let text = spice(&lvs, &m, "0.2.0").unwrap();
        let sub = sp::spice_subckt(&text, "ota").unwrap().unwrap();
        let mut ports = sub.ports.clone();
        ports.sort();
        assert_eq!(ports, ["inm", "inp", "out", "vbn", "vdd", "vss"]);
        assert!(
            text.contains("*.PININFO inm:I inp:I out:O vbn:I vdd:B vss:B"),
            "{text}"
        );
        let mut fewer = m.clone();
        fewer.pins.retain(|p| p.name != "vbn");
        assert!(spice(&lvs, &fewer, "0.2.0").unwrap_err().contains("vbn"));
    }

    #[test]
    fn the_spec_table_is_toml_with_finite_values() {
        let mut measured = BTreeMap::new();
        measured.insert("tt_27".to_string(), 4.2e7);
        measured.insert("ss_125".to_string(), f64::NAN);
        let entries = vec![SpecEntry {
            name: "gbw".into(),
            description: Some("Unity-gain frequency".into()),
            unit: "Hz".into(),
            min: Some(1e6),
            max: None,
            measured,
            definition: Some("gbw = unity_gain_freq(V(out)) >= 1Meg".into()),
        }];
        let text = specs_toml(&entries, "0.2.0").unwrap();
        let v: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(v["schema"].as_str(), Some("macro.specs/v1"));
        let spec = &v["specs"].as_array().unwrap()[0];
        assert_eq!(spec["measured"]["tt_27"].as_float(), Some(4.2e7));
        assert!(spec["measured"].get("ss_125").is_none());
        let mut empty = entries[0].clone();
        empty.measured.clear();
        assert!(specs_toml(&[empty], "0.2.0").is_err());
    }

    #[test]
    fn port_words_carry_direction_and_use() {
        let pins = ota_pins();
        let words: Vec<String> = pins.iter().map(port_words).collect();
        assert_eq!(words[0], "inm:input:analog");
        assert_eq!(words[4], "vdd:inout:power");
    }
}

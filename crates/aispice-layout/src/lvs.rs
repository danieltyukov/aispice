//! Layout versus schematic with the PDK's KLayout deck.
//!
//! The schematic side is written from the circuit's netlist: only its PDK
//! devices, in the CDL form IHP's LVS reader takes (the form xschem's
//! `lvs_format` writes: `M1 d g s b sg13_lv_nmos w=.. l=.. ng=.. m=..`),
//! inside `.subckt <cell> <ports>`. Ports are the nets the devices share with
//! the rest of the schematic: sources, test-bench parts, the outside world.
//! The layout names them with labels.
//!
//! The result database is read back through KLayout's own API by
//! `klayout/lvs_report.py`, which writes every net, device and pin pair with
//! its status as JSON.

use crate::tools::{self, LayoutError, PdkLayoutFiles, Programs, RunDir};
use aispice_core::netlist::{Line, Netlist};
use aispice_core::pdk::{Device, Family};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

const REPORT_SCRIPT: &str = include_str!("../klayout/lvs_report.py");
/// The JSON report is not read past this size.
const MAX_JSON_BYTES: u64 = 16 << 20;

/// The schematic netlist LVS compares against.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LvsNetlist {
    pub text: String,
    pub cell: String,
    /// Nets the layout must label.
    pub ports: Vec<String>,
    /// Instance names of the devices kept, as in the schematic.
    pub devices: Vec<String>,
}

/// Parameters on an instance line: `w=1u l=0.13u` and the like.
pub fn params(rest: &str) -> BTreeMap<String, String> {
    rest.split_whitespace()
        .filter_map(|t| t.split_once('='))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
        .collect()
}

/// An instance's parameter, else the device's default.
fn param(given: &BTreeMap<String, String>, device: &Device, key: &str) -> Option<String> {
    given
        .get(key)
        .cloned()
        .or_else(|| params(device.params).get(key).cloned())
}

/// The CDL line IHP's LVS reader takes for one PDK device.
fn device_line(
    name: &str,
    nodes: &[String],
    device: &Device,
    given: &BTreeMap<String, String>,
) -> Result<String, String> {
    let p = |k: &str| param(given, device, k).unwrap_or_else(|| "1".into());
    let nodes = nodes.join(" ");
    Ok(match (device.family, device.name) {
        (Family::Mos, _) if device.is_mosfet() => format!(
            "M{name} {nodes} {} w={} l={} ng={} m={}",
            device.name,
            p("w"),
            p("l"),
            p("ng"),
            p("m")
        ),
        (Family::Res, "rsil" | "rppd" | "rhigh") => format!(
            "R{name} {nodes} {} w={} l={} b={} m={}",
            device.name,
            p("w"),
            p("l"),
            param(given, device, "b").unwrap_or_else(|| "0".into()),
            p("m")
        ),
        (Family::Cap, "cap_cmim") => format!(
            "C{name} {nodes} cap_cmim w={} l={} m={}",
            p("w"),
            p("l"),
            p("m")
        ),
        (Family::Hbt, "npn13G2" | "npn13G2l" | "npn13G2v") => {
            let el = param(given, device, "el")
                .and_then(|v| aispice_core::units::parse(&v))
                .unwrap_or(1.0);
            let (le, we) = match device.name {
                "npn13G2" => ("900e-9".to_string(), "70n"),
                "npn13G2l" => (format!("{}", el * 1e-6), "70n"),
                _ => (format!("{}", el * 1e-6), "120n"),
            };
            format!(
                "Q{name} {nodes} {} le={le} we={we} m={}",
                device.name,
                p("nx")
            )
        }
        _ => {
            return Err(format!(
                "LVS does not handle {} instances yet; the devices it compares are the MOS transistors, rsil, rppd, rhigh, cap_cmim and the npn13G2 HBTs",
                device.name
            ));
        }
    })
}

/// The PDK device a netlist line instantiates, if any.
fn pdk_device(e: &aispice_core::netlist::Element) -> Option<&'static Device> {
    (e.letter() == 'X')
        .then(|| e.first_word().and_then(aispice_core::pdk::device))
        .flatten()
}

/// Whether a netlist line is one of the devices LVS keeps: a PDK device,
/// and among `only` (netlist names, `XM1` or `M1`) when that is given.
fn kept(e: &aispice_core::netlist::Element, only: Option<&[String]>) -> bool {
    pdk_device(e).is_some()
        && only.is_none_or(|o| {
            let bare = e.name.strip_prefix(['X', 'x']).unwrap_or(&e.name);
            o.iter().any(|n| {
                n.eq_ignore_ascii_case(&e.name)
                    || n.strip_prefix(['X', 'x'])
                        .unwrap_or(n)
                        .eq_ignore_ascii_case(bare)
            })
        })
}

/// The nets a block of devices shares with the rest of the circuit (and
/// ground): what its layout must label. `only` picks the block's devices by
/// netlist name; without it the block is every PDK device.
pub fn block_ports(netlist: &Netlist, only: Option<&[String]>) -> Vec<String> {
    let mut inside: BTreeSet<String> = BTreeSet::new();
    let mut outside: BTreeSet<String> = BTreeSet::new();
    for e in netlist.elements() {
        if kept(e, only) {
            inside.extend(e.nodes.iter().cloned());
        } else {
            outside.extend(e.nodes.iter().cloned());
        }
    }
    inside
        .into_iter()
        .filter(|n| outside.contains(n) || n == "0")
        .collect()
}

/// Write the LVS schematic netlist of a circuit: its PDK devices (or the
/// `only` ones) as CDL in `.subckt <cell> <ports>`, plus devices the layout
/// adds on purpose (`extra`: a generator's dummies).
pub fn lvs_netlist(
    netlist: &Netlist,
    cell: &str,
    only: Option<&[String]>,
    extra: &[crate::generator::ExtraDevice],
) -> Result<LvsNetlist, String> {
    tools::check_name("cell", cell).map_err(|e| e.to_string())?;
    let mut lines = Vec::new();
    let mut devices = Vec::new();
    for item in &netlist.items {
        match item {
            Line::Element(e) if kept(e, only) => {
                let device = pdk_device(e).expect("kept lines are PDK devices");
                if e.nodes.len() != device.pins.len() {
                    return Err(format!(
                        "{} has {} nodes, but {} has {} pins",
                        e.name,
                        e.nodes.len(),
                        device.name,
                        device.pins.len()
                    ));
                }
                let inst = e.name.strip_prefix(['X', 'x']).unwrap_or(&e.name);
                tools::check_name("instance", inst).map_err(|e| e.to_string())?;
                for n in &e.nodes {
                    tools::check_name("net", n).map_err(|e| e.to_string())?;
                }
                let rest = e.rest.split_once(char::is_whitespace).map_or("", |x| x.1);
                lines.push(device_line(inst, &e.nodes, device, &params(rest))?);
                devices.push(inst.to_string());
            }
            Line::Subckt(s) => {
                let hidden = s
                    .body
                    .iter()
                    .any(|l| matches!(l, Line::Element(e) if pdk_device(e).is_some()));
                if hidden {
                    return Err(format!(
                        "subcircuit {} holds PDK devices; LVS compares a flat circuit, so draw them at the top level",
                        s.name
                    ));
                }
            }
            _ => {}
        }
    }
    if devices.is_empty() {
        return Err(match only {
            Some(o) => format!("none of {} is a PDK device of the circuit", o.join(", ")),
            None => "the circuit has no PDK devices to compare".into(),
        });
    }
    for x in extra {
        // KLayout drops a transistor whose terminals are all one net when it
        // extracts the layout, so such a dummy is not compared.
        if x.d == x.g && x.g == x.s && x.s == x.b {
            continue;
        }
        tools::check_name("instance", &x.name).map_err(|e| e.to_string())?;
        for n in [&x.d, &x.g, &x.s, &x.b] {
            tools::check_name("net", n).map_err(|e| e.to_string())?;
        }
        if aispice_core::pdk::device(&x.model).is_none_or(|d| !d.is_mosfet()) {
            return Err(format!("{} is not a PDK transistor", x.model));
        }
        lines.push(format!(
            "M{} {} {} {} {} {} w={}u l={}u ng={} m=1",
            x.name,
            x.d,
            x.g,
            x.s,
            x.b,
            x.model,
            x.w as f64 / 1000.0,
            x.l as f64 / 1000.0,
            x.ng
        ));
        devices.push(x.name.clone());
    }
    let ports = block_ports(netlist, only);
    let mut text = format!(
        "* LVS netlist written by aispice\n.subckt {cell} {}\n",
        ports.join(" ")
    );
    for l in lines {
        text.push_str(&l);
        text.push('\n');
    }
    text.push_str(&format!(".ends {cell}\n"));
    Ok(LvsNetlist {
        text,
        cell: cell.to_string(),
        ports,
        devices,
    })
}

/// One pair from the comparison: a net, a device or a pin, by its name on
/// each side (`None` when one side has nothing to match it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pair {
    pub layout: Option<String>,
    pub schematic: Option<String>,
    /// `match`, `warning`, `mismatch`, `nomatch` or `skipped`.
    pub status: String,
}

impl Pair {
    pub fn ok(&self) -> bool {
        matches!(self.status.as_str(), "match" | "warning" | "skipped")
    }

    fn describe(&self, what: &str) -> String {
        match (&self.layout, &self.schematic) {
            (Some(l), Some(s)) => format!("{what} {s} (layout {l}): {}", self.status),
            (None, Some(s)) => format!("{what} {s}: in the schematic, not found in the layout"),
            (Some(l), None) => format!("{what} {l}: in the layout, not in the schematic"),
            (None, None) => format!("{what}: {}", self.status),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CircuitResult {
    pub layout: Option<String>,
    pub schematic: Option<String>,
    pub status: String,
    #[serde(default)]
    pub nets: Vec<Pair>,
    #[serde(default)]
    pub devices: Vec<Pair>,
    #[serde(default)]
    pub pins: Vec<Pair>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LvsReport {
    #[serde(default)]
    pub cell: String,
    #[serde(default)]
    pub matched: bool,
    pub circuits: Vec<CircuitResult>,
    /// KLayout's comparison messages.
    #[serde(default)]
    pub log: Vec<String>,
    /// The layout's extracted netlist and the files kept.
    #[serde(default)]
    pub extracted: Option<PathBuf>,
    #[serde(default)]
    pub lvsdb: Option<PathBuf>,
    #[serde(default)]
    pub schematic: Option<PathBuf>,
}

impl LvsReport {
    /// Nets, devices and pins that did not match, in words.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.circuits {
            out.extend(c.nets.iter().filter(|p| !p.ok()).map(|p| p.describe("net")));
            out.extend(
                c.devices
                    .iter()
                    .filter(|p| !p.ok())
                    .map(|p| p.describe("device")),
            );
            out.extend(c.pins.iter().filter(|p| !p.ok()).map(|p| p.describe("pin")));
        }
        out
    }

    /// Schematic names of the nets that did not match.
    pub fn bad_nets(&self) -> Vec<String> {
        self.circuits
            .iter()
            .flat_map(|c| c.nets.iter())
            .filter(|p| !p.ok())
            .filter_map(|p| p.schematic.clone().or_else(|| p.layout.clone()))
            .collect()
    }

    pub fn to_text(&self) -> String {
        if self.matched {
            return format!(
                "LVS clean: the layout of {} matches the schematic.\n",
                self.cell
            );
        }
        let mut t = format!(
            "LVS: the layout of {} does not match the schematic.\n",
            self.cell
        );
        for p in self.problems().iter().take(40) {
            t.push_str(&format!("  {p}\n"));
        }
        for l in self.log.iter().take(20) {
            t.push_str(&format!("  log: {l}\n"));
        }
        t
    }
}

/// Run LVS on a cell against a schematic netlist from [`lvs_netlist`].
pub async fn run(
    programs: &Programs,
    files: &PdkLayoutFiles,
    gds: &Path,
    schematic: &LvsNetlist,
    out_dir: &Path,
    cancel: &CancellationToken,
) -> Result<LvsReport, LayoutError> {
    tools::check_path(gds)?;
    tools::check_path(out_dir)?;
    tools::check_name("cell", &schematic.cell)?;
    std::fs::create_dir_all(out_dir).map_err(|e| LayoutError::Io(e.to_string()))?;
    let sch = out_dir.join("schematic.cir");
    std::fs::write(&sch, &schematic.text).map_err(|e| LayoutError::Io(e.to_string()))?;
    let lvsdb = out_dir.join("lvs.lvsdb");
    let extracted = out_dir.join("extracted.cir");
    let log = out_dir.join("lvs.log");
    let f = |b: bool| if b { "true" } else { "false" }.to_string();
    let vars: Vec<(&str, String)> = vec![
        ("run_mode", "deep".into()),
        ("no_net_names", f(false)),
        ("spice_comments", f(false)),
        ("net_only", f(false)),
        ("top_lvl_pins", f(false)),
        ("no_simplify", f(false)),
        ("no_series_res", f(false)),
        ("no_parallel_res", f(false)),
        ("combine_devices", f(false)),
        ("purge", f(false)),
        ("purge_nets", f(false)),
        ("topcell", schematic.cell.clone()),
        ("input", gds.display().to_string()),
        ("schematic", sch.display().to_string()),
        ("ignore_top_ports_mismatch", f(false)),
        ("implicit_nets", "\"\"".into()),
        ("report", lvsdb.display().to_string()),
        ("log", log.display().to_string()),
        ("target_netlist", extracted.display().to_string()),
    ];
    let run = RunDir::new()?;
    let out = tools::klayout(programs, run.path(), &files.lvs_deck, &vars, cancel).await?;
    if !lvsdb.is_file() {
        let all = out.combined();
        let tail: Vec<&str> = all
            .lines()
            .filter(|l| !l.contains("Memory Usage"))
            .collect();
        let start = tail.len().saturating_sub(10);
        return Err(LayoutError::Failed {
            program: "KLayout LVS".into(),
            message: tail[start..].join("\n"),
        });
    }
    let mut report = read_lvsdb(programs, &lvsdb, out_dir, cancel).await?;
    report.cell = schematic.cell.clone();
    report.extracted = extracted.is_file().then_some(extracted);
    report.lvsdb = Some(lvsdb);
    report.schematic = Some(sch);
    Ok(report)
}

/// Read an LVS database through KLayout's API.
pub async fn read_lvsdb(
    programs: &Programs,
    lvsdb: &Path,
    out_dir: &Path,
    cancel: &CancellationToken,
) -> Result<LvsReport, LayoutError> {
    let run = RunDir::new()?;
    let script = run.script("lvs_report.py", REPORT_SCRIPT)?;
    let json = out_dir.join("lvs.json");
    let out = tools::klayout(
        programs,
        run.path(),
        &script,
        &[
            ("db", lvsdb.display().to_string()),
            ("out", json.display().to_string()),
        ],
        cancel,
    )
    .await?;
    let size = std::fs::metadata(&json).map(|m| m.len()).unwrap_or(0);
    if !out.success() || size == 0 {
        return Err(LayoutError::Failed {
            program: "KLayout (reading the LVS result)".into(),
            message: out
                .combined()
                .lines()
                .rev()
                .take(6)
                .collect::<Vec<_>>()
                .join("\n"),
        });
    }
    if size > MAX_JSON_BYTES {
        return Err(LayoutError::Input(
            "the LVS result is too large to read".into(),
        ));
    }
    let text = std::fs::read_to_string(&json).map_err(|e| LayoutError::Io(e.to_string()))?;
    parse_report(&text).map_err(LayoutError::Input)
}

/// Parse the JSON `lvs_report.py` writes.
pub fn parse_report(text: &str) -> Result<LvsReport, String> {
    let mut r: LvsReport =
        serde_json::from_str(text).map_err(|e| format!("the LVS result is not readable: {e}"))?;
    r.matched = !r.circuits.is_empty()
        && r.circuits
            .iter()
            .all(|c| matches!(c.status.as_str(), "match" | "warning" | "skipped"))
        && r.problems().is_empty();
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aispice_core::netlist::parse;

    #[test]
    fn pdk_devices_become_cdl_and_test_bench_parts_stay_out() {
        let n = parse(
            "* ota\nVDD vdd 0 1.2\nVIN inp 0 0.6 AC 1\nIB vdd vbn 20u\nC1 out 0 1p\nXM1 d1 inp tail 0 sg13_lv_nmos w=2u l=0.5u ng=2 m=1\nXM2 out inm tail 0 sg13_lv_nmos w=2u l=0.5u\nXM5 tail vbn 0 0 sg13_lv_nmos w=4u l=1u ng=1 m=1\nXR1 a b 0 rppd w=1u l=4u\nXC1 out 0 cap_cmim w=10u l=10u m=1\nXQ1 c b e 0 npn13G2 Nx=2\nXM9 vdd inm vdd vdd sg13_lv_pmos\n.ac dec 10 1 1G\n",
        );
        let l = lvs_netlist(&n, "ota", None, &[]).unwrap();
        assert_eq!(l.devices, ["M1", "M2", "M5", "R1", "C1", "Q1", "M9"]);
        // Ports: nets a device shares with a source or a test-bench part,
        // and ground.
        assert_eq!(l.ports, ["0", "inp", "out", "vbn", "vdd"]);
        for line in [
            ".subckt ota 0 inp out vbn vdd",
            "MM1 d1 inp tail 0 sg13_lv_nmos w=2u l=0.5u ng=2 m=1",
            "MM2 out inm tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1",
            "RR1 a b 0 rppd w=1u l=4u b=0 m=1",
            "CC1 out 0 cap_cmim w=10u l=10u m=1",
            "QQ1 c b e 0 npn13G2 le=900e-9 we=70n m=2",
            "MM9 vdd inm vdd vdd sg13_lv_pmos w=0.15u l=0.13u ng=1 m=1",
            ".ends ota",
        ] {
            assert!(l.text.contains(line), "{line}\n{}", l.text);
        }
        assert!(!l.text.contains("VDD vdd"));
    }

    #[test]
    fn a_block_of_the_circuit_with_its_dummies() {
        let n = parse(
            "* ota\nVDD vdd 0 1.2\nXM1 x inp tail 0 sg13_lv_nmos w=2u l=0.5u\nXM2 out inm tail 0 sg13_lv_nmos w=2u l=0.5u\nXM5 tail vbn 0 0 sg13_lv_nmos w=4u l=1u\n.op\n",
        );
        let only = vec!["XM1".to_string(), "M2".to_string()];
        // The pair alone: tail now leaves the block (to M5), x and out
        // stay inside.
        assert_eq!(block_ports(&n, Some(&only)), ["0", "tail"]);
        let dummy = crate::generator::ExtraDevice {
            name: "pair_dummy1".into(),
            model: "sg13_lv_nmos".into(),
            w: 1000,
            l: 500,
            ng: 1,
            d: "tail".into(),
            g: "0".into(),
            s: "tail".into(),
            b: "0".into(),
        };
        let l = lvs_netlist(&n, "pair", Some(&only), &[dummy]).unwrap();
        assert_eq!(l.devices, ["M1", "M2", "pair_dummy1"]);
        assert!(l.text.contains(".subckt pair 0 tail\n"), "{}", l.text);
        assert!(
            l.text
                .contains("Mpair_dummy1 tail 0 tail 0 sg13_lv_nmos w=1u l=0.5u ng=1 m=1"),
            "{}",
            l.text
        );
        assert!(!l.text.contains("MM5"));
        let none = vec!["M7".to_string()];
        assert!(
            lvs_netlist(&n, "pair", Some(&none), &[])
                .unwrap_err()
                .contains("M7")
        );
    }

    #[test]
    fn what_lvs_cannot_compare_is_refused() {
        assert!(
            lvs_netlist(&parse("t\nR1 a 0 1k\n.op\n"), "x", None, &[])
                .unwrap_err()
                .contains("no PDK devices")
        );
        let tap = parse("t\nXT1 a 0 ptap1\n.op\n");
        assert!(
            lvs_netlist(&tap, "x", None, &[])
                .unwrap_err()
                .contains("ptap1")
        );
        let nested =
            parse("t\nXA a b blk\n.subckt blk a b\nXM1 a b 0 0 sg13_lv_nmos\n.ends\n.op\n");
        assert!(
            lvs_netlist(&nested, "x", None, &[])
                .unwrap_err()
                .contains("flat")
        );
        let odd = parse("t\nXM1 a b 0 0 sg13_lv_nmos\n.op\n");
        assert!(lvs_netlist(&odd, "bad name", None, &[]).is_err());
        let net = parse("t\nXM1 a;b g 0 0 sg13_lv_nmos\n.op\n");
        assert!(lvs_netlist(&net, "x", None, &[]).is_err());
    }

    #[test]
    fn reports_list_what_did_not_match() {
        let json = r#"{"circuits":[{"layout":"ota","schematic":"OTA","status":"mismatch",
            "nets":[{"layout":"$5","schematic":"TAIL","status":"mismatch"},{"layout":"out","schematic":"OUT","status":"match"},{"layout":null,"schematic":"VBN","status":"nomatch"}],
            "devices":[{"layout":"$1","schematic":"M1","status":"match"},{"layout":null,"schematic":"M5","status":"nomatch"}],
            "pins":[]}],"log":["Net TAIL is not matching any net from reference netlist"]}"#;
        let r = parse_report(json).unwrap();
        assert!(!r.matched);
        assert_eq!(r.bad_nets(), ["TAIL", "VBN"]);
        let text = r.to_text();
        assert!(text.contains("net TAIL (layout $5): mismatch"), "{text}");
        assert!(
            text.contains("device M5: in the schematic, not found in the layout"),
            "{text}"
        );
        let ok = parse_report(r#"{"circuits":[{"layout":"a","schematic":"A","status":"match","nets":[{"layout":"x","schematic":"X","status":"warning"}],"devices":[],"pins":[]}],"log":[]}"#).unwrap();
        assert!(ok.matched);
        assert!(parse_report("[]").is_err());
    }
}

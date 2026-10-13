//! Parasitic extraction with Magic and the PDK's tech file, and the
//! post-layout netlist: the circuit with its laid-out devices replaced by
//! one instance of the extracted subcircuit, joined by net name.
//!
//! Magic writes `X0 d g s b sg13_lv_nmos ad= pd= as= ps= w= l=` for each
//! finger, which the PDK's subcircuits simulate as they are, plus coupling
//! and ground capacitors and, with resistance, a resistor network per net.

use crate::tools::{self, LayoutError, PdkLayoutFiles, Programs, RunDir};
use aispice_core::netlist::{self, Element, Line, Netlist, Subckt};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

const SCRIPT: &str = include_str!("../magic/extract.tcl");
/// Extracted netlists larger than this are not read.
pub const MAX_BYTES: u64 = 64 << 20;
/// Capacitors smaller than this (fF) are dropped.
const CTHRESH: &str = "0.01";

/// An extracted cell.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Extracted {
    pub cell: String,
    /// The subcircuit's ports, in Magic's order.
    pub ports: Vec<String>,
    /// The `.subckt ... .ends` text, names made SPICE-safe.
    pub text: String,
    pub devices: usize,
    pub capacitors: usize,
    pub resistors: usize,
    /// Total capacitance, fF.
    pub total_cap_ff: f64,
    pub resistance: bool,
    /// Where the netlist was kept.
    pub path: Option<PathBuf>,
    pub notes: Vec<String>,
}

impl Extracted {
    pub fn to_text(&self) -> String {
        let mut t = format!(
            "Extracted {}: {} device fingers, {} capacitors ({:.1} fF in all){}; pins {}.\n",
            self.cell,
            self.devices,
            self.capacitors,
            self.total_cap_ff,
            if self.resistance {
                format!(", {} resistors", self.resistors)
            } else {
                String::new()
            },
            self.ports.join(", ")
        );
        for n in &self.notes {
            t.push_str(&format!("  note: {n}\n"));
        }
        t
    }
}

fn elements(sub: &Subckt) -> impl Iterator<Item = &Element> {
    sub.body.iter().filter_map(|l| match l {
        Line::Element(e) => Some(e),
        _ => None,
    })
}

/// The substrate net Magic named in a `.ext` file: `substrate "name" ...`.
fn substrate_name(ext: &str) -> Option<String> {
    ext.lines().find_map(|l| {
        let rest = l.strip_prefix("substrate \"")?;
        Some(rest.split('"').next()?.to_string())
    })
}

/// Make a node name safe for SPICE: Magic's internal names end in `#`.
fn safe_node(n: &str) -> String {
    n.replace('#', "_")
}

/// Read Magic's output into one clean subcircuit. With `substrate`, the
/// substrate net Magic split into pieces (named `<auto>` and `<auto>.tN`)
/// is joined again under that name and made a port.
pub fn clean(
    text: &str,
    cell: &str,
    substrate: Option<(&str, &str)>,
) -> Result<(String, Vec<String>, [usize; 3], f64), String> {
    let parsed = netlist::parse(text);
    let sub = parsed
        .subckts()
        .find(|s| s.name.eq_ignore_ascii_case(cell))
        .ok_or_else(|| format!("Magic's output has no subcircuit {cell}"))?;
    let rename = |n: &str| -> String {
        if let Some((auto, name)) = substrate {
            let stem = auto.trim_end_matches('#');
            if n == auto || n == stem || n.starts_with(&format!("{stem}.")) {
                return name.to_string();
            }
        }
        safe_node(n)
    };
    let mut ports: Vec<String> = sub.ports.iter().map(|p| rename(p)).collect();
    if let Some((_, name)) = substrate
        && !ports.iter().any(|p| p == name)
    {
        ports.insert(0, name.to_string());
    }
    let mut body = Vec::new();
    let mut counts = [0usize; 3];
    let mut total_cap = 0.0;
    for e in elements(sub) {
        let nodes: Vec<String> = e.nodes.iter().map(|n| rename(n)).collect();
        match e.letter() {
            'X' => counts[0] += 1,
            'C' => {
                counts[1] += 1;
                total_cap += e
                    .first_word()
                    .and_then(aispice_core::units::parse)
                    .unwrap_or(0.0)
                    * 1e15;
            }
            'R' => {
                if nodes.len() == 2 && nodes[0] == nodes[1] {
                    continue;
                }
                counts[2] += 1;
            }
            other => {
                return Err(format!(
                    "Magic's output has an element {} of a kind aispice does not expect ({other})",
                    e.name
                ));
            }
        }
        body.push(format!("{} {} {}", e.name, nodes.join(" "), e.rest));
    }
    let mut out = format!(".subckt {cell} {}\n", ports.join(" "));
    for l in body {
        out.push_str(&l);
        out.push('\n');
    }
    out.push_str(&format!(".ends {cell}\n"));
    Ok((out, ports, counts, total_cap))
}

/// One Magic pass in a fresh folder of its own: the netlist it wrote, and
/// the `.ext` file it left.
#[allow(clippy::too_many_arguments)]
async fn magic_pass(
    programs: &Programs,
    files: &PdkLayoutFiles,
    gds: &Path,
    cell: &str,
    resist: bool,
    unlabel: &str,
    cancel: &CancellationToken,
) -> Result<(String, String), LayoutError> {
    let run = RunDir::new()?;
    let script = run.script("extract.tcl", SCRIPT)?;
    let out = run.path().join(if resist { "r.spice" } else { "c.spice" });
    let out = out.as_path();
    let env = [
        ("AISPICE_GDS", gds.display().to_string()),
        ("AISPICE_CELL", cell.to_string()),
        ("AISPICE_OUT", out.display().to_string()),
        ("AISPICE_RESIST", if resist { "1" } else { "0" }.to_string()),
        ("AISPICE_UNLABEL", unlabel.to_string()),
        ("AISPICE_CTHRESH", CTHRESH.to_string()),
    ];
    let access = tools::Access::new()
        .read(&files.root)
        .read(gds)
        .write_file(out);
    let res = tools::magic(
        programs,
        &run,
        &files.magicrc,
        &script,
        &env,
        &access,
        cancel,
    )
    .await?;
    if !res.success() || !tools::is_output(out) {
        let why = if res.code.is_none() {
            "Magic crashed".to_string()
        } else {
            tools::tail(&res.combined(), 10)
        };
        return Err(LayoutError::Failed {
            program: "Magic".into(),
            message: why,
        });
    }
    let netlist = tools::read_output_text(out, MAX_BYTES)?;
    let ext = tools::read_output_text(&run.path().join(format!("{cell}.ext")), MAX_BYTES)
        .map_err(|e| LayoutError::Io(format!("Magic's .ext file: {e}")))?;
    Ok((netlist, ext))
}

/// Extract a cell with capacitance (and with `resistance`, resistor
/// networks), keeping the netlist in `out_dir`.
pub async fn run(
    programs: &Programs,
    files: &PdkLayoutFiles,
    gds: &Path,
    cell: &str,
    resistance: bool,
    out_dir: &Path,
    cancel: &CancellationToken,
) -> Result<Extracted, LayoutError> {
    tools::check_path(gds)?;
    tools::check_path(out_dir)?;
    tools::check_name("cell", cell)?;
    files.check_magic()?;
    std::fs::create_dir_all(out_dir).map_err(|e| LayoutError::Io(e.to_string()))?;
    let (mut text, ext) = magic_pass(programs, files, gds, cell, false, "", cancel).await?;
    let mut substrate = None;
    let mut notes = Vec::new();
    if resistance {
        let unlabel = substrate_name(&ext)
            .filter(|n| !n.ends_with('#'))
            .unwrap_or_default();
        if !unlabel.is_empty() {
            tools::check_name("net", &unlabel)?;
        }
        let (r_text, ext) = magic_pass(programs, files, gds, cell, true, &unlabel, cancel).await?;
        text = r_text;
        if !unlabel.is_empty() {
            if let Some(auto) = substrate_name(&ext) {
                substrate = Some((auto, unlabel.clone()));
            }
            notes.push(format!(
                "the substrate net {unlabel} is kept ideal: Magic cannot split a labelled substrate net into resistors"
            ));
        }
    }
    let (text, ports, counts, total) = clean(
        &text,
        cell,
        substrate.as_ref().map(|(a, n)| (a.as_str(), n.as_str())),
    )
    .map_err(LayoutError::Input)?;
    // Magic writes only in its own run folders, so `out_dir` holds
    // aispice's results alone; an earlier one is replaced, a link refused.
    let path = out_dir.join(format!("{cell}.extracted.spice"));
    tools::replace_file(&path, text.as_bytes())?;
    Ok(Extracted {
        cell: cell.to_string(),
        ports,
        text,
        devices: counts[0],
        capacitors: counts[1],
        resistors: counts[2],
        total_cap_ff: total,
        resistance,
        path: Some(path),
        notes,
    })
}

/// The extraction with one net renamed in its ports and its body: a macro's
/// ground pin back to the test bench's `0`, for instance.
pub fn rename_net(ex: &Extracted, from: &str, to: &str) -> Result<Extracted, String> {
    let parsed = netlist::parse(&format!("* extracted\n{}", ex.text));
    let sub = parsed
        .subckts()
        .next()
        .ok_or("the extracted netlist has no subcircuit")?;
    let swap = |n: &str| {
        if n.eq_ignore_ascii_case(from) {
            to.to_string()
        } else {
            n.to_string()
        }
    };
    let ports: Vec<String> = ex.ports.iter().map(|p| swap(p)).collect();
    let sub_ports: Vec<String> = sub.ports.iter().map(|p| swap(p)).collect();
    let mut text = format!(".subckt {} {}\n", ex.cell, sub_ports.join(" "));
    for e in elements(sub) {
        let nodes: Vec<String> = e.nodes.iter().map(|n| swap(n)).collect();
        text.push_str(&format!("{} {} {}\n", e.name, nodes.join(" "), e.rest));
    }
    text.push_str(&format!(".ends {}\n", ex.cell));
    Ok(Extracted {
        ports,
        text,
        ..ex.clone()
    })
}

/// Whether a netlist line is one of the laid-out devices: its name is in
/// `devices` (with or without the `X`).
fn laid_out(e: &Element, devices: &[String]) -> bool {
    let bare = e.name.strip_prefix(['X', 'x']).unwrap_or(&e.name);
    devices.iter().any(|d| {
        d.eq_ignore_ascii_case(&e.name)
            || d.strip_prefix(['X', 'x'])
                .unwrap_or(d)
                .eq_ignore_ascii_case(bare)
    })
}

/// The circuit with `devices` replaced by the extracted subcircuit. Every
/// net those devices share with the rest of the circuit must be a pin of
/// the layout, and every pin a net of the circuit.
pub fn post_layout(
    circuit: &Netlist,
    ex: &Extracted,
    devices: &[String],
) -> Result<Netlist, String> {
    let parsed = netlist::parse(&format!("* extracted\n{}", ex.text));
    let sub = parsed
        .subckts()
        .next()
        .ok_or("the extracted netlist has no subcircuit")?
        .clone();
    for e in elements(&sub) {
        if e.letter() == 'X' && e.first_word().and_then(aispice_core::pdk::device).is_none() {
            return Err(format!(
                "the extracted {} calls {}, which is not a PDK device",
                e.name,
                e.first_word().unwrap_or("")
            ));
        }
    }
    let mut removed_nets: BTreeSet<String> = BTreeSet::new();
    let mut kept_nets: BTreeSet<String> = BTreeSet::new();
    let mut removed = 0;
    let mut items = Vec::new();
    let mut at = None;
    for item in &circuit.items {
        match item {
            Line::Element(e) if laid_out(e, devices) => {
                removed += 1;
                removed_nets.extend(e.nodes.iter().cloned());
                at.get_or_insert(items.len());
            }
            Line::Element(e) => {
                kept_nets.extend(e.nodes.iter().cloned());
                items.push(item.clone());
            }
            _ => items.push(item.clone()),
        }
    }
    if removed == 0 {
        return Err(format!(
            "none of the laid-out devices ({}) is in the circuit",
            devices.join(", ")
        ));
    }
    let ports: BTreeSet<&String> = ex.ports.iter().collect();
    for n in removed_nets.intersection(&kept_nets) {
        if !ports.contains(n) {
            return Err(format!(
                "the circuit connects to net {n} of the laid-out devices, but the layout has no pin {n}; label it in the layout"
            ));
        }
    }
    for p in &ex.ports {
        if !kept_nets.contains(p) && !removed_nets.contains(p) && p != "0" {
            return Err(format!("the layout's pin {p} is not a net of the circuit"));
        }
    }
    let taken = |name: &str| circuit.subckts().any(|s| s.name.eq_ignore_ascii_case(name));
    let mut name = ex.cell.clone();
    if taken(&name) {
        name = format!("{}_layout", ex.cell);
    }
    items.insert(
        at.unwrap_or(0),
        Line::Element(Element {
            name: format!("X{}_layout", ex.cell),
            nodes: ex.ports.clone(),
            rest: name.clone(),
        }),
    );
    items.push(Line::Subckt(Subckt {
        name,
        ports: sub.ports.clone(),
        params: String::new(),
        body: sub.body.clone(),
    }));
    Ok(Netlist {
        title: circuit.title.clone(),
        items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAGIC_C: &str = "* NGSPICE file created from ota5t.ext - technology: ihp-sg13g2\n\n.subckt ota5t 0 inm inp out vbn vdd\nX0 0 vbn vbn 0 sg13_lv_nmos ad=0.19p pd=1.38u as=0.19p ps=1.38u w=1u l=1u\nX1 a_724_616# inp a_1000_3996# 0 sg13_lv_nmos ad=0.19p pd=1.38u as=0.19p ps=1.38u w=1u l=0.5u\nC0 inp inm 0.21053f\nC19 a_724_616# 0 2.89731f ; **FLOATING\n.ends\n";

    #[test]
    fn magic_output_becomes_a_clean_subcircuit() {
        let (text, ports, counts, total) = clean(MAGIC_C, "ota5t", None).unwrap();
        assert_eq!(ports, ["0", "inm", "inp", "out", "vbn", "vdd"]);
        assert_eq!(counts, [2, 2, 0]);
        assert!((total - 3.10784).abs() < 1e-6, "{total}");
        assert!(
            text.contains("X1 a_724_616_ inp a_1000_3996_ 0 sg13_lv_nmos"),
            "{text}"
        );
        assert!(!text.contains('#'), "{text}");
        assert!(text.ends_with(".ends ota5t\n"));
        assert!(clean(MAGIC_C, "other", None).is_err());
        assert!(clean(&MAGIC_C.replace("C0 inp", "V0 inp"), "ota5t", None).is_err());
    }

    #[test]
    fn a_split_substrate_is_joined_and_made_a_pin() {
        let r = "* x\n.subckt mirror out ref\nX0 w_n26_0.t1 ref ref w_n26_0.t0 sg13_lv_nmos w=1u l=1u\nR0 w_n26_0.t1 w_n26_0.n5 12.5\nR1 out out.t1 3.2\nC0 out 0 1f\n.ends\n";
        let (text, ports, counts, _) = clean(r, "mirror", Some(("w_n26_0#", "0"))).unwrap();
        assert_eq!(ports, ["0", "out", "ref"]);
        assert_eq!(counts, [1, 1, 1]);
        assert!(text.contains("X0 0 ref ref 0 sg13_lv_nmos"), "{text}");
        assert!(!text.contains("R0"), "{text}");
        assert!(text.contains("R1 out out.t1 3.2"), "{text}");
        assert_eq!(
            substrate_name("node \"a\" 1\nsubstrate \"w_n26_0#\" 6425 0\n"),
            Some("w_n26_0#".into())
        );
    }

    fn extracted() -> Extracted {
        let (text, ports, counts, total) = clean(MAGIC_C, "ota5t", None).unwrap();
        Extracted {
            cell: "ota5t".into(),
            ports,
            text,
            devices: counts[0],
            capacitors: counts[1],
            resistors: counts[2],
            total_cap_ff: total,
            resistance: false,
            path: None,
            notes: vec![],
        }
    }

    #[test]
    fn a_net_is_renamed_in_ports_and_body() {
        let mut ex = extracted();
        ex.text = ex
            .text
            .replace(" 0 ", " vss ")
            .replace("ota5t 0 ", "ota5t vss ");
        ex.ports = ex
            .ports
            .iter()
            .map(|p| if p == "0" { "vss".into() } else { p.clone() })
            .collect();
        let back = rename_net(&ex, "vss", "0").unwrap();
        assert_eq!(back.ports, extracted().ports);
        assert!(
            back.text
                .starts_with(".subckt ota5t 0 inm inp out vbn vdd\n"),
            "{}",
            back.text
        );
        assert!(!back.text.contains("vss"), "{}", back.text);
        assert!(
            back.text.contains("C19 a_724_616_ 0 2.89731f"),
            "{}",
            back.text
        );
    }

    #[test]
    fn the_post_layout_netlist_swaps_devices_for_the_layout() {
        let circuit = netlist::parse(
            "* ota\nVDD vdd 0 1.2\nVP inp 0 0.6\nVM inm 0 0.6\nIB vdd vbn 20u\nCL out 0 1p\nXM1 x inp tail 0 sg13_lv_nmos w=2u l=0.5u\nXM2 out inm tail 0 sg13_lv_nmos w=2u l=0.5u\nXM5 tail vbn 0 0 sg13_lv_nmos w=4u l=1u\n.lib /pdk/cornerMOSlv.lib mos_tt\n.ac dec 10 1 1G\n",
        );
        let devices: Vec<String> = ["XM1", "M2", "XM5"].map(String::from).to_vec();
        let n = post_layout(&circuit, &extracted(), &devices).unwrap();
        let text = netlist::write(&n);
        assert!(!text.contains("XM1 "), "{text}");
        assert!(
            text.contains("Xota5t_layout 0 inm inp out vbn vdd ota5t"),
            "{text}"
        );
        assert!(
            text.contains(".subckt ota5t 0 inm inp out vbn vdd"),
            "{text}"
        );
        assert!(text.contains(".lib /pdk/cornerMOSlv.lib mos_tt"), "{text}");
        assert!(text.contains("CL out 0 1p"), "{text}");
        // A test bench net the layout does not label is named.
        let mut ex = extracted();
        ex.ports.retain(|p| p != "vbn");
        let e = post_layout(&circuit, &ex, &devices).unwrap_err();
        assert!(e.contains("net vbn"), "{e}");
        let e = post_layout(&circuit, &extracted(), &["M9".to_string()]).unwrap_err();
        assert!(e.contains("M9"), "{e}");
        let mut bad = extracted();
        bad.text = bad.text.replace("sg13_lv_nmos", "evil");
        assert!(
            post_layout(&circuit, &bad, &devices)
                .unwrap_err()
                .contains("evil")
        );
    }
}

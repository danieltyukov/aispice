//! The blocks the generators know, the parts each expects, and how they map
//! onto rows.
//!
//! A role map names which instances play which part. When it is empty the
//! roles are found from the topology, and a generator always checks the
//! topology it expects (the pair's sources share the tail's drain, the load
//! is a mirror, ...) and says what differs.

use super::row::{Island, Kind, RowDev, RowSpec};
use super::{RoleDevices, Roles, rules};
use aispice_core::netlist::{Line, Netlist};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Template {
    /// Roles `ref` (the diode-connected device) and `out` (one or more).
    CurrentMirror,
    /// Role `pair` (two devices sharing a source).
    DiffPair,
    /// Five-transistor OTA: roles `pair`, `load` (a mirror), `tail` and
    /// optionally `bias` (the diode that mirrors into the tail).
    Ota5t,
    /// StrongARM latch comparator: roles `tail` (the clocked foot), `pair`
    /// (the inputs), `latch_n` and `latch_p` (the cross-coupled pairs) and
    /// `reset` (the clocked precharge devices).
    Comparator,
}

impl Template {
    pub fn name(self) -> &'static str {
        match self {
            Template::CurrentMirror => "current_mirror",
            Template::DiffPair => "diff_pair",
            Template::Ota5t => "ota5t",
            Template::Comparator => "comparator",
        }
    }
}

/// A PDK MOS transistor from the netlist.
#[derive(Debug, Clone, PartialEq)]
pub struct Mos {
    /// The netlist line's name: `XM1`.
    pub element: String,
    pub dev: RowDev,
    pub kind: Kind,
}

impl Mos {
    fn short(&self) -> &str {
        &self.dev.name
    }
    fn diode(&self) -> bool {
        self.dev.g == self.dev.d || self.dev.g == self.dev.s
    }
}

fn param(rest: &str, defaults: &str, key: &str) -> Option<f64> {
    let find = |text: &str| {
        text.split_whitespace()
            .filter_map(|t| t.split_once('='))
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .and_then(|(_, v)| aispice_core::units::parse(v))
    };
    find(rest).or_else(|| find(defaults))
}

/// The PDK MOS transistors at the top level of a netlist.
pub fn mosfets(netlist: &Netlist) -> Result<Vec<Mos>, String> {
    let mut out = Vec::new();
    for item in &netlist.items {
        let e = match item {
            Line::Element(e) if e.letter() == 'X' => e,
            _ => continue,
        };
        let Some(device) = e.first_word().and_then(aispice_core::pdk::device) else {
            continue;
        };
        if !device.is_mosfet() {
            continue;
        }
        let kind = match device.name {
            "sg13_lv_nmos" => Kind::Nmos,
            "sg13_lv_pmos" => Kind::Pmos,
            other => {
                return Err(format!(
                    "{} is {other}; the generators draw the low-voltage devices sg13_lv_nmos and sg13_lv_pmos",
                    e.name
                ));
            }
        };
        let rest = e.rest.split_once(char::is_whitespace).map_or("", |x| x.1);
        let w = param(rest, device.params, "w").unwrap_or(0.0);
        let l = param(rest, device.params, "l").unwrap_or(0.0);
        let m = param(rest, device.params, "m").unwrap_or(1.0);
        if m.fract() != 0.0 || m < 1.0 {
            return Err(format!("{} has m={m}; use a whole multiplier", e.name));
        }
        let to_nm = |v: f64, what: &str| {
            rules::um_to_nm(v * 1e6).ok_or_else(|| {
                format!(
                    "{} has {what}={}; layouts need sizes on the 5 nm grid",
                    e.name,
                    aispice_core::units::format(v)
                )
            })
        };
        let w_nm = to_nm(w * m, "w*m")?;
        let l_nm = to_nm(l, "l")?;
        let name = e
            .name
            .strip_prefix(['X', 'x'])
            .unwrap_or(&e.name)
            .to_string();
        out.push(Mos {
            element: e.name.clone(),
            kind,
            dev: RowDev {
                name,
                model: device.name,
                w: w_nm,
                l: l_nm,
                d: e.nodes[0].clone(),
                g: e.nodes[1].clone(),
                s: e.nodes[2].clone(),
                b: e.nodes[3].clone(),
            },
        });
    }
    if out.is_empty() {
        return Err(
            "the circuit has no sg13_lv_nmos or sg13_lv_pmos transistors to lay out".into(),
        );
    }
    Ok(out)
}

/// What a template makes of a circuit: rows bottom to top, the devices
/// drawn and the roles as resolved.
pub struct Block {
    pub rows: Vec<RowSpec>,
    pub elements: Vec<String>,
    pub roles: Roles,
}

fn lookup<'a>(mos: &'a [Mos], name: &str) -> Result<&'a Mos, String> {
    let bare = name.strip_prefix(['X', 'x']).unwrap_or(name);
    mos.iter()
        .find(|m| m.dev.name.eq_ignore_ascii_case(bare) || m.element.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            format!(
                "no transistor {name} in the circuit; its transistors are {}",
                mos.iter().map(Mos::short).collect::<Vec<_>>().join(", ")
            )
        })
}

fn role<'a>(mos: &'a [Mos], roles: &Roles, key: &str) -> Result<Option<Vec<&'a Mos>>, String> {
    roles
        .get(key)
        .map(|r| r.names().into_iter().map(|n| lookup(mos, n)).collect())
        .transpose()
}

fn need<'a>(
    mos: &'a [Mos],
    roles: &Roles,
    key: &str,
    count: Option<usize>,
) -> Result<Vec<&'a Mos>, String> {
    let found = role(mos, roles, key)?.ok_or_else(|| format!("the role map has no `{key}`"))?;
    if let Some(n) = count
        && found.len() != n
    {
        return Err(format!(
            "role `{key}` needs {n} device(s), got {}",
            found.len()
        ));
    }
    Ok(found)
}

fn same_kind(devs: &[&Mos], what: &str) -> Result<Kind, String> {
    let k = devs[0].kind;
    if let Some(d) = devs.iter().find(|d| d.kind != k) {
        return Err(format!(
            "{what}: {} and {} are not the same type",
            devs[0].short(),
            d.short()
        ));
    }
    Ok(k)
}

fn common_source(devs: &[&Mos], what: &str) -> Result<String, String> {
    let a = &devs[0].dev;
    for s in [&a.s, &a.d] {
        if devs.iter().all(|d| &d.dev.s == s || &d.dev.d == s) {
            return Ok(s.clone());
        }
    }
    Err(format!(
        "{what}: {} do not share a source net",
        devs.iter()
            .map(|d| d.short())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn common_bulk(devs: &[&Mos], what: &str) -> Result<String, String> {
    let b = &devs[0].dev.b;
    if let Some(d) = devs.iter().find(|d| &d.dev.b != b) {
        return Err(format!(
            "{what}: {} has bulk {} but {} has {}; a row sits in one well",
            devs[0].short(),
            b,
            d.short(),
            d.dev.b
        ));
    }
    Ok(b.clone())
}

fn row(name: &str, devs: &[&Mos]) -> Result<RowSpec, String> {
    let kind = same_kind(devs, name)?;
    let source = common_source(devs, name)?;
    let bulk = common_bulk(devs, name)?;
    Ok(RowSpec {
        name: name.to_string(),
        kind,
        islands: vec![Island {
            devices: devs.iter().map(|d| d.dev.clone()).collect(),
            source,
        }],
        off: bulk.clone(),
        bulk,
    })
}

fn names(devs: &[&Mos]) -> RoleDevices {
    if devs.len() == 1 {
        RoleDevices::One(devs[0].short().to_string())
    } else {
        RoleDevices::Many(devs.iter().map(|d| d.short().to_string()).collect())
    }
}

/// Fill in a role map from the topology when none was given.
fn auto_roles(template: Template, mos: &[Mos]) -> Result<Roles, String> {
    let mut r = Roles::new();
    let ask = |what: &str| {
        format!(
            "could not tell the {what} from the circuit; give a role map, e.g. {}",
            match template {
                Template::CurrentMirror => r#"{"ref": "M1", "out": "M2"}"#,
                Template::DiffPair => r#"{"pair": ["M1", "M2"]}"#,
                Template::Ota5t =>
                    r#"{"pair": ["M1", "M2"], "load": ["M3", "M4"], "tail": "M5", "bias": "M6"}"#,
                Template::Comparator =>
                    r#"{"tail": "M0", "pair": ["M1", "M2"], "latch_n": ["M3", "M4"], "latch_p": ["M5", "M6"], "reset": ["M7", "M8", "M9", "M10"]}"#,
            }
        )
    };
    // Two devices of one type and size sharing a source, gates apart.
    let pairs: Vec<(&Mos, &Mos)> = mos
        .iter()
        .enumerate()
        .flat_map(|(i, a)| mos[i + 1..].iter().map(move |b| (a, b)))
        .filter(|(a, b)| {
            a.kind == b.kind
                && a.dev.w == b.dev.w
                && a.dev.l == b.dev.l
                && a.dev.s == b.dev.s
                && a.dev.g != b.dev.g
                && !a.diode()
                && !b.diode()
                && a.dev.d != b.dev.d
        })
        .collect();
    match template {
        Template::CurrentMirror => {
            let refs: Vec<&Mos> = mos
                .iter()
                .filter(|m| m.dev.g == m.dev.d && mos.iter().any(|o| o != *m && o.dev.g == m.dev.g))
                .collect();
            let [rf] = refs.as_slice() else {
                return Err(ask("reference device"));
            };
            let outs: Vec<&Mos> = mos
                .iter()
                .filter(|o| *o != *rf && o.dev.g == rf.dev.g && o.kind == rf.kind)
                .collect();
            r.insert("ref".into(), names(&[rf]));
            r.insert("out".into(), names(&outs));
        }
        Template::DiffPair => {
            let [(a, b)] = pairs.as_slice() else {
                return Err(ask("pair"));
            };
            r.insert("pair".into(), names(&[a, b]));
        }
        Template::Ota5t => {
            let [(a, b)] = pairs.as_slice() else {
                return Err(ask("input pair"));
            };
            let tail: Vec<&Mos> = mos
                .iter()
                .filter(|m| m.kind == a.kind && (m.dev.d == a.dev.s) && *m != *a && *m != *b)
                .collect();
            let [tail] = tail.as_slice() else {
                return Err(ask("tail device"));
            };
            let load: Vec<&Mos> = mos
                .iter()
                .filter(|m| m.kind != a.kind && (m.dev.d == a.dev.d || m.dev.d == b.dev.d))
                .collect();
            if load.len() != 2 {
                return Err(ask("load"));
            }
            let bias: Vec<&Mos> = mos
                .iter()
                .filter(|m| {
                    *m != *tail
                        && m.kind == tail.kind
                        && m.dev.g == tail.dev.g
                        && m.dev.d == m.dev.g
                })
                .collect();
            r.insert("pair".into(), names(&[a, b]));
            r.insert("load".into(), names(&load));
            r.insert("tail".into(), names(&[tail]));
            if let [bias] = bias.as_slice() {
                r.insert("bias".into(), names(&[bias]));
            }
        }
        Template::Comparator => {
            // The input pair is the one whose source a third device of its
            // type drives: the clocked tail.
            let tailed: Vec<(&Mos, &Mos, &Mos)> = pairs
                .iter()
                .filter_map(|(a, b)| {
                    let t: Vec<&Mos> = mos
                        .iter()
                        .filter(|m| m.kind == a.kind && m.dev.d == a.dev.s && m != a && m != b)
                        .collect();
                    match t.as_slice() {
                        [t] => Some((*a, *b, *t)),
                        _ => None,
                    }
                })
                .collect();
            let [(a, b, tail)] = tailed.as_slice() else {
                return Err(ask("input pair and its clocked tail"));
            };
            let drains = [&a.dev.d, &b.dev.d];
            let latch_n: Vec<&Mos> = mos
                .iter()
                .filter(|m| m.kind == a.kind && drains.contains(&&m.dev.s))
                .collect();
            if latch_n.len() != 2 {
                return Err(ask("cross-coupled NMOS pair"));
            }
            let outs = [&latch_n[0].dev.d, &latch_n[1].dev.d];
            let latch_p: Vec<&Mos> = mos
                .iter()
                .filter(|m| {
                    m.kind != a.kind && outs.contains(&&m.dev.d) && outs.contains(&&m.dev.g)
                })
                .collect();
            let reset: Vec<&Mos> = mos
                .iter()
                .filter(|m| m.kind != a.kind && m.dev.g == tail.dev.g)
                .collect();
            r.insert("tail".into(), names(&[tail]));
            r.insert("pair".into(), names(&[a, b]));
            r.insert("latch_n".into(), names(&latch_n));
            r.insert("latch_p".into(), names(&latch_p));
            r.insert("reset".into(), names(&reset));
        }
    }
    Ok(r)
}

/// A row of islands, one per device, each on its own source net.
fn split_row(name: &str, devs: &[&Mos], sources: &[&String]) -> Result<RowSpec, String> {
    let kind = same_kind(devs, name)?;
    let bulk = common_bulk(devs, name)?;
    Ok(RowSpec {
        name: name.to_string(),
        kind,
        islands: devs
            .iter()
            .zip(sources)
            .map(|(d, s)| Island {
                devices: vec![d.dev.clone()],
                source: (*s).clone(),
            })
            .collect(),
        off: bulk.clone(),
        bulk,
    })
}

/// Whether two devices are cross-coupled: each one's gate is the other's
/// drain.
fn cross_coupled(a: &Mos, b: &Mos, src_a: &str, src_b: &str) -> bool {
    let (da, db) = (drain(a, src_a), drain(b, src_b));
    &a.dev.g == db && &b.dev.g == da
}

/// Check a circuit against a template and lay its devices into rows.
pub fn block(template: Template, mos: &[Mos], given: &Roles) -> Result<Block, String> {
    let roles = if given.is_empty() {
        auto_roles(template, mos)?
    } else {
        given.clone()
    };
    let expected: &[&str] = match template {
        Template::CurrentMirror => &["ref", "out"],
        Template::DiffPair => &["pair"],
        Template::Ota5t => &["pair", "load", "tail", "bias"],
        Template::Comparator => &["tail", "pair", "latch_n", "latch_p", "reset"],
    };
    if let Some(k) = roles.keys().find(|k| !expected.contains(&k.as_str())) {
        return Err(format!(
            "role `{k}` is not one of {}'s: {}",
            template.name(),
            expected.join(", ")
        ));
    }
    let mut used: Vec<&Mos> = Vec::new();
    let rows = match template {
        Template::CurrentMirror => {
            let rf = need(mos, &roles, "ref", Some(1))?[0];
            let outs = need(mos, &roles, "out", None)?;
            if rf.dev.g != rf.dev.d {
                return Err(format!(
                    "the reference {} is not diode-connected (gate {}, drain {})",
                    rf.short(),
                    rf.dev.g,
                    rf.dev.d
                ));
            }
            for o in &outs {
                if o.dev.g != rf.dev.g {
                    return Err(format!(
                        "{}'s gate is {}, not the reference's {}",
                        o.short(),
                        o.dev.g,
                        rf.dev.g
                    ));
                }
            }
            let mut devs = vec![rf];
            devs.extend(outs);
            used.extend(&devs);
            vec![row("mirror", &devs)?]
        }
        Template::DiffPair => {
            let pair = need(mos, &roles, "pair", Some(2))?;
            check_pair(&pair)?;
            used.extend(&pair);
            vec![row("pair", &pair)?]
        }
        Template::Ota5t => {
            let pair = need(mos, &roles, "pair", Some(2))?;
            let load = need(mos, &roles, "load", Some(2))?;
            let tail = need(mos, &roles, "tail", Some(1))?[0];
            let bias = role(mos, &roles, "bias")?.unwrap_or_default();
            if bias.len() > 1 {
                return Err("role `bias` takes one device".into());
            }
            let tail_net = check_pair(&pair)?;
            if pair[0].dev.w != pair[1].dev.w {
                return Err(format!(
                    "the pair {} and {} differ in width",
                    pair[0].short(),
                    pair[1].short()
                ));
            }
            if tail.kind != pair[0].kind {
                return Err(format!("the tail {} is not the pair's type", tail.short()));
            }
            if tail.dev.d != tail_net && tail.dev.s != tail_net {
                return Err(format!(
                    "the tail {} does not connect to the pair's source net {tail_net}",
                    tail.short()
                ));
            }
            if load[0].kind == pair[0].kind {
                return Err("the load must be the opposite type of the pair".into());
            }
            let load_src = common_source(&load, "load")?;
            if load[0].dev.g != load[1].dev.g {
                return Err(format!(
                    "the load {} and {} do not share a gate: it should be a current mirror",
                    load[0].short(),
                    load[1].short()
                ));
            }
            if !load
                .iter()
                .any(|l| l.dev.g == l.dev.d || l.dev.g == l.dev.s)
            {
                return Err("neither load device is diode-connected".into());
            }
            let pd: Vec<&String> = pair.iter().map(|p| drain(p, &tail_net)).collect();
            let ld: Vec<&String> = load.iter().map(|l| drain(l, &load_src)).collect();
            if !(pd.contains(&ld[0]) && pd.contains(&ld[1])) {
                return Err(format!(
                    "the load drains ({}, {}) are not the pair drains ({}, {})",
                    ld[0], ld[1], pd[0], pd[1]
                ));
            }
            let mut tail_row = vec![tail];
            if let Some(b) = bias.first() {
                if b.dev.g != tail.dev.g || b.dev.g != b.dev.d {
                    return Err(format!(
                        "the bias {} should be diode-connected on the tail's gate {}",
                        b.short(),
                        tail.dev.g
                    ));
                }
                tail_row.push(b);
            }
            used.extend(&pair);
            used.extend(&load);
            used.extend(&tail_row);
            let rows = vec![
                row("tail", &tail_row)?,
                row("pair", &pair)?,
                row("load", &load)?,
            ];
            // N rows sit below P rows, so the n-well is at the top.
            if pair[0].kind == Kind::Nmos {
                rows
            } else {
                rows.into_iter().rev().collect()
            }
        }
        Template::Comparator => {
            let tail = need(mos, &roles, "tail", Some(1))?[0];
            let pair = need(mos, &roles, "pair", Some(2))?;
            let latch_n = need(mos, &roles, "latch_n", Some(2))?;
            let latch_p = need(mos, &roles, "latch_p", Some(2))?;
            let reset = need(mos, &roles, "reset", None)?;
            let s = check_pair(&pair)?;
            if pair[0].kind != Kind::Nmos {
                return Err("the comparator generator takes an NMOS input pair".into());
            }
            if tail.kind != Kind::Nmos || (tail.dev.d != s && tail.dev.s != s) {
                return Err(format!(
                    "the tail {} should be an NMOS on the pair's source net {s}",
                    tail.short()
                ));
            }
            let pd: Vec<&String> = pair.iter().map(|p| drain(p, &s)).collect();
            // Each NMOS latch device sits on one input device's drain.
            let mut srcs: Vec<&String> = Vec::new();
            for m in &latch_n {
                let src = pd
                    .iter()
                    .find(|d| ***d == m.dev.s || ***d == m.dev.d)
                    .ok_or_else(|| {
                        format!(
                            "the NMOS latch device {} does not sit on a pair drain ({} or {})",
                            m.short(),
                            pd[0],
                            pd[1]
                        )
                    })?;
                srcs.push(src);
            }
            if srcs[0] == srcs[1] {
                return Err("the NMOS latch devices sit on the same pair drain".into());
            }
            if !cross_coupled(latch_n[0], latch_n[1], srcs[0], srcs[1]) {
                return Err(format!(
                    "{} and {} are not cross-coupled",
                    latch_n[0].short(),
                    latch_n[1].short()
                ));
            }
            let vdd = common_source(&latch_p, "latch_p")?;
            if latch_p.iter().any(|m| m.kind != Kind::Pmos)
                || !cross_coupled(latch_p[0], latch_p[1], &vdd, &vdd)
            {
                return Err(format!(
                    "{} and {} should be cross-coupled PMOS",
                    latch_p[0].short(),
                    latch_p[1].short()
                ));
            }
            for m in &reset {
                if m.dev.g != tail.dev.g || m.kind != Kind::Pmos {
                    return Err(format!(
                        "the reset device {} should be a PMOS clocked like the tail ({})",
                        m.short(),
                        tail.dev.g
                    ));
                }
            }
            common_source(&reset, "reset")?;
            used.push(tail);
            used.extend(&pair);
            used.extend(&latch_n);
            used.extend(&latch_p);
            used.extend(&reset);
            vec![
                row("tail", &[tail])?,
                row("pair", &pair)?,
                split_row("latch_n", &latch_n, &srcs)?,
                row("latch_p", &latch_p)?,
                row("reset", &reset)?,
            ]
        }
    };
    let mut seen = BTreeMap::new();
    for u in &used {
        if seen.insert(u.element.clone(), ()).is_some() {
            return Err(format!("{} has more than one role", u.short()));
        }
    }
    let n_bulk: Vec<&String> = used
        .iter()
        .filter(|m| m.kind == Kind::Nmos)
        .map(|m| &m.dev.b)
        .collect();
    if let Some(b) = n_bulk.iter().find(|b| **b != n_bulk[0]) {
        return Err(format!(
            "NMOS bulks differ ({} and {}); the substrate is one net",
            n_bulk[0], b
        ));
    }
    Ok(Block {
        rows,
        elements: used.iter().map(|m| m.element.clone()).collect(),
        roles,
    })
}

fn drain<'a>(m: &'a Mos, source: &str) -> &'a String {
    if m.dev.s == source {
        &m.dev.d
    } else {
        &m.dev.s
    }
}

/// A pair: one type and length, a shared source, separate gates and
/// drains. Returns the source net.
fn check_pair(pair: &[&Mos]) -> Result<String, String> {
    same_kind(pair, "pair")?;
    let src = common_source(pair, "pair")?;
    let (a, b) = (&pair[0].dev, &pair[1].dev);
    if a.g == b.g {
        return Err(format!(
            "the pair {} and {} share their gate {}",
            a.name, b.name, a.g
        ));
    }
    if a.l != b.l {
        return Err(format!(
            "the pair {} and {} differ in length",
            a.name, b.name
        ));
    }
    Ok(src)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aispice_core::netlist::parse;

    pub const OTA: &str = "* 5T OTA\nVDD vdd 0 1.2\nVP inp 0 0.6 AC 0.5\nVM inm 0 0.6 AC -0.5\nIB vdd vbn 20u\nCL out 0 1p\nXM1 x inp tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM2 out inm tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM3 x x vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM4 out x vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM5 tail vbn 0 0 sg13_lv_nmos w=4u l=1u ng=1 m=1\nXM6 vbn vbn 0 0 sg13_lv_nmos w=2u l=1u ng=1 m=1\n.ac dec 20 1 1G\n";

    #[test]
    fn the_ota_roles_are_found_and_checked() {
        let mos = mosfets(&parse(OTA)).unwrap();
        assert_eq!(mos.len(), 6);
        assert_eq!(mos[4].dev.w, 4000);
        let b = block(Template::Ota5t, &mos, &Roles::new()).unwrap();
        assert_eq!(
            b.roles["pair"],
            RoleDevices::Many(vec!["M1".into(), "M2".into()])
        );
        assert_eq!(b.roles["tail"], RoleDevices::One("M5".into()));
        assert_eq!(b.roles["bias"], RoleDevices::One("M6".into()));
        let names: Vec<&str> = b.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["tail", "pair", "load"]);
        assert_eq!(b.rows[1].islands[0].source, "tail");
        assert_eq!(b.rows[2].bulk, "vdd");
        assert_eq!(b.elements.len(), 6);
    }

    #[test]
    fn wrong_topologies_are_named() {
        let mos = mosfets(&parse(OTA)).unwrap();
        let mut roles = Roles::new();
        roles.insert(
            "pair".into(),
            RoleDevices::Many(vec!["M1".into(), "M3".into()]),
        );
        roles.insert(
            "load".into(),
            RoleDevices::Many(vec!["M2".into(), "M4".into()]),
        );
        roles.insert("tail".into(), RoleDevices::One("M5".into()));
        let e = block(Template::Ota5t, &mos, &roles).err().unwrap();
        assert!(e.contains("not the same type"), "{e}");
        let mut roles = Roles::new();
        roles.insert("ref".into(), RoleDevices::One("M4".into()));
        roles.insert("out".into(), RoleDevices::One("M3".into()));
        let e = block(Template::CurrentMirror, &mos, &roles).err().unwrap();
        assert!(e.contains("not diode-connected"), "{e}");
        let mut roles = Roles::new();
        roles.insert("pear".into(), RoleDevices::One("M4".into()));
        let e = block(Template::DiffPair, &mos, &roles).err().unwrap();
        assert!(e.contains("not one of diff_pair's"), "{e}");
        let e = lookup(&mos, "M9").err().unwrap();
        assert!(e.contains("M1, M2"), "{e}");
    }

    pub const STRONGARM: &str = "* StrongARM comparator\nVDD vdd 0 1.2\nVCLK clk 0 PULSE(0 1.2 1n 50p 50p 2n 4n)\nVP inp 0 0.61\nVM inm 0 0.59\nXM0 s clk 0 0 sg13_lv_nmos w=4u l=0.13u\nXM1 xp inp s 0 sg13_lv_nmos w=4u l=0.13u\nXM2 xn inm s 0 sg13_lv_nmos w=4u l=0.13u\nXM3 outn outp xp 0 sg13_lv_nmos w=2u l=0.13u\nXM4 outp outn xn 0 sg13_lv_nmos w=2u l=0.13u\nXM5 outn outp vdd vdd sg13_lv_pmos w=2u l=0.13u\nXM6 outp outn vdd vdd sg13_lv_pmos w=2u l=0.13u\nXM7 outn clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM8 outp clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM9 xp clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM10 xn clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nCP outp 0 5f\nCN outn 0 5f\n.tran 10p 8n\n";

    #[test]
    fn the_comparator_roles_are_found_and_split_into_islands() {
        let mos = mosfets(&parse(STRONGARM)).unwrap();
        let b = block(Template::Comparator, &mos, &Roles::new()).unwrap();
        assert_eq!(b.roles["tail"], RoleDevices::One("M0".into()));
        assert_eq!(
            b.roles["reset"],
            RoleDevices::Many(vec!["M7".into(), "M8".into(), "M9".into(), "M10".into()])
        );
        let names: Vec<&str> = b.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["tail", "pair", "latch_n", "latch_p", "reset"]);
        let sources: Vec<&str> = b.rows[2]
            .islands
            .iter()
            .map(|i| i.source.as_str())
            .collect();
        assert_eq!(sources, ["xp", "xn"]);
        assert_eq!(b.elements.len(), 11);
        let mut roles = b.roles.clone();
        roles.insert(
            "latch_p".into(),
            RoleDevices::Many(vec!["M5".into(), "M7".into()]),
        );
        roles.insert(
            "reset".into(),
            RoleDevices::Many(vec!["M6".into(), "M8".into()]),
        );
        let e = block(Template::Comparator, &mos, &roles).err().unwrap();
        assert!(e.contains("cross-coupled"), "{e}");
    }

    #[test]
    fn sizes_off_the_grid_are_refused() {
        let n = parse("t\nXM1 a b 0 0 sg13_lv_nmos w=1.2345u l=0.13u\n.op\n");
        assert!(mosfets(&n).unwrap_err().contains("5 nm grid"));
        let n = parse("t\nXM1 a b 0 0 sg13_hv_nmos w=1u l=0.45u\n.op\n");
        assert!(mosfets(&n).unwrap_err().contains("low-voltage"));
    }
}

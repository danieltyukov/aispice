//! Design rule checks with the PDK's own KLayout decks.
//!
//! IHP's `run_drc.py` only gathers `-rd` switches and calls KLayout through
//! a shell; aispice calls KLayout directly with the same switches, one deck
//! at a time (the main rule set, the extra rules, antenna, density), and
//! reads each deck's report database.

use crate::tools::{self, LayoutError, PdkLayoutFiles, Programs, RunDir};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Report databases larger than this are not read.
pub const MAX_REPORT_BYTES: u64 = 64 << 20;
/// Markers kept per rule; the rest are counted.
pub const MAX_PER_RULE: usize = 1000;

/// Which decks to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrcChecks {
    /// The extra rules outside IHP's main set.
    pub extra: bool,
    pub antenna: bool,
    /// Metal and active density: a chip-level rule, off for blocks.
    pub density: bool,
    /// IHP's recommended rules (names ending in `R`), on by default. A
    /// chip leaves them out: its IO cells with bond pads abutting them, as
    /// IHP and Croc place them, break the recommended pad rules.
    pub recommended: bool,
}

impl Default for DrcChecks {
    fn default() -> Self {
        Self {
            extra: true,
            antenna: true,
            density: false,
            recommended: true,
        }
    }
}

/// One rule broken at one place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Violation {
    pub rule: String,
    pub description: String,
    /// The layer the rule is about, from the rule's name.
    pub layer: Option<String>,
    pub cell: String,
    /// Bounding box of the marker, um: left, bottom, right, top.
    pub bbox: [f64; 4],
}

/// How often a rule failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleCount {
    pub rule: String,
    pub description: String,
    pub layer: Option<String>,
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DrcReport {
    pub cell: String,
    /// The decks that ran.
    pub decks: Vec<String>,
    pub rules: Vec<RuleCount>,
    /// Markers, at most [`MAX_PER_RULE`] per rule.
    pub violations: Vec<Violation>,
    /// Report databases kept, one per deck.
    pub reports: Vec<PathBuf>,
}

impl DrcReport {
    pub fn clean(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn total(&self) -> usize {
        self.rules.iter().map(|r| r.count).sum()
    }

    /// The report in words: decks, then each rule with its count and where
    /// its first markers are.
    pub fn to_text(&self) -> String {
        if self.clean() {
            return format!(
                "DRC clean: cell {}, decks {}.\n",
                self.cell,
                self.decks.join(", ")
            );
        }
        let mut t = format!(
            "DRC: {} violation(s) of {} rule(s) in cell {} (decks {}).\n",
            self.total(),
            self.rules.len(),
            self.cell,
            self.decks.join(", ")
        );
        for r in &self.rules {
            t.push_str(&format!(
                "  {} ({}x{}): {}\n",
                r.rule,
                r.count,
                r.layer
                    .as_deref()
                    .map(|l| format!(", {l}"))
                    .unwrap_or_default(),
                r.description
            ));
            for v in self.violations.iter().filter(|v| v.rule == r.rule).take(3) {
                t.push_str(&format!(
                    "    at ({:.3}, {:.3}) to ({:.3}, {:.3}) um in {}\n",
                    v.bbox[0], v.bbox[1], v.bbox[2], v.bbox[3], v.cell
                ));
            }
        }
        t
    }
}

/// The layer a rule is about, from IHP's rule naming (`M1.b`, `TV1.a`,
/// `Gat.a`, `NW.b1`, ...).
pub fn layer_of(rule: &str) -> Option<String> {
    let prefix = rule.split('.').next()?;
    let name = match prefix {
        "NW" => "NWell",
        "PWB" => "PWell:block",
        "NBL" => "nBuLay",
        "Act" => "Activ",
        "AFil" => "Activ:filler",
        "TGO" => "ThickGateOx",
        "Gat" => "GatPoly",
        "GFil" => "GatPoly:filler",
        "pSD" => "pSD",
        "nSD" => "nSD",
        "Sal" => "SalBlock",
        "Cnt" => "Cont",
        "CntB" => "ContBar",
        "M1" => "Metal1",
        "M2" => "Metal2",
        "M3" => "Metal3",
        "M4" => "Metal4",
        "M5" => "Metal5",
        "V1" => "Via1",
        "V2" => "Via2",
        "V3" => "Via3",
        "V4" => "Via4",
        "TV1" => "TopVia1",
        "TV2" => "TopVia2",
        "TM1" => "TopMetal1",
        "TM2" => "TopMetal2",
        "MIM" => "MIM",
        "Mim" => "MIM",
        "Rhi" => "rhigh",
        "Rppd" => "rppd",
        "Rsil" => "rsil",
        "LU" => "latch-up (taps)",
        "Pas" => "Passiv",
        "Ant" => "antenna",
        _ => return None,
    };
    Some(name.to_string())
}

/// Elements in a report database nest at most this deep; KLayout writes
/// five levels.
const MAX_XML_DEPTH: usize = 64;

/// How deep elements nest, counted from the tags without parsing, so a
/// hostile file is refused before the XML reader recurses into it.
fn xml_depth(text: &str) -> usize {
    let b = text.as_bytes();
    let (mut depth, mut max) = (0usize, 0usize);
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        match b.get(i + 1) {
            Some(b'/') => depth = depth.saturating_sub(1),
            Some(b'?') | Some(b'!') | None => {}
            Some(_) => {
                let end = b[i..].iter().position(|c| *c == b'>').map(|p| i + p);
                if !matches!(end, Some(e) if e > i && b[e - 1] == b'/') {
                    depth += 1;
                    max = max.max(depth);
                }
            }
        }
        i += 1;
    }
    max
}

/// Parse a KLayout report database.
pub fn parse_lyrdb(text: &str) -> Result<(String, Vec<RuleCount>, Vec<Violation>), String> {
    if xml_depth(text) > MAX_XML_DEPTH {
        return Err(format!(
            "the report nests deeper than {MAX_XML_DEPTH} levels"
        ));
    }
    let doc = roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 50_000_000,
            ..Default::default()
        },
    )
    .map_err(|e| format!("the report is not valid XML: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "report-database" {
        return Err("not a KLayout report database".into());
    }
    let top = child_text(root, "top-cell").unwrap_or_default();
    let mut descriptions: BTreeMap<String, String> = BTreeMap::new();
    for cat in root.descendants().filter(|n| n.has_tag_name("category")) {
        if let Some(name) = child_text(cat, "name") {
            descriptions.insert(name, child_text(cat, "description").unwrap_or_default());
        }
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut violations = Vec::new();
    if let Some(items) = root.children().find(|n| n.has_tag_name("items")) {
        for item in items.children().filter(|n| n.has_tag_name("item")) {
            let rule = child_text(item, "category")
                .unwrap_or_default()
                .trim_matches('\'')
                .to_string();
            // Nested categories are written as `'parent'.'child'`.
            let rule = rule.rsplit("'.'").next().unwrap_or(&rule).to_string();
            let n = counts.entry(rule.clone()).or_default();
            *n += 1;
            if *n > MAX_PER_RULE {
                continue;
            }
            let cell = child_text(item, "cell").unwrap_or_else(|| top.clone());
            let mut bbox: Option<[f64; 4]> = None;
            if let Some(values) = item.children().find(|n| n.has_tag_name("values")) {
                for v in values.children().filter(|n| n.has_tag_name("value")) {
                    if let Some(b) = value_bbox(v.text().unwrap_or("")) {
                        bbox = Some(match bbox {
                            None => b,
                            Some(a) => [
                                a[0].min(b[0]),
                                a[1].min(b[1]),
                                a[2].max(b[2]),
                                a[3].max(b[3]),
                            ],
                        });
                    }
                }
            }
            violations.push(Violation {
                description: descriptions.get(&rule).cloned().unwrap_or_default(),
                layer: layer_of(&rule),
                rule,
                cell,
                bbox: bbox.unwrap_or([0.0; 4]),
            });
        }
    }
    let rules = counts
        .into_iter()
        .map(|(rule, count)| RuleCount {
            description: descriptions.get(&rule).cloned().unwrap_or_default(),
            layer: layer_of(&rule),
            rule,
            count,
        })
        .collect();
    Ok((top, rules, violations))
}

fn child_text(node: roxmltree::Node, tag: &str) -> Option<String> {
    node.children()
        .find(|n| n.has_tag_name(tag))
        .and_then(|n| n.text())
        .map(|t| t.trim().to_string())
}

/// The bounding box of a marker value: `polygon: (x,y;x,y;...)`,
/// `box: (l,b;r,t)`, `edge: (x,y;x,y)`, `edge-pair: (...)|(...)` or
/// `(...)/(...)`, `path: (...) w=...`. Values that are not geometry give
/// `None`.
pub fn value_bbox(value: &str) -> Option<[f64; 4]> {
    let (kind, rest) = value.split_once(':')?;
    if !matches!(
        kind.trim(),
        "polygon" | "box" | "edge" | "edge-pair" | "path" | "simple-polygon"
    ) {
        return None;
    }
    let mut bbox: Option<[f64; 4]> = None;
    for group in rest.split('(').skip(1) {
        let inner = group.split(')').next().unwrap_or("");
        for point in inner.split(';') {
            let mut xy = point.split(',').map(|t| t.trim().parse::<f64>());
            if let (Some(Ok(x)), Some(Ok(y))) = (xy.next(), xy.next()) {
                bbox = Some(match bbox {
                    None => [x, y, x, y],
                    Some(b) => [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)],
                });
            }
        }
    }
    bbox
}

/// The DRC decks of the PDK and what each run is called.
fn decks(files: &PdkLayoutFiles, checks: DrcChecks) -> Vec<(&'static str, PathBuf, &'static str)> {
    let mut out = vec![("main", files.drc_deck.clone(), "main")];
    let rule_decks = files.drc_dir.join("rule_decks");
    if checks.extra {
        out.push((
            "extra",
            rule_decks.join("sg13g2_maximal.drc"),
            "sg13g2_maximal",
        ));
    }
    if checks.antenna {
        out.push(("antenna", rule_decks.join("antenna.drc"), "antenna"));
    }
    if checks.density {
        out.push(("density", rule_decks.join("density.drc"), "density"));
    }
    out
}

/// The `-rd` switches IHP's `run_drc.py` passes, with the report and log of
/// one deck.
fn switches(
    files: &PdkLayoutFiles,
    gds: &Path,
    cell: &str,
    checks: DrcChecks,
    table: &str,
    report: &Path,
    log: &Path,
) -> Vec<(&'static str, String)> {
    let json = if files.drc_json.is_file() {
        &files.drc_json
    } else {
        &files.drc_json_default
    };
    let f = |b: bool| if b { "true" } else { "false" }.to_string();
    vec![
        ("threads", "4".into()),
        (
            "drc_json_default",
            files.drc_json_default.display().to_string(),
        ),
        ("drc_json", json.display().to_string()),
        ("run_mode", "deep".into()),
        ("precheck_drc", f(false)),
        ("disable_extra_rules", f(!checks.extra)),
        ("no_feol", f(false)),
        ("no_beol", f(false)),
        ("no_offgrid", f(false)),
        ("no_angle", f(false)),
        ("density_sanity", f(false)),
        ("density", f(checks.density)),
        ("no_forbidden", f(false)),
        ("no_pin", f(false)),
        ("no_recommended", f(!checks.recommended)),
        ("topcell", cell.to_string()),
        ("input", gds.display().to_string()),
        ("report", report.display().to_string()),
        ("log", log.display().to_string()),
        ("tables", table.to_string()),
    ]
}

/// Run DRC on a cell of a GDS file. Reports and logs are kept in `out_dir`.
pub async fn run(
    programs: &Programs,
    files: &PdkLayoutFiles,
    gds: &Path,
    cell: &str,
    checks: DrcChecks,
    out_dir: &Path,
    cancel: &CancellationToken,
) -> Result<DrcReport, LayoutError> {
    tools::check_path(gds)?;
    tools::check_path(out_dir)?;
    tools::check_name("cell", cell)?;
    std::fs::create_dir_all(out_dir).map_err(|e| LayoutError::Io(e.to_string()))?;
    let run = RunDir::new()?;
    let access = tools::Access::new()
        .read(&files.root)
        .read(gds)
        .write(out_dir);
    let jobs = decks(files, checks);
    let futures = jobs.iter().map(|(name, deck, table)| {
        let report = out_dir.join(format!("{name}.lyrdb"));
        let log = out_dir.join(format!("{name}.log"));
        let vars = switches(files, gds, cell, checks, table, &report, &log);
        let dir = run.path().to_path_buf();
        let access = &access;
        async move {
            let out = tools::klayout(programs, &dir, deck, &vars, access, cancel).await?;
            if !out.success() || !report.is_file() {
                return Err(LayoutError::Failed {
                    program: format!("KLayout DRC ({name} deck)"),
                    message: tools::tail(&out.combined(), 8),
                });
            }
            Ok::<PathBuf, LayoutError>(report)
        }
    });
    let reports: Vec<PathBuf> = futures::future::join_all(futures)
        .await
        .into_iter()
        .collect::<Result<_, _>>()?;
    let mut report = DrcReport {
        cell: cell.to_string(),
        decks: jobs.iter().map(|(n, _, _)| n.to_string()).collect(),
        reports: reports.clone(),
        ..Default::default()
    };
    let mut counts: BTreeMap<String, RuleCount> = BTreeMap::new();
    for path in &reports {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if size > MAX_REPORT_BYTES {
            return Err(LayoutError::Input(format!(
                "{} is larger than {} MiB",
                path.display(),
                MAX_REPORT_BYTES >> 20
            )));
        }
        let text = std::fs::read_to_string(path).map_err(|e| LayoutError::Io(e.to_string()))?;
        let (_, rules, violations) = parse_lyrdb(&text).map_err(LayoutError::Input)?;
        for r in rules {
            counts
                .entry(r.rule.clone())
                .and_modify(|c| c.count += r.count)
                .or_insert(r);
        }
        report.violations.extend(violations);
    }
    report.rules = counts.into_values().collect();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = r#"<?xml version='1.0' encoding='utf-8'?>
<report-database>
 <description>Main DRC Run Report</description>
 <top-cell>TOP</top-cell>
 <categories>
  <category><name>M1.b</name><description>5.16. M1.b: Min. Metal1 space or notch: 0.18um.</description><categories/></category>
  <category><name>LU.b</name><description>7.2. LU.b : Max. space to a tie is 20.0 um</description><categories/></category>
  <category><name>Gat.a</name><description>Min. GatPoly width</description><categories/></category>
 </categories>
 <cells><cell><name>TOP</name></cell></cells>
 <items>
  <item><category>'M1.b'</category><cell>TOP</cell><values><value>edge-pair: (1,2;1,3)|(1.1,3;1.1,2)</value></values></item>
  <item><category>'M1.b'</category><cell>TOP</cell><values><value>edge-pair: (5,0;5,1)/(5.1,1;5.1,0)</value></values></item>
  <item><category>'LU.b'</category><cell>nmos</cell><values><value>polygon: (0,0;0,1;0.81,1;0.81,0)</value></values></item>
 </items>
</report-database>"#;

    #[test]
    fn reports_give_rules_layers_and_places() {
        let (top, rules, v) = parse_lyrdb(REPORT).unwrap();
        assert_eq!(top, "TOP");
        assert_eq!(rules.len(), 2, "rules without markers are not listed");
        assert_eq!((rules[1].rule.as_str(), rules[1].count), ("M1.b", 2));
        assert_eq!(rules[1].layer.as_deref(), Some("Metal1"));
        assert!(rules[1].description.contains("0.18"));
        assert_eq!(v[0].bbox, [1.0, 2.0, 1.1, 3.0]);
        assert_eq!(v[1].bbox, [5.0, 0.0, 5.1, 1.0]);
        assert_eq!(v[2].cell, "nmos");
        assert_eq!(v[2].bbox, [0.0, 0.0, 0.81, 1.0]);
        let report = DrcReport {
            cell: "TOP".into(),
            decks: vec!["main".into()],
            rules,
            violations: v,
            reports: vec![],
        };
        let text = report.to_text();
        assert!(text.contains("3 violation(s) of 2 rule(s)"), "{text}");
        assert!(text.contains("M1.b (2x, Metal1)"), "{text}");
        assert!(
            text.contains("at (1.000, 2.000) to (1.100, 3.000) um in TOP"),
            "{text}"
        );
    }

    #[test]
    fn layers_follow_rule_names() {
        assert_eq!(layer_of("TV1.a").as_deref(), Some("TopVia1"));
        assert_eq!(layer_of("NW.b1").as_deref(), Some("NWell"));
        assert_eq!(layer_of("Cnt.e").as_deref(), Some("Cont"));
        assert_eq!(layer_of("XYZ.q"), None);
    }

    #[test]
    fn values_without_geometry_have_no_box() {
        assert_eq!(value_bbox("text: 'hello'"), None);
        assert_eq!(value_bbox("float: 1.5"), None);
        assert_eq!(value_bbox("box: (0,0;2,3)"), Some([0.0, 0.0, 2.0, 3.0]));
    }

    #[test]
    fn markers_are_capped_per_rule_but_counted() {
        let mut xml = String::from(
            "<report-database><top-cell>T</top-cell><categories><category><name>M1.a</name></category></categories><items>",
        );
        for i in 0..(MAX_PER_RULE + 5) {
            xml.push_str(&format!("<item><category>'M1.a'</category><cell>T</cell><values><value>box: ({i},0;{i},1)</value></values></item>"));
        }
        xml.push_str("</items></report-database>");
        let (_, rules, v) = parse_lyrdb(&xml).unwrap();
        assert_eq!(rules[0].count, MAX_PER_RULE + 5);
        assert_eq!(v.len(), MAX_PER_RULE);
        assert!(parse_lyrdb("<html/>").is_err());
        assert_eq!(xml_depth("<a><b/><c>x</c></a>"), 2);
        let deep = format!("{}{}", "<a>".repeat(100), "</a>".repeat(100));
        assert!(parse_lyrdb(&deep).unwrap_err().contains("deeper"));
        assert!(parse_lyrdb("<!DOCTYPE x [<!ENTITY a 'b'>]><report-database/>").is_err());
    }
}

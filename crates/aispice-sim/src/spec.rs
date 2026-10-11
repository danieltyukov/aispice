//! Spec tables: measurements with limits, evaluated to pass or fail.
//!
//! A spec is how a design goal becomes checkable: "gain at 1 kHz at least
//! 20 dB" is a [`Measure`] plus a lower limit. [`evaluate`] measures every
//! spec on a run and reports a margin per row, normalised so the optimizer
//! and the agent can compare a decibel spec with a frequency spec: a margin
//! of 0.1 means passing by 10% of the limit, -0.1 failing by 10%.
//!
//! Specs have a one-line text form, used in `<circuit>.specs` files and chat:
//!
//! ```text
//! gain = gain_db_at(V(out)/V(in), 1k) >= 20
//! bw   = bandwidth_3db(V(out)) in 1Meg..2Meg
//! pm   = phase_margin(V(out)) >= 45
//! vout = avg(V(out), 1m, 2m) = 3.3 +- 2%
//! os   = overshoot_pct(V(out)) <= 10 target 5
//! ```
//!
//! `>=` and `<=` set one limit, `in a..b` both, `= t` a target, `= t +- d`
//! (or `+- p%`) a target with symmetric limits. A trailing word is a display
//! unit. Comparison numbers may carry units (`20dB`, `1MHz`, `10us`); they
//! are ignored, except that `MHz` means megahertz.

use crate::dataset::Dataset;
use crate::expr::{format_number, parse_number};
use crate::measure::{self, Measure, MeasureResult, human};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Spec {
    /// Short name for reports: `gain`, `bw`, `pm`.
    pub name: String,
    pub measure: Measure,
    /// Lowest passing value.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "measure::lenient::opt"
    )]
    pub min: Option<f64>,
    /// Highest passing value.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "measure::lenient::opt"
    )]
    pub max: Option<f64>,
    /// The ideal value. Alone it is informational (the optimizer pulls
    /// toward it); with limits it is shown alongside them.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "measure::lenient::opt"
    )]
    pub target: Option<f64>,
    /// Unit to show values in, when the measurement's own is not wanted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SpecRow {
    pub name: String,
    /// The measured value; for a stepped run, the worst step's.
    pub value: Option<f64>,
    pub unit: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub target: Option<f64>,
    pub pass: bool,
    /// Distance to the nearest limit over that limit's magnitude: positive
    /// passes by that fraction, negative fails by it. `None` without limits
    /// or without a value.
    pub margin: Option<f64>,
    /// For a stepped run, the label of the step the row reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worst_step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// One line for people: `pm: 52.3° (>= 45°) pass, margin +16%`.
    pub display: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SpecReport {
    pub rows: Vec<SpecRow>,
    pub all_pass: bool,
    /// `all 3 specs pass`, or which ones fail and by how much.
    pub summary: String,
}

impl SpecReport {
    pub fn row(&self, name: &str) -> Option<&SpecRow> {
        self.rows.iter().find(|r| r.name == name)
    }

    /// The smallest margin of any row with limits, `None` if a limited row
    /// has no value. Useful as a single "how close to failing" number.
    pub fn worst_margin(&self) -> Option<f64> {
        let mut worst = f64::INFINITY;
        for r in &self.rows {
            if r.min.is_none() && r.max.is_none() {
                continue;
            }
            worst = worst.min(r.margin?);
        }
        Some(worst)
    }
}

impl fmt::Display for SpecReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for r in &self.rows {
            writeln!(f, "{}", r.display)?;
        }
        write!(f, "{}", self.summary)
    }
}

/// Measure every spec on the results of a run.
pub fn evaluate(specs: &[Spec], datasets: &[Dataset]) -> SpecReport {
    let rows: Vec<SpecRow> = specs
        .iter()
        .map(|s| row(s, &measure::measure(&s.name, &s.measure, datasets)))
        .collect();
    let all_pass = rows.iter().all(|r| r.pass);
    let summary = summarize(&rows);
    SpecReport {
        rows,
        all_pass,
        summary,
    }
}

/// Normalised margin of `v` against the limits.
pub fn margin(v: f64, min: Option<f64>, max: Option<f64>) -> Option<f64> {
    let scale = |limit: f64, other: Option<f64>| {
        if limit.abs() > 1e-300 {
            limit.abs()
        } else {
            other
                .filter(|o| o.abs() > 1e-300)
                .map(f64::abs)
                .unwrap_or(1.0)
        }
    };
    let lo = min.map(|m| (v - m) / scale(m, max));
    let hi = max.map(|m| (m - v) / scale(m, min));
    match (lo, hi) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn passes(v: f64, min: Option<f64>, max: Option<f64>) -> bool {
    v.is_finite() && min.is_none_or(|m| v >= m) && max.is_none_or(|m| v <= m)
}

fn row(spec: &Spec, result: &MeasureResult) -> SpecRow {
    let unit = spec
        .unit
        .clone()
        .filter(|u| !u.is_empty())
        .or_else(|| (!result.unit.is_empty()).then(|| result.unit.clone()))
        .or_else(|| spec.measure.unit_hint().map(str::to_string))
        .unwrap_or_default();
    let limited = spec.min.is_some() || spec.max.is_some();
    let stepped = result.per_step.len() > 1;

    let mut value = result.value;
    let mut worst_step = None;
    let mut note = result.note.clone();
    let mut pass;

    if result.per_step.is_empty() {
        pass = false;
    } else if let Some(missing) = result.per_step.iter().find(|s| s.value.is_none()) {
        pass = false;
        if stepped {
            value = None;
            worst_step = Some(missing.label.clone());
            note = Some(format!(
                "step {}: {}",
                missing.label,
                missing.note.as_deref().unwrap_or("no value")
            ));
        }
    } else {
        // Every step has a value: report the one closest to failing.
        let worst = result
            .per_step
            .iter()
            .min_by(|a, b| {
                let ma = a
                    .value
                    .and_then(|v| margin(v, spec.min, spec.max))
                    .unwrap_or(0.0);
                let mb = b
                    .value
                    .and_then(|v| margin(v, spec.min, spec.max))
                    .unwrap_or(0.0);
                ma.total_cmp(&mb)
            })
            .filter(|_| limited);
        if stepped {
            let chosen = worst.unwrap_or(&result.per_step[0]);
            value = chosen.value;
            worst_step = Some(chosen.label.clone());
        }
        pass = result
            .per_step
            .iter()
            .all(|s| s.value.is_some_and(|v| passes(v, spec.min, spec.max)));
    }
    let m = value.and_then(|v| margin(v, spec.min, spec.max));
    if value.is_none() {
        pass = false;
    }
    let mut r = SpecRow {
        name: spec.name.clone(),
        value,
        unit,
        min: spec.min,
        max: spec.max,
        target: spec.target,
        pass,
        margin: m,
        worst_step,
        note,
        display: String::new(),
    };
    r.display = row_display(&r, stepped);
    r
}

fn limits_text(r: &SpecRow) -> Option<String> {
    let h = |v: f64| human(v, &r.unit);
    let mut s = match (r.min, r.max) {
        (Some(a), Some(b)) => format!("{}..{}", h(a), h(b)),
        (Some(a), None) => format!(">= {}", h(a)),
        (None, Some(b)) => format!("<= {}", h(b)),
        (None, None) => String::new(),
    };
    if let Some(t) = r.target {
        if !s.is_empty() {
            s.push_str(", ");
        }
        s.push_str(&format!("target {}", h(t)));
    }
    (!s.is_empty()).then_some(s)
}

pub(crate) fn row_display(r: &SpecRow, stepped: bool) -> String {
    let limited = r.min.is_some() || r.max.is_some();
    let mut s = format!("{}: ", r.name);
    match r.value {
        Some(v) => s.push_str(&human(v, &r.unit)),
        None => s.push_str(&format!(
            "n/a ({})",
            r.note.as_deref().unwrap_or("no value")
        )),
    }
    if stepped
        && r.value.is_some()
        && let Some(step) = &r.worst_step
    {
        s.push_str(&format!(" worst at {step}"));
    }
    if let Some(l) = limits_text(r) {
        s.push_str(&format!(" ({l})"));
    }
    if limited || r.value.is_none() {
        s.push_str(if r.pass { " pass" } else { " FAIL" });
    }
    if let Some(m) = r.margin {
        s.push_str(&format!(", margin {:+.1}%", m * 100.0));
    }
    s
}

pub(crate) fn summarize(rows: &[SpecRow]) -> String {
    let failing: Vec<&SpecRow> = rows.iter().filter(|r| !r.pass).collect();
    if failing.is_empty() {
        return match rows.len() {
            0 => "no specs".into(),
            1 => "the spec passes".into(),
            n => format!("all {n} specs pass"),
        };
    }
    let reasons: Vec<String> = failing
        .iter()
        .map(|r| match r.value {
            None => format!("{} (no value)", r.name),
            Some(v) => {
                let h = |x: f64| human(x, &r.unit);
                match (r.min, r.max) {
                    (Some(lo), _) if v < lo => format!("{} ({} < {})", r.name, h(v), h(lo)),
                    (_, Some(hi)) if v > hi => format!("{} ({} > {})", r.name, h(v), h(hi)),
                    _ => format!("{} ({})", r.name, h(v)),
                }
            }
        })
        .collect();
    format!(
        "{} of {} specs pass; failing: {}",
        rows.len() - failing.len(),
        rows.len(),
        reasons.join(", ")
    )
}

// The text form.

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{}{message}", line.map(|l| format!("line {l}: ")).unwrap_or_default())]
pub struct SpecParseError {
    /// 1-based line, when parsing several lines.
    pub line: Option<usize>,
    pub message: String,
}

fn err(message: impl Into<String>) -> SpecParseError {
    SpecParseError {
        line: None,
        message: message.into(),
    }
}

impl Spec {
    /// Parse one line such as `pm = phase_margin(V(out)) >= 45`.
    pub fn parse(text: &str) -> Result<Spec, SpecParseError> {
        let (name, rest) = text.split_once('=').ok_or_else(|| {
            err(format!(
                "expected `name = measurement`, found `{}`",
                text.trim()
            ))
        })?;
        let name = name.trim();
        let valid = name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'));
        if !valid {
            return Err(err(format!("`{name}` is not a valid spec name")));
        }
        let (kind, inner, used) = measure::split_call(rest).map_err(|e| err(e.message))?;
        let measure = measure::build_measure(&kind, &inner).map_err(|e| err(e.message))?;
        let mut spec = Spec {
            name: name.to_string(),
            measure,
            min: None,
            max: None,
            target: None,
            unit: None,
        };
        parse_clause(&rest[used..], &mut spec)?;
        Ok(spec)
    }
}

impl std::str::FromStr for Spec {
    type Err = SpecParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Spec::parse(s)
    }
}

/// Parse several specs, one per line. Blank lines and lines starting with
/// `#` are skipped; names must be unique.
pub fn parse_specs(text: &str) -> Result<Vec<Spec>, SpecParseError> {
    let mut out: Vec<Spec> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let spec = Spec::parse(t).map_err(|e| SpecParseError {
            line: Some(i + 1),
            message: e.message,
        })?;
        if out.iter().any(|s| s.name == spec.name) {
            return Err(SpecParseError {
                line: Some(i + 1),
                message: format!("spec `{}` is defined twice", spec.name),
            });
        }
        out.push(spec);
    }
    Ok(out)
}

fn plain_numbers(m: &Measure) -> bool {
    matches!(m.unit_hint(), Some("dB" | "%" | "°"))
}

/// A limit number: SPICE suffixes and attached units allowed (`20dB`,
/// `1MHz`), a trailing `%` ignored.
fn limit_number(tok: &str) -> Result<f64, SpecParseError> {
    let t = tok.trim().trim_end_matches('%');
    parse_number(t).ok_or_else(|| err(format!("`{tok}` is not a number")))
}

fn parse_clause(text: &str, spec: &mut Spec) -> Result<(), SpecParseError> {
    let mut s = text.trim();
    if s.is_empty() {
        return Ok(());
    }
    // Comparison: operators may touch the number (`>=20`).
    let ops: [(&str, u8); 7] = [
        (">=", b'>'),
        ("=>", b'>'),
        ("<=", b'<'),
        ("=<", b'<'),
        ("==", b'='),
        (">", b'>'),
        ("<", b'<'),
    ];
    let mut handled = false;
    for (op, kind) in ops {
        if let Some(r) = s.strip_prefix(op) {
            let (tok, rest) = next_token(r);
            let v = limit_number(tok)?;
            match kind {
                b'>' => spec.min = Some(v),
                b'<' => spec.max = Some(v),
                _ => {
                    s = rest;
                    target_with_tolerance(v, &mut s, spec)?;
                    handled = true;
                    break;
                }
            }
            s = rest;
            handled = true;
            break;
        }
    }
    if !handled {
        if let Some(r) = s.strip_prefix('=') {
            let (tok, rest) = next_token(r);
            let v = limit_number(tok)?;
            s = rest;
            target_with_tolerance(v, &mut s, spec)?;
        } else if s.len() >= 3
            && s[..2].eq_ignore_ascii_case("in")
            && s[2..].starts_with(char::is_whitespace)
        {
            let (tok, rest) = next_token(&s[2..]);
            let (a, b) = tok
                .split_once("..")
                .ok_or_else(|| err(format!("expected a range like `1k..2k`, found `{tok}`")))?;
            let (a, b) = (limit_number(a)?, limit_number(b)?);
            if a > b {
                return Err(err(format!(
                    "the range {tok} is empty (its start is above its end)"
                )));
            }
            spec.min = Some(a);
            spec.max = Some(b);
            s = rest;
        } else if !s.starts_with("target") {
            return Err(err(format!(
                "expected `>=`, `<=`, `in a..b` or `= value` after the measurement, found `{s}`"
            )));
        }
    }
    // Optional target and unit.
    let (tok, rest) = next_token(s);
    if tok.eq_ignore_ascii_case("target") {
        let (num, rest) = next_token(rest);
        if spec.target.is_some() {
            return Err(err("the target is given twice"));
        }
        spec.target = Some(limit_number(num)?);
        s = rest;
    }
    let (tok, rest) = next_token(s);
    if !tok.is_empty() {
        spec.unit = Some(tok.to_string());
        if !rest.trim().is_empty() {
            return Err(err(format!("unexpected `{}`", rest.trim())));
        }
    }
    Ok(())
}

fn target_with_tolerance(t: f64, s: &mut &str, spec: &mut Spec) -> Result<(), SpecParseError> {
    spec.target = Some(t);
    let rest = s.trim_start();
    let after = rest
        .strip_prefix("+-")
        .or_else(|| rest.strip_prefix("±"))
        .or_else(|| rest.strip_prefix("+/-"));
    if let Some(after) = after {
        let (tok, rest) = next_token(after);
        let d = if let Some(p) = tok.strip_suffix('%') {
            let p = parse_number(p).ok_or_else(|| err(format!("`{tok}` is not a percentage")))?;
            t.abs() * p / 100.0
        } else {
            limit_number(tok)?
        };
        if d < 0.0 {
            return Err(err("the tolerance must not be negative"));
        }
        spec.min = Some(t - d);
        spec.max = Some(t + d);
        *s = rest;
    }
    Ok(())
}

/// The shortest decimal `d` with `t - d == lo` and `t + d == hi` exactly,
/// so a parsed `3.3 +- 2%` prints back as a tolerance rather than as the
/// float noise of `3.3 - 0.066`.
fn short_tolerance(t: f64, lo: f64, hi: f64) -> Option<f64> {
    let raw = t - lo;
    if !raw.is_finite() || raw < 0.0 {
        return None;
    }
    (1..=17).find_map(|digits| {
        let d: f64 = format!("{:.*e}", digits - 1, raw).parse().ok()?;
        let printed = limit_number(&format_number(d)).ok()?;
        (t - printed == lo && t + printed == hi).then_some(printed)
    })
}

fn next_token(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    (&s[..end], &s[end..])
}

impl fmt::Display for Spec {
    /// The text form; parsing it gives back the same spec.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plain = plain_numbers(&self.measure);
        let num = |v: f64| {
            if plain && v.is_finite() {
                v.to_string()
            } else {
                format_number(v)
            }
        };
        write!(f, "{} = {}", self.name, self.measure)?;
        let mut target_done = false;
        match (self.min, self.max, self.target) {
            (Some(lo), Some(hi), Some(t)) => {
                // `= t +- d` when some short d reads back exactly.
                if let Some(d) = short_tolerance(t, lo, hi) {
                    write!(f, " = {} +- {}", num(t), num(d))?;
                    target_done = true;
                } else {
                    write!(f, " in {}..{}", num(lo), num(hi))?;
                }
            }
            (Some(lo), Some(hi), None) => write!(f, " in {}..{}", num(lo), num(hi))?,
            (Some(lo), None, _) => write!(f, " >= {}", num(lo))?,
            (None, Some(hi), _) => write!(f, " <= {}", num(hi))?,
            (None, None, Some(t)) => {
                write!(f, " = {}", num(t))?;
                target_done = true;
            }
            (None, None, None) => {}
        }
        if let (Some(t), false) = (self.target, target_done) {
            write!(f, " target {}", num(t))?;
        }
        if let Some(u) = &self.unit {
            write!(f, " {u}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::Complex;
    use crate::expr::test_support::*;

    fn spec(text: &str) -> Spec {
        Spec::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    /// AC response of a gain-10 single-pole amplifier with the pole at `fc`.
    fn amp(fc: f64) -> Dataset {
        let f = logspace(10.0, 1e8, 141);
        let h: Vec<Complex> = f.iter().map(|&f| poles(f, &[fc], 10.0)).collect();
        let vin = vec![Complex::new(1.0, 0.0); f.len()];
        ac(f, vec![("V(out)", h), ("V(in)", vin)])
    }

    #[test]
    fn parses_the_examples() {
        let s = spec("gain = gain_db_at(V(out)/V(in), 1k) >= 20");
        assert_eq!(s.name, "gain");
        assert_eq!(s.min, Some(20.0));
        assert_eq!(s.max, None);
        assert_eq!(
            s.measure,
            Measure::GainDbAt {
                expr: "V(out)/V(in)".into(),
                freq: 1e3
            }
        );
        let s = spec("bw = bandwidth_3db(V(out)) in 1Meg..2Meg");
        assert_eq!((s.min, s.max), (Some(1e6), Some(2e6)));
        let s = spec("pm = phase_margin(V(out)) >= 45");
        assert_eq!(s.min, Some(45.0));
        let s = spec("vout = avg(V(out), 1m, 2m) = 3.3 +- 2%");
        assert_eq!(s.target, Some(3.3));
        assert!((s.min.unwrap() - 3.234).abs() < 1e-12 && (s.max.unwrap() - 3.366).abs() < 1e-12);
        let s = spec("os = overshoot_pct(V(out)) <= 10 target 5");
        assert_eq!((s.max, s.target), (Some(10.0), Some(5.0)));
        let s = spec("bw2 = bandwidth_3db(V(out)) >= 1MHz");
        assert_eq!(s.min, Some(1e6));
        let s = spec("g = gain_db_at(V(out), 1k) >=20dB");
        assert_eq!(s.min, Some(20.0));
        let s = spec("g = gain_db_at(V(out), 1k) >= 20 dB");
        assert_eq!(s.unit.as_deref(), Some("dB"));
        let s = spec("t = rise_time(V(out)) < 10us");
        assert!((s.max.unwrap() - 10e-6).abs() < 1e-18);
        let s = spec("info = max(V(out))");
        assert_eq!((s.min, s.max, s.target), (None, None, None));
        let s = spec("v = value_at(V(out), 1m) = 2.5 ± 0.1");
        assert_eq!((s.min, s.max), (Some(2.4), Some(2.6)));
        let s = spec("c = crossing(V(a,b), 0, rise, 2) in 1m..2m");
        assert_eq!(s.measure.exprs(), vec!["V(a,b)"]);
    }

    #[test]
    fn display_round_trips() {
        for text in [
            "gain = gain_db_at(V(out)/V(in), 1k) >= 20",
            "bw = bandwidth_3db(V(out)) in 1Meg..2Meg",
            "pm = phase_margin(V(out)) >= 45",
            "os = overshoot_pct(V(out)) <= 10 target 5",
            "v = value_at(V(out), 1m) = 2.5 +- 100m",
            "vdb = gain_db_at(V(out), 1k) = 6 +- 0.1",
            "t = rise_time(V(out), 20, 80) <= 10u",
            "info = max(V(out), to=1m)",
            "g = gain_db_at(V(out), 1k) >= 20 dB",
            "z = freq_at_db(V(out), -0.5) <= 1k",
            "tgt = peak_gain(V(out)) = 6.02",
            "gm = gain_margin(V(out)) >= 0.5",
        ] {
            let s = spec(text);
            assert_eq!(s.to_string(), text);
            assert_eq!(spec(&s.to_string()), s);
        }
        // Tolerances that do not print exactly fall back to an explicit range.
        let s = spec("v = avg(V(out)) = 3.3 +- 2%");
        let shown = s.to_string();
        assert_eq!(spec(&shown), s, "{shown}");
        // JSON round trip, with lenient numbers.
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Spec>(&json).unwrap(), s);
        let lenient: Spec = serde_json::from_str(
            r#"{"name":"bw","measure":{"kind":"bandwidth_3db","expr":"V(out)"},"min":"1Meg"}"#,
        )
        .unwrap();
        assert_eq!(lenient.min, Some(1e6));
    }

    #[test]
    fn parse_errors() {
        for (text, needle) in [
            ("gain_db_at(V(out), 1k) >= 20", "not a valid spec name"),
            ("no equals here", "expected `name = measurement`"),
            ("x = nope(V(out))", "unknown measurement"),
            ("x = max(V(out)) >= soon", "not a number"),
            ("x = max(V(out)) in 2..1", "is empty"),
            ("x = max(V(out)) in 1", "expected a range"),
            ("x = max(V(out)) ~ 3", "expected `>=`"),
            ("x = max(V(out)) >= 1 V extra", "unexpected `extra`"),
            ("x = max(V(out)) = 3 +- -1", "must not be negative"),
            ("x = max(V(out)) = 3 target 4", "twice"),
            ("1x = max(V(out))", "not a valid spec name"),
        ] {
            let e = Spec::parse(text).unwrap_err().to_string();
            assert!(e.contains(needle), "{text}: {e}");
        }
        let e = parse_specs("a = max(V(x))\n# comment\n\nb = nope(V(x))").unwrap_err();
        assert_eq!(e.line, Some(4));
        assert!(e.to_string().starts_with("line 4: "));
        let e = parse_specs("a = max(V(x))\na = min(V(x))").unwrap_err();
        assert!(e.to_string().contains("defined twice"));
        let ok = parse_specs("# specs\na = max(V(x)) <= 1\n  b = min(V(x)) >= 0  \n").unwrap();
        assert_eq!(ok.len(), 2);
    }

    #[test]
    fn evaluates_with_margins() {
        let ds = amp(100e3);
        let specs = parse_specs(
            "gain = gain_db_at(V(out)/V(in), 1k) >= 19\n\
             bw = bandwidth_3db(V(out)) in 50k..200k\n\
             pm = phase_margin(V(out)) >= 100\n\
             peak = peak_gain(V(out)) = 20\n\
             missing = bandwidth_3db(V(nope)) >= 1",
        )
        .unwrap();
        let report = evaluate(&specs, &[ds]);
        let gain = report.row("gain").unwrap();
        assert!(gain.pass);
        assert!((gain.margin.unwrap() - (20.0 - 19.0) / 19.0).abs() < 1e-3);
        assert_eq!(gain.unit, "dB");
        let bw = report.row("bw").unwrap();
        assert!(bw.pass);
        // Nearest limit is 50k: (100k - 50k) / 50k = 1, versus (200k-100k)/200k = 0.5.
        assert!((bw.margin.unwrap() - 0.5).abs() < 5e-3, "{:?}", bw.margin);
        let pm = report.row("pm").unwrap();
        // A single pole with gain 10 has about 95.7 degrees of margin.
        assert!(!pm.pass);
        assert!(pm.margin.unwrap() < 0.0);
        assert!(pm.display.contains("FAIL"), "{}", pm.display);
        let peak = report.row("peak").unwrap();
        assert!(peak.pass && peak.margin.is_none());
        assert!(peak.display.contains("target 20dB"), "{}", peak.display);
        let missing = report.row("missing").unwrap();
        assert!(!missing.pass && missing.value.is_none());
        assert!(
            missing.display.contains("unknown vector"),
            "{}",
            missing.display
        );
        assert!(!report.all_pass);
        assert!(
            report
                .summary
                .starts_with("3 of 5 specs pass; failing: pm (95.7"),
            "{}",
            report.summary
        );
        assert!(
            report.summary.ends_with("missing (no value)"),
            "{}",
            report.summary
        );
        assert_eq!(report.worst_margin(), None);
        assert_eq!(
            gain.display,
            format!("gain: 20dB (>= 19dB) pass, margin +{:.1}%", 100.0 / 19.0)
        );
        let text = report.to_string();
        assert_eq!(text.lines().count(), 6);
    }

    #[test]
    fn stepped_runs_report_the_worst_step() {
        let ds = stepped(vec![
            ("C=1n", amp(100e3)),
            ("C=2n", amp(50e3)),
            ("C=4n", amp(25e3)),
        ]);
        let specs = parse_specs("bw = bandwidth_3db(V(out)) >= 40k").unwrap();
        let report = evaluate(&specs, std::slice::from_ref(&ds));
        let row = &report.rows[0];
        assert!(!row.pass);
        assert_eq!(row.worst_step.as_deref(), Some("C=4n"));
        assert!((row.value.unwrap() - 25e3).abs() < 100.0);
        assert!(row.display.contains("worst at C=4n"), "{}", row.display);
        let specs = parse_specs("bw = bandwidth_3db(V(out)) >= 20k").unwrap();
        let report = evaluate(&specs, &[ds]);
        assert!(report.all_pass);
        assert_eq!(report.summary, "the spec passes");
        assert!((report.worst_margin().unwrap() - 0.25).abs() < 5e-3);
    }

    #[test]
    fn margins_handle_zero_limits() {
        assert_eq!(margin(0.5, Some(0.0), None), Some(0.5));
        assert_eq!(margin(-0.5, Some(0.0), Some(2.0)), Some(-0.25));
        assert_eq!(margin(1.0, None, None), None);
        assert_eq!(margin(15.0, None, Some(10.0)), Some(-0.5));
        assert!(passes(1.0, Some(1.0), Some(1.0)));
        assert!(!passes(f64::NAN, None, None));
    }
}

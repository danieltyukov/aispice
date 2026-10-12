//! What simulators say besides waveforms: `.meas` results, errors, warnings,
//! `.step` labels and run time.
//!
//! LTspice writes a `.log` file: 8-bit text for normal runs, UTF-16LE when a
//! fatal error stops it, so the encoding is detected per file. ngspice and
//! Xyce print the same kind of information on stdout. Each simulator gets its
//! own parser because the formats share nothing but intent, and a missed error
//! line is worse than an unparsed one: unrecognised text is simply ignored,
//! while anything that looks like an error is kept verbatim.

use crate::dataset::Complex;
use serde::{Deserialize, Serialize};

/// One `.meas` result as the simulator reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub name: String,
    /// Index of the `.step` run this value belongs to, `None` when the run
    /// was not stepped.
    pub step: Option<usize>,
    /// The result. For an AC measurement this is the real value when the
    /// result is real (phase 0 or 180 degrees) and the magnitude otherwise;
    /// the full value is in `complex`. `None` when the measurement failed.
    pub value: Option<f64>,
    pub complex: Option<Complex>,
    /// What was measured, as the simulator echoed it: `MAX(v(out))`,
    /// `v(out)`, `vmax/2`. Empty when the simulator does not say.
    pub expr: String,
    /// The rest of the line, such as `FROM 0 TO 0.005` or `at 0.002`.
    pub detail: String,
}

impl Measurement {
    pub fn failed(&self) -> bool {
        self.value.is_none()
    }

    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            step: None,
            value: None,
            complex: None,
            expr: String::new(),
            detail: String::new(),
        }
    }
}

/// Everything parsed out of a log or console output.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LogReport {
    pub measurements: Vec<Measurement>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    /// `.step` run labels in order, such as `r=1000`. Empty when unstepped.
    pub step_labels: Vec<String>,
    /// Simulator-reported elapsed time in seconds.
    pub elapsed: Option<f64>,
}

impl LogReport {
    fn error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !msg.is_empty() && !self.errors.contains(&msg) {
            self.errors.push(msg);
        }
    }

    fn warning(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !msg.is_empty() && !self.warnings.contains(&msg) {
            self.warnings.push(msg);
        }
    }

    /// Measurement results by name and step.
    pub fn measurement(&self, name: &str, step: Option<usize>) -> Option<&Measurement> {
        self.measurements
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(name) && m.step == step)
    }
}

/// Phrases that mean the run failed even when the line does not start with
/// `Error`. Matched case-insensitively.
const FAILURE_PHRASES: &[&str] = &[
    "singular matrix",
    "time step too small",
    "timestep too small",
    "unknown subcircuit",
    "missing model",
    "iteration limit reached",
    "gmin stepping failed",
    "source stepping failed",
    "could not open",
    "can't find definition",
    "unknown device",
    "analysis failed",
];

fn has_failure_phrase(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    FAILURE_PHRASES.iter().any(|p| lower.contains(p))
}

/// Decode an LTspice log in whichever encoding it was written and parse it.
pub fn parse_ltspice_log_bytes(bytes: &[u8]) -> (String, LogReport) {
    let (text, _) = aispice_core::encoding::decode(bytes);
    let report = parse_ltspice_log(&text);
    (text, report)
}

/// Parse an LTspice `.log`.
pub fn parse_ltspice_log(text: &str) -> LogReport {
    let mut r = LogReport::default();
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut i = 0;
    let mut fourier_of: Option<String> = None;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();
        i += 1;
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if let Some(rest) = t.strip_prefix(".step ") {
            r.step_labels.push(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = lower.strip_prefix("total elapsed time:") {
            r.elapsed = rest.split_whitespace().next().and_then(|v| v.parse().ok());
            continue;
        }
        if let Some(name) = t.strip_prefix("Measurement: ") {
            // A stepped table: header row, then one row per step.
            let name = name.trim();
            let header = lines.get(i).map(|l| l.trim()).unwrap_or("");
            i += 1;
            let mut cols = header.split('\t').map(str::trim).skip(1);
            let expr = cols.next().unwrap_or("").to_string();
            let extra: Vec<&str> = cols.collect();
            while let Some(row) = lines.get(i) {
                let fields: Vec<&str> = row.trim().split('\t').map(str::trim).collect();
                let Some(step) = fields.first().and_then(|s| s.parse::<usize>().ok()) else {
                    break;
                };
                i += 1;
                let mut m = Measurement::new(name);
                m.step = Some(step.saturating_sub(1));
                m.expr = expr.clone();
                if let Some(v) = fields.get(1) {
                    set_value(&mut m, v);
                }
                m.detail = extra
                    .iter()
                    .zip(fields.iter().skip(2))
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                r.measurements.push(m);
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("Measurement \"") {
            let name = rest.split('"').next().unwrap_or("");
            if lower.contains("fail") {
                r.measurements.push(Measurement::new(name));
                r.warning(t);
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("Fourier components of ") {
            fourier_of = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = t.strip_prefix("Total Harmonic Distortion:") {
            let pct = rest.trim().split('%').next().unwrap_or("");
            let target = fourier_of.clone().unwrap_or_default();
            let mut m = Measurement::new(&format!("thd({target})"));
            m.value = pct.trim().parse().ok();
            m.expr = format!("THD of {target}");
            m.detail = "percent".into();
            r.measurements.push(m);
            continue;
        }
        if lower.starts_with("fatal error") || lower.starts_with("error") {
            // Continuation lines are indented: the offending netlist line.
            let mut msg = t.to_string();
            while let Some(next) = lines.get(i) {
                if next.starts_with(' ') || next.starts_with('\t') {
                    let n = next.trim();
                    if !n.is_empty() {
                        msg = format!("{}: {n}", msg.trim_end_matches(':'));
                    }
                    i += 1;
                } else {
                    break;
                }
            }
            r.error(msg);
            continue;
        }
        if lower.starts_with("warning") || lower.contains("questionable") {
            r.warning(t);
            continue;
        }
        if has_failure_phrase(t) {
            r.error(t);
            continue;
        }
        if let Some(m) = parse_ltspice_measurement(t) {
            r.measurements.push(m);
        }
    }
    r
}

/// One unstepped LTspice measurement line, in any of its forms:
///
/// - `vmax: MAX(v(out))=0.993269 FROM 0 TO 0.005`
/// - `vend: v(out)=0.864631 at 0.002` (FIND: the value comes first)
/// - `tcross: v(out)=0.5 AT 0.000693954` (WHEN: the value is the time)
/// - `half: vmax/2=0.496635` (PARAM)
/// - `g1k: v(out)=(-1.44507dB,-32.1419°) at 1000` (AC)
/// - `trise=0.00219675 FROM 0.000106095 TO 0.00230285` (TRIG/TARG)
fn parse_ltspice_measurement(line: &str) -> Option<Measurement> {
    let ident = |s: &str| {
        !s.is_empty()
            && s.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if let Some((name, rest)) = line.split_once(": ")
        && ident(name)
    {
        let (head, detail) = split_detail(rest);
        let (expr, value) = head.rsplit_once('=')?;
        let mut m = Measurement::new(name);
        m.expr = expr.trim().to_string();
        m.detail = detail.to_string();
        if let Some(when) = detail.strip_prefix("AT ") {
            // WHEN: the result is where the condition held.
            set_value(&mut m, when.trim());
        } else {
            set_value(&mut m, value.trim());
        }
        return (m.value.is_some()).then_some(m);
    }
    let (name, rest) = line.split_once('=')?;
    if !ident(name) {
        return None;
    }
    let (value, detail) = split_detail(rest);
    let mut m = Measurement::new(name);
    m.detail = detail.to_string();
    set_value(&mut m, value.trim());
    (m.value.is_some() && !m.detail.is_empty()).then_some(m)
}

/// Split `0.99 FROM 0 TO 5m` into the value part and the qualifier.
fn split_detail(s: &str) -> (&str, &str) {
    for key in [" FROM ", " at ", " AT "] {
        if let Some(i) = s.find(key) {
            return (&s[..i], s[i + 1..].trim());
        }
    }
    (s, "")
}

/// Parse a measurement value: a number, `(XdB,Y°)` or `(re,im)`.
fn set_value(m: &mut Measurement, text: &str) {
    let t = text.trim();
    if let Some(inner) = t.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        let Some((a, b)) = inner.split_once(',') else {
            return;
        };
        let z = if let Some(db) = a.trim().strip_suffix("dB") {
            let deg = b
                .trim()
                .trim_end_matches(['\u{b0}', '\u{fffd}'])
                .trim_end_matches(|c: char| !c.is_ascii_digit() && c != '.');
            let (Ok(db), Ok(deg)) = (db.trim().parse::<f64>(), deg.trim().parse::<f64>()) else {
                return;
            };
            let mag = 10f64.powf(db / 20.0);
            let rad = deg.to_radians();
            Complex::new(mag * rad.cos(), mag * rad.sin())
        } else {
            let (Ok(re), Ok(im)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>()) else {
                return;
            };
            Complex::new(re, im)
        };
        let real = z.im.abs() <= 1e-9 * z.abs().max(f64::MIN_POSITIVE);
        m.value = Some(if real { z.re } else { z.abs() });
        m.complex = Some(z);
        return;
    }
    m.value = t.parse::<f64>().ok();
}

/// Parse ngspice's console output (stdout and stderr together).
pub fn parse_ngspice_output(text: &str) -> LogReport {
    let mut r = LogReport::default();
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut i = 0;
    let mut in_meas = false;
    let mut interrupted = false;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();
        i += 1;
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("measurements for") {
            in_meas = true;
            continue;
        }
        if let Some(rest) = lower.strip_prefix("total elapsed time (seconds) =") {
            r.elapsed = rest.trim().parse().ok();
            in_meas = false;
            continue;
        }
        if lower.starts_with("total analysis time") || lower.starts_with("no. of data rows") {
            in_meas = false;
            continue;
        }
        if lower.starts_with("error on line") {
            let mut parts = vec![t.trim_end_matches(':').to_string()];
            while let Some(next) = lines.get(i) {
                let n = next.trim();
                if !(next.starts_with(' ') || next.starts_with('\t')) || n.is_empty() {
                    break;
                }
                i += 1;
                if n.to_ascii_lowercase().contains("simulation interrupted") {
                    interrupted = true;
                    continue;
                }
                parts.push(n.to_string());
            }
            r.error(parts.join(": "));
            continue;
        }
        if lower.starts_with("error: measure") {
            // A failed .meas is reported here and on the `failed!` line.
            r.warning(t);
            continue;
        }
        if lower.starts_with(".meas") && lower.ends_with("failed!") {
            let name = t.split_whitespace().nth(2).unwrap_or("");
            r.measurements.push(Measurement::new(name));
            r.warning(t);
            continue;
        }
        if lower.contains("simulation interrupted") {
            interrupted = true;
            continue;
        }
        if lower.starts_with("error") || lower.starts_with("doanalyses:") {
            r.error(t);
            continue;
        }
        if lower.starts_with("warning") {
            if lower.contains("singular matrix") {
                r.error(t);
            } else {
                r.warning(t);
            }
            continue;
        }
        if has_failure_phrase(t) && !lower.starts_with("note") {
            r.error(t);
            continue;
        }
        if in_meas && let Some(m) = parse_ngspice_measurement(t) {
            r.measurements.push(m);
        }
    }
    if interrupted && r.errors.is_empty() {
        r.error("Simulation interrupted due to error");
    }
    r
}

/// `vmax = 9.932623e-01 at= 5.000000e-03` or
/// `trise = 2.197212e-03 targ= 2.302579e-03 trig= 1.053663e-04`.
fn parse_ngspice_measurement(line: &str) -> Option<Measurement> {
    let (name, rest) = line.split_once('=')?;
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let mut words = rest.split_whitespace();
    let value = words.next()?;
    let mut m = Measurement::new(name);
    m.detail = words.collect::<Vec<_>>().join(" ");
    // A PARAM measurement whose inputs failed prints `name = failed`.
    if value.eq_ignore_ascii_case("failed") {
        return Some(m);
    }
    set_value(&mut m, value);
    m.value.is_some().then_some(m)
}

/// Parse Xyce's console output.
pub fn parse_xyce_output(text: &str) -> LogReport {
    let mut r = LogReport::default();
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut i = 0;
    let mut in_meas = false;
    let mut aborted = false;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();
        i += 1;
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if lower.contains("***** measure functions") {
            in_meas = true;
            continue;
        }
        if t.starts_with("*****") {
            in_meas = false;
            if let Some(rest) = lower.strip_prefix("***** total elapsed run time:") {
                r.elapsed = rest.split_whitespace().next().and_then(|v| v.parse().ok());
            }
            continue;
        }
        if lower.starts_with("netlist error") || lower.starts_with("netlist warning") {
            let error = lower.starts_with("netlist error");
            let mut msg = t.to_string();
            while let Some(next) = lines.get(i) {
                if next.starts_with(' ') && !next.trim().is_empty() {
                    msg.push_str(": ");
                    msg.push_str(next.trim());
                    i += 1;
                } else {
                    break;
                }
            }
            if error {
                r.error(msg);
            } else if !lower.contains("no print specified") {
                // aispice always asks for a raw file, so the missing .print
                // warning is noise.
                r.warning(msg);
            }
            continue;
        }
        if lower.starts_with("simulation aborted") {
            aborted = true;
            continue;
        }
        if lower.starts_with("*** xyce abort") || lower == "errors" {
            continue;
        }
        if lower.starts_with("error") || lower.starts_with("msg_fatal") {
            r.error(t);
            continue;
        }
        if lower.starts_with("warning") {
            r.warning(t);
            continue;
        }
        if in_meas {
            if let Some(m) = parse_xyce_measurement(t) {
                r.measurements.push(m);
            }
            continue;
        }
        if has_failure_phrase(t) {
            r.error(t);
        }
    }
    if aborted && r.errors.is_empty() {
        r.error("Xyce aborted the simulation");
    }
    r
}

/// `VMAX = 9.934761e-01 at time = 5.000000e-03` or `NEVER = FAILED`.
fn parse_xyce_measurement(line: &str) -> Option<Measurement> {
    if line.to_ascii_lowercase().starts_with("measure start time") {
        return None;
    }
    let (name, rest) = line.split_once(" = ")?;
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let mut words = rest.split_whitespace();
    let value = words.next()?;
    let mut m = Measurement::new(name);
    m.detail = words.collect::<Vec<_>>().join(" ");
    if !value.eq_ignore_ascii_case("failed") {
        set_value(&mut m, value);
        m.value?;
    }
    Some(m)
}

/// Parse Spectre's log: `ERROR (SFE-868): ...` and `WARNING (...)` blocks,
/// whose message continues on indented lines, and the elapsed time from
/// `Total time required for simulation ...: ... elapsed = 1.2 s`.
/// The results of `.meas` lines from Spectre's `<deck>.measure` file, which
/// every release tried (19.1 to 25.1) writes in SPICE mode instead of
/// printing them: `name = value` lines under the `Analysis Type` that made
/// them, `NaN` for a measurement that failed. The lines before the first
/// analysis (date, design, version) are not results.
pub fn parse_spectre_measure(text: &str) -> Vec<Measurement> {
    let mut out = Vec::new();
    let mut analysis: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(kind) = t.strip_prefix("Analysis Type") {
            analysis = Some(kind.trim_start_matches([' ', '\t', ':']).trim().to_string());
            continue;
        }
        let (Some(kind), Some((name, value))) = (&analysis, t.split_once('=')) else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.contains(char::is_whitespace) {
            continue;
        }
        out.push(Measurement {
            name: name.to_string(),
            step: None,
            value: value.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
            complex: None,
            expr: String::new(),
            detail: kind.clone(),
        });
    }
    out
}

pub fn parse_spectre_output(text: &str) -> LogReport {
    let mut r = LogReport::default();
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        i += 1;
        let upper = t.to_ascii_uppercase();
        let is_error = upper.starts_with("ERROR (") || upper.starts_with("ERROR:");
        let is_warning = upper.starts_with("WARNING (") || upper.starts_with("WARNING:");
        if is_error || is_warning {
            let mut msg = t.to_string();
            while let Some(next) = lines.get(i) {
                let n = next.trim();
                if n.is_empty() || !(next.starts_with(' ') || next.starts_with('\t')) {
                    break;
                }
                let nu = n.to_ascii_uppercase();
                if nu.starts_with("ERROR") || nu.starts_with("WARNING") {
                    break;
                }
                msg.push(' ');
                msg.push_str(n);
                i += 1;
            }
            if is_error {
                r.error(msg);
            } else {
                r.warning(msg);
            }
            continue;
        }
        if t.starts_with("Total time required for simulation")
            && let Some(e) = t.split("elapsed =").nth(1)
        {
            r.elapsed = e.split_whitespace().next().and_then(|v| v.parse().ok());
        }
    }
    r
}

/// Parse a Xyce `.mt0` measure file: `NAME = value` per line.
pub fn parse_xyce_mt(text: &str) -> Vec<Measurement> {
    text.lines()
        .filter_map(|l| parse_xyce_measurement(l.trim()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ltspice_measurement_forms() {
        let m = parse_ltspice_measurement("vmax: MAX(v(out))=0.993269 FROM 0 TO 0.005").unwrap();
        assert_eq!(m.name, "vmax");
        assert_eq!(m.expr, "MAX(v(out))");
        assert_eq!(m.value, Some(0.993269));
        assert_eq!(m.detail, "FROM 0 TO 0.005");

        let m = parse_ltspice_measurement("vend: v(out)=0.864631 at 0.002").unwrap();
        assert_eq!(m.value, Some(0.864631));

        let m = parse_ltspice_measurement("tcross: v(out)=0.5 AT 0.000693954").unwrap();
        assert_eq!(m.value, Some(0.000693954));

        let m = parse_ltspice_measurement("half: vmax/2=0.496635").unwrap();
        assert_eq!((m.expr.as_str(), m.value), ("vmax/2", Some(0.496635)));

        let m =
            parse_ltspice_measurement("trise=0.00219675 FROM 0.000106095 TO 0.00230285").unwrap();
        assert_eq!(m.name, "trise");
        assert_eq!(m.value, Some(0.00219675));

        let m =
            parse_ltspice_measurement("g1k: v(out)=(-1.44507dB,-32.1419\u{b0}) at 1000").unwrap();
        let z = m.complex.unwrap();
        assert!((z.db() + 1.44507).abs() < 1e-9);
        assert!((z.phase_deg() + 32.1419).abs() < 1e-9);
        assert!((m.value.unwrap() - z.abs()).abs() < 1e-12);

        let m = parse_ltspice_measurement("fc2: fc*2=(70.0597dB,0\u{b0})").unwrap();
        assert!((m.value.unwrap() - 3184.08).abs() < 0.01);

        assert!(parse_ltspice_measurement("tnom = 27").is_none());
        assert!(parse_ltspice_measurement("method = modified trap").is_none());
        assert!(parse_ltspice_measurement("Date: Sat Oct 10 14:11:39 2026").is_none());
    }

    #[test]
    fn ltspice_errors_join_their_continuation_lines() {
        let r = parse_ltspice_log(
            "Circuit: bad\n\nFatal Error: Unknown subcircuit called in:\n   x1 in out nosuchsub\n\n",
        );
        assert_eq!(
            r.errors,
            vec!["Fatal Error: Unknown subcircuit called in: x1 in out nosuchsub"]
        );
    }

    #[test]
    fn ngspice_measurement_lines() {
        let m = parse_ngspice_measurement("vmax                =  9.932623e-01 at=  5.000000e-03")
            .unwrap();
        assert_eq!((m.name.as_str(), m.value), ("vmax", Some(0.9932623)));
        assert_eq!(m.detail, "at= 5.000000e-03");
        let f = parse_ngspice_measurement("half                =   failed").unwrap();
        assert!(f.failed());
    }

    #[test]
    fn spectre_errors_and_warnings() {
        // The layout of Spectre's log as documented in its user guide.
        let log = "Error found by spectre during circuit read-in.\n    ERROR (SFE-868): \"rc.scs\" 4: Cannot open the input file\n        'models.scs'.\n\nWARNING (SPECTRE-16707): Only tran analysis is supported\n    in this mode.\nTotal time required for simulation `rc.scs': CPU = 12 ms, elapsed = 0.25 s.\nspectre completes with 1 error, 1 warning, and 0 notices.\n";
        let r = parse_spectre_output(log);
        assert_eq!(
            r.errors,
            vec!["ERROR (SFE-868): \"rc.scs\" 4: Cannot open the input file 'models.scs'."]
        );
        assert_eq!(
            r.warnings,
            vec!["WARNING (SPECTRE-16707): Only tran analysis is supported in this mode."]
        );
        assert_eq!(r.elapsed, Some(0.25));
    }

    /// `<deck>.measure` as Spectre 25.1 writes it (19.1 pads less).
    #[test]
    fn spectre_measure_file() {
        let text = "Exported variables from results directory:  ./t.raw.psf\n\ndate               :  11:15:14 PM, Sun Oct 11, 2026\ndesign             :  * t\nsimulator          :  spectre\nversion            :  25.1.0.156.isr2\n\n\nMeasurement Name   :  timeSweep\nAnalysis Type      :  tran\nnever                 =  NaN \nvend                  =  0.993339 \nvmax                  =  0.999956 \n\n\nMeasurement Name   :  frequencySweep\nAnalysis Type      :  ac\nf3                    =  1587.8 \ng1k                   =  -1.44507 \n";
        let m = parse_spectre_measure(text);
        let got: Vec<(&str, Option<f64>, &str)> = m
            .iter()
            .map(|m| (m.name.as_str(), m.value, m.detail.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("never", None, "tran"),
                ("vend", Some(0.993339), "tran"),
                ("vmax", Some(0.999956), "tran"),
                ("f3", Some(1587.8), "ac"),
                ("g1k", Some(-1.44507), "ac"),
            ]
        );
        assert!(m[0].failed());
        assert!(parse_spectre_measure("version : 19.1\nx = 1\n").is_empty());
    }

    #[test]
    fn xyce_measurement_lines() {
        let m = parse_xyce_measurement("VEND = 8.645833e-01 for AT = 2.000000e-03").unwrap();
        assert_eq!(m.value, Some(0.8645833));
        let f = parse_xyce_measurement("NEVER = FAILED").unwrap();
        assert!(f.failed());
        assert!(parse_xyce_measurement("Measure Start Time= 0.000000e+00").is_none());
    }
}

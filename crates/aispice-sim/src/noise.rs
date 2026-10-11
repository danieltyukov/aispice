//! Noise analysis results in one shape, whatever simulator produced them.
//!
//! Every simulator reports `.noise` differently: ngspice writes
//! `onoise_spectrum` and `inoise_spectrum` in V/√Hz, LTspice `V(onoise)` and
//! `V(inoise)`, Spectre `out` and `in`, and Xyce writes squared densities to a
//! separate `<deck>_noise.dat` text file instead of its raw file. aispice
//! names them all [`ONOISE`] and [`INOISE`], as densities over a `frequency`
//! axis, so one set of measurements works everywhere.

use crate::dataset::{AnalysisKind, Dataset, Quantity, Step, Vector, VectorData};
use crate::raw::RawError;

/// Output noise density, V/√Hz.
pub const ONOISE: &str = "V(onoise)";
/// Input-referred noise density: V/√Hz, or A/√Hz when the input source is a
/// current source.
pub const INOISE: &str = "V(inoise)";

/// aispice's name and quantity for a vector of a simulator's noise plot,
/// given the variable's name and type as the raw file declares them. `None`
/// keeps the vector as it is.
pub(crate) fn canonical(name: &str, kind: &str) -> Option<(&'static str, Quantity)> {
    let n = name.trim().to_ascii_lowercase();
    let k = kind.trim().to_ascii_lowercase();
    // ngspice types the input `current-density` for a current source input.
    let input_quantity = if k.starts_with("current") || k.starts_with("a/") {
        Quantity::Current
    } else {
        Quantity::Voltage
    };
    match n.as_str() {
        "onoise_spectrum" => Some((ONOISE, Quantity::Voltage)),
        "inoise_spectrum" => Some((INOISE, input_quantity)),
        // Spectre: fixed names, with the unit as the type.
        "out" if k.contains("sqrt(hz)") => Some((ONOISE, Quantity::Voltage)),
        "in" if k.contains("sqrt(hz)") => Some((INOISE, input_quantity)),
        "v(onoise)" => Some((ONOISE, Quantity::Voltage)),
        "v(inoise)" => Some((INOISE, Quantity::Voltage)),
        // LTspice, for a current source input.
        "inoise" | "i(inoise)" => Some((INOISE, Quantity::Current)),
        _ => None,
    }
}

/// The source a deck's `.noise` line refers its noise to, if any.
pub fn input_source(deck: &str) -> Option<String> {
    deck.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        let kw = w.next()?;
        if !kw.eq_ignore_ascii_case(".noise") {
            return None;
        }
        // `.noise V(out[,ref]) src ...`; a space after the comma splits the
        // output into two words.
        let rest: Vec<&str> = w.collect();
        let mut i = 0;
        let mut depth = 0i32;
        while i < rest.len() {
            depth += rest[i].matches('(').count() as i32 - rest[i].matches(')').count() as i32;
            i += 1;
            if depth <= 0 {
                break;
            }
        }
        rest.get(i).map(|s| s.to_string())
    })
}

/// Mark the input-referred noise as a current when the deck's `.noise` line
/// names a current source, for the simulators whose output does not say.
pub fn mark_input_quantity(datasets: &mut [Dataset], deck: &str) {
    let Some(src) = input_source(deck) else {
        return;
    };
    if !src.to_ascii_uppercase().starts_with('I') {
        return;
    }
    for ds in datasets
        .iter_mut()
        .filter(|d| d.kind == AnalysisKind::Noise)
    {
        for v in ds.vectors.iter_mut().filter(|v| v.name == INOISE) {
            v.quantity = Quantity::Current;
        }
    }
}

/// Read Xyce's `<deck>_noise.dat`: a Tecplot text file whose variables are
/// `frequency`, the output's real and imaginary parts, and the output and
/// input noise as squared densities (V²/Hz). Each `ZONE` is one step.
pub fn read_xyce_noise_dat(text: &str) -> Result<Dataset, RawError> {
    let mut names: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<f64>> = Vec::new();
    let mut zone_starts: Vec<usize> = Vec::new();
    let mut in_header = true;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let upper = t.to_ascii_uppercase();
        if upper.starts_with("ZONE") {
            in_header = false;
            zone_starts.push(rows.len());
            continue;
        }
        if in_header {
            // Every quoted string in the header names a variable, except the
            // title.
            let body = match upper.find("VARIABLES") {
                Some(i) => &t[i..],
                None if upper.starts_with("TITLE") => continue,
                None => t,
            };
            let mut parts = body.split('"');
            parts.next();
            while let Some(name) = parts.next() {
                names.push(name.trim().to_string());
                parts.next();
            }
            continue;
        }
        let values: Option<Vec<f64>> = t
            .split_whitespace()
            .map(|w| w.parse::<f64>().ok())
            .collect();
        match values {
            Some(v) if v.len() == names.len() => rows.push(v),
            _ => {
                return Err(RawError::Data(format!(
                    "Xyce noise file: expected {} numbers, found `{t}`",
                    names.len()
                )));
            }
        }
    }
    let col = |want: &str| names.iter().position(|n| n.eq_ignore_ascii_case(want));
    let (Some(f), Some(on), Some(inn)) = (
        col("frequency"),
        col("onoise_spectrum"),
        col("inoise_spectrum"),
    ) else {
        return Err(RawError::Header(format!(
            "Xyce noise file has no frequency, onoise_spectrum and inoise_spectrum columns (it has {})",
            names.join(", ")
        )));
    };
    if rows.is_empty() {
        return Err(RawError::Data("Xyce noise file has no data".into()));
    }
    let density = |i: usize| -> Vec<f64> { rows.iter().map(|r| r[i].max(0.0).sqrt()).collect() };
    if zone_starts.is_empty() {
        zone_starts.push(0);
    }
    let steps: Vec<Step> = zone_starts
        .iter()
        .enumerate()
        .map(|(k, &s)| Step {
            range: s..zone_starts.get(k + 1).copied().unwrap_or(rows.len()),
            label: if zone_starts.len() > 1 {
                format!("step {}", k + 1)
            } else {
                String::new()
            },
        })
        .collect();
    Ok(Dataset {
        title: String::new(),
        plotname: "Noise Spectral Density".into(),
        kind: AnalysisKind::Noise,
        axis: Some(0),
        vectors: vec![
            Vector {
                name: "frequency".into(),
                quantity: Quantity::Frequency,
                data: VectorData::Real(rows.iter().map(|r| r[f]).collect()),
            },
            Vector {
                name: ONOISE.into(),
                quantity: Quantity::Voltage,
                data: VectorData::Real(density(on)),
            },
            Vector {
                name: INOISE.into(),
                quantity: Quantity::Voltage,
                data: VectorData::Real(density(inn)),
            },
        ],
        steps,
    })
}

/// Put the spectra from Xyce's noise file into the run's datasets: beside
/// the small-signal solution of the raw file's noise plot when it has the
/// same points, otherwise as a dataset of their own.
pub fn merge_xyce_noise(datasets: &mut Vec<Dataset>, spectra: Dataset) {
    if let Some(ds) = datasets
        .iter_mut()
        .find(|d| d.kind == AnalysisKind::Noise && d.len() == spectra.len() && d.axis.is_some())
    {
        ds.vectors.retain(|v| {
            !v.name.eq_ignore_ascii_case(ONOISE) && !v.name.eq_ignore_ascii_case(INOISE)
        });
        ds.vectors.extend(spectra.vectors.into_iter().skip(1));
        return;
    }
    datasets.push(spectra);
}

/// The rms of a noise density over `from..to` (default: all of it): the
/// square root of the integral of the density squared over frequency.
///
/// Between two points the density squared is taken to follow a power law,
/// which is exact for white and 1/f noise and is how ngspice integrates its
/// own totals; a segment with a zero or negative end falls back to the
/// trapezoid.
pub fn integrate_rms(
    f: &[f64],
    density: &[f64],
    from: Option<f64>,
    to: Option<f64>,
) -> Result<f64, String> {
    let pts: Vec<(f64, f64)> = f
        .iter()
        .zip(density)
        .filter(|(x, y)| x.is_finite() && y.is_finite())
        .map(|(&x, &y)| (x, y * y))
        .collect();
    if pts.len() < 2 {
        return Err("the noise sweep has fewer than two points".into());
    }
    let (first, last) = (pts[0].0, pts[pts.len() - 1].0);
    let lo = from.unwrap_or(first);
    let hi = to.unwrap_or(last);
    let tol = 1e-9 * last.abs().max(first.abs());
    if !(lo.is_finite() && hi.is_finite()) || hi <= lo {
        return Err(format!("the band {lo}..{hi} Hz is empty"));
    }
    if lo < first - tol || hi > last + tol {
        return Err(format!(
            "the band {}..{} is outside the swept range {}..{}",
            crate::measure::human(lo, "Hz"),
            crate::measure::human(hi, "Hz"),
            crate::measure::human(first, "Hz"),
            crate::measure::human(last, "Hz")
        ));
    }
    let (lo, hi) = (lo.max(first), hi.min(last));
    let mut total = 0.0;
    for w in pts.windows(2) {
        let ((f0, p0), (f1, p1)) = (w[0], w[1]);
        if f1 <= f0 || f1 <= lo || f0 >= hi {
            continue;
        }
        let a = f0.max(lo);
        let b = f1.min(hi);
        let (pa, pb) = (power_at(f0, p0, f1, p1, a), power_at(f0, p0, f1, p1, b));
        total += segment(a, pa, b, pb);
    }
    Ok(total.max(0.0).sqrt())
}

/// The power-law exponent between two points, when both are positive.
fn exponent(f0: f64, p0: f64, f1: f64, p1: f64) -> Option<f64> {
    (f0 > 0.0 && f1 > 0.0 && p0 > 0.0 && p1 > 0.0).then(|| (p1 / p0).ln() / (f1 / f0).ln())
}

fn power_at(f0: f64, p0: f64, f1: f64, p1: f64, f: f64) -> f64 {
    if f == f0 {
        return p0;
    }
    if f == f1 {
        return p1;
    }
    match exponent(f0, p0, f1, p1) {
        Some(e) => p0 * (f / f0).powf(e),
        None => p0 + (p1 - p0) * (f - f0) / (f1 - f0),
    }
}

fn segment(f0: f64, p0: f64, f1: f64, p1: f64) -> f64 {
    match exponent(f0, p0, f1, p1) {
        Some(e) if (e + 1.0).abs() < 1e-9 => p0 * f0 * (f1 / f0).ln(),
        Some(e) => p0 * f0 / (e + 1.0) * ((f1 / f0).powf(e + 1.0) - 1.0),
        None => 0.5 * (p0 + p1) * (f1 - f0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logspace(a: f64, b: f64, per_decade: usize) -> Vec<f64> {
        let n = ((b / a).log10() * per_decade as f64).round() as usize;
        (0..=n)
            .map(|k| a * 10f64.powf(k as f64 / per_decade as f64))
            .collect()
    }

    #[test]
    fn white_noise_integrates_to_density_times_root_bandwidth() {
        let f = logspace(1.0, 1e6, 10);
        let d = vec![2.879e-9; f.len()];
        let v = integrate_rms(&f, &d, None, None).unwrap();
        let want = 2.879e-9 * (1e6f64 - 1.0).sqrt();
        assert!((v - want).abs() < 1e-9 * want, "{v} vs {want}");
        // A band inside the sweep, between sweep points.
        let v = integrate_rms(&f, &d, Some(1.5e3), Some(2.5e4)).unwrap();
        let want = 2.879e-9 * (2.5e4f64 - 1.5e3).sqrt();
        assert!((v - want).abs() < 1e-9 * want, "{v} vs {want}");
    }

    #[test]
    fn flicker_noise_integrates_exactly() {
        // Density squared k/f: the integral from a to b is k ln(b/a), with
        // only two points per decade.
        let f = logspace(1.0, 1e5, 2);
        let k = 1e-14;
        let d: Vec<f64> = f.iter().map(|f| (k / f).sqrt()).collect();
        let v = integrate_rms(&f, &d, Some(10.0), Some(1e4)).unwrap();
        let want = (k * (1e3f64).ln()).sqrt();
        assert!((v - want).abs() < 1e-9 * want, "{v} vs {want}");
        // Density squared k f^-2: integral k (1/a - 1/b).
        let d: Vec<f64> = f.iter().map(|f| (k / (f * f)).sqrt()).collect();
        let v = integrate_rms(&f, &d, None, None).unwrap();
        let want = (k * (1.0 - 1e-5)).sqrt();
        assert!((v - want).abs() < 1e-9 * want, "{v} vs {want}");
    }

    #[test]
    fn bands_outside_the_sweep_are_refused() {
        let f = logspace(10.0, 1e3, 5);
        let d = vec![1e-9; f.len()];
        assert!(integrate_rms(&f, &d, Some(1.0), None).is_err());
        assert!(integrate_rms(&f, &d, None, Some(1e4)).is_err());
        assert!(integrate_rms(&f, &d, Some(500.0), Some(100.0)).is_err());
        assert!(integrate_rms(&f[..1], &d[..1], None, None).is_err());
        // Zero density: the trapezoid, no NaN.
        let z = vec![0.0; f.len()];
        assert_eq!(integrate_rms(&f, &z, None, None).unwrap(), 0.0);
    }

    #[test]
    fn xyce_noise_file_reads_as_densities() {
        let text = "TITLE=\"noise output\"\tVARIABLES=\"frequency\" \n\t\"Re(V(OUT))\"\n\t\"Im(V(OUT))\"\n\t\"onoise_spectrum \"\n\t\"inoise_spectrum \"\n\nZONE F=POINT  T=\"Xyce data\"\n1.000000e+00\t5.000000e-01\t0.000000e+00\t8.287877e-18\t3.315151e-17\n1.000000e+01\t5.000000e-01\t0.000000e+00\t8.287877e-18\t3.315151e-17\n";
        let ds = read_xyce_noise_dat(text).unwrap();
        assert_eq!(ds.kind, AnalysisKind::Noise);
        assert_eq!(ds.names(), vec!["frequency", ONOISE, INOISE]);
        assert_eq!(ds.len(), 2);
        let on = ds.vector("onoise").unwrap().data.real();
        assert!((on[0] - 2.8789e-9).abs() < 1e-12, "{on:?}");
        let inn = ds.vector(INOISE).unwrap().data.real();
        assert!((inn[1] - 5.7577e-9).abs() < 1e-12, "{inn:?}");
        assert!(read_xyce_noise_dat("TITLE=\"x\" VARIABLES=\"frequency\"\nZONE\n1\n").is_err());
        assert!(
            read_xyce_noise_dat(
                "VARIABLES=\"frequency\" \"onoise_spectrum\" \"inoise_spectrum\"\nZONE\n1 2\n"
            )
            .is_err()
        );
    }

    #[test]
    fn noise_input_source_and_current_inputs() {
        assert_eq!(
            input_source("t\n.noise V(out) V1 dec 10 1 1Meg\n").as_deref(),
            Some("V1")
        );
        assert_eq!(
            input_source("t\n.NOISE v(out, ref) Iin dec 10 1 1k\n").as_deref(),
            Some("Iin")
        );
        assert_eq!(input_source("t\n.ac dec 10 1 1k\n"), None);
        let mut ds = vec![
            read_xyce_noise_dat(
                "VARIABLES=\"frequency\" \"onoise_spectrum\" \"inoise_spectrum\"\nZONE\n1 1e-18 1e-24\n",
            )
            .unwrap(),
        ];
        mark_input_quantity(&mut ds, "t\n.noise V(out) V1 dec 1 1 10\n");
        assert_eq!(ds[0].vector(INOISE).unwrap().quantity, Quantity::Voltage);
        mark_input_quantity(&mut ds, "t\n.noise V(out) I1 dec 1 1 10\n");
        assert_eq!(ds[0].vector(INOISE).unwrap().quantity, Quantity::Current);
    }

    #[test]
    fn simulator_noise_names_map_to_one_convention() {
        assert_eq!(
            canonical("onoise_spectrum", "voltage-density"),
            Some((ONOISE, Quantity::Voltage))
        );
        assert_eq!(
            canonical("inoise_spectrum", "current-density"),
            Some((INOISE, Quantity::Current))
        );
        assert_eq!(
            canonical("out", "V/sqrt(Hz)"),
            Some((ONOISE, Quantity::Voltage))
        );
        assert_eq!(
            canonical("in", "A/sqrt(Hz)"),
            Some((INOISE, Quantity::Current))
        );
        // A node named `out` in an AC-like plot is not noise.
        assert_eq!(
            canonical("I(inoise)", "current"),
            Some((INOISE, Quantity::Current))
        );
        assert_eq!(canonical("out", "voltage"), None);
        assert_eq!(canonical("V(r1)", "voltage"), None);
    }
}

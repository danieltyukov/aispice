//! Trim range checks: does every corner and every mismatch sample have a
//! trim code that brings the specs into range?
//!
//! The check simulates every code for every sample, so for the samples it
//! ran the answer is a proof rather than a search that assumes the spec
//! moves monotonically with the code. This module reduces those results:
//! which samples can be trimmed, the code each one needs, how much of the
//! range is left at either end, and the yield without trimming.

use crate::spec::SpecReport;
use serde::Serialize;
use std::collections::BTreeMap;

/// The specs at one code for one sample.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodeResult {
    pub code: i64,
    pub pass: bool,
    /// The smallest relative margin over the specs (negative when one
    /// fails); `None` when a spec had no value.
    pub margin: Option<f64>,
}

impl CodeResult {
    /// From a spec report: passing when every spec passes, the margin the
    /// worst spec's.
    pub fn from_report(code: i64, report: &SpecReport) -> Self {
        let margins: Option<Vec<f64>> = report
            .rows
            .iter()
            .map(|r| r.value.and_then(|v| crate::spec::margin(v, r.min, r.max)))
            .collect();
        CodeResult {
            code,
            pass: report.all_pass,
            margin: margins.and_then(|m| m.into_iter().reduce(f64::min)),
        }
    }

    /// A code whose run failed to simulate.
    pub fn failed(code: i64) -> Self {
        CodeResult {
            code,
            pass: false,
            margin: None,
        }
    }
}

/// Every code for one sample: a corner, or a corner and a Monte Carlo run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SampleResult {
    pub label: String,
    pub codes: Vec<CodeResult>,
}

/// What one sample needs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Need {
    pub label: String,
    /// The passing code with the largest margin (the lower code on a tie);
    /// for a sample no code passes, the code that comes closest.
    pub code: Option<i64>,
    pub margin: Option<f64>,
    pub pass: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrimReport {
    pub samples: usize,
    /// Samples some code brings into range.
    pub covered: usize,
    pub all_covered: bool,
    /// The codes tried, lowest first.
    pub first_code: i64,
    pub last_code: i64,
    /// Samples no code brings into range, with the closest they come.
    pub uncovered: Vec<Need>,
    /// How many samples need each code.
    pub histogram: BTreeMap<i64, usize>,
    /// The lowest and highest code any covered sample needs.
    pub lowest_needed: Option<i64>,
    pub highest_needed: Option<i64>,
    /// Codes left unused below the lowest and above the highest needed:
    /// a range that only just covers the spread shows 0.
    pub headroom_low: Option<i64>,
    pub headroom_high: Option<i64>,
    /// The code without trimming and how many samples pass at it.
    pub nominal_code: i64,
    pub nominal_pass: usize,
    pub needs: Vec<Need>,
}

impl TrimReport {
    pub fn summary(&self) -> String {
        let pct = |n: usize| 100.0 * n as f64 / self.samples.max(1) as f64;
        let mut s = if self.all_covered {
            format!(
                "every one of the {} samples has a code that brings the specs into range",
                self.samples
            )
        } else {
            format!(
                "{} of {} samples cannot be trimmed into range by any code from {} to {}",
                self.uncovered.len(),
                self.samples,
                self.first_code,
                self.last_code
            )
        };
        if let (Some(lo), Some(hi), Some(hl), Some(hh)) = (
            self.lowest_needed,
            self.highest_needed,
            self.headroom_low,
            self.headroom_high,
        ) {
            s.push_str(&format!(
                "; codes {lo} to {hi} are needed, leaving {hl} below and {hh} above"
            ));
        }
        s.push_str(&format!(
            "; untrimmed (code {}) {} of {} pass ({:.1}%)",
            self.nominal_code,
            self.nominal_pass,
            self.samples,
            pct(self.nominal_pass)
        ));
        s
    }
}

fn better(a: &CodeResult, b: &CodeResult) -> bool {
    match (a.margin, b.margin) {
        (Some(x), Some(y)) => x > y,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Reduce the results of every code for every sample.
pub fn summarize(samples: &[SampleResult], nominal_code: i64) -> TrimReport {
    let mut codes: Vec<i64> = samples
        .iter()
        .flat_map(|s| s.codes.iter().map(|c| c.code))
        .collect();
    codes.sort_unstable();
    codes.dedup();
    let first_code = codes.first().copied().unwrap_or(0);
    let last_code = codes.last().copied().unwrap_or(0);
    let mut needs = Vec::with_capacity(samples.len());
    let mut nominal_pass = 0;
    for s in samples {
        let mut best: Option<&CodeResult> = None;
        let mut best_pass: Option<&CodeResult> = None;
        for c in &s.codes {
            if best.is_none_or(|b| better(c, b)) {
                best = Some(c);
            }
            if c.pass && best_pass.is_none_or(|b| better(c, b)) {
                best_pass = Some(c);
            }
            if c.code == nominal_code && c.pass {
                nominal_pass += 1;
            }
        }
        let chosen = best_pass.or(best);
        needs.push(Need {
            label: s.label.clone(),
            code: chosen.map(|c| c.code),
            margin: chosen.and_then(|c| c.margin),
            pass: best_pass.is_some(),
        });
    }
    let mut histogram = BTreeMap::new();
    for n in needs.iter().filter(|n| n.pass) {
        if let Some(c) = n.code {
            *histogram.entry(c).or_insert(0) += 1;
        }
    }
    let lowest_needed = histogram.keys().next().copied();
    let highest_needed = histogram.keys().next_back().copied();
    let index = |code: i64| codes.iter().position(|c| *c == code).map(|i| i as i64);
    let headroom_low = lowest_needed.and_then(index);
    let headroom_high = highest_needed.and_then(|c| index(c).map(|i| codes.len() as i64 - 1 - i));
    let uncovered: Vec<Need> = needs.iter().filter(|n| !n.pass).cloned().collect();
    TrimReport {
        samples: samples.len(),
        covered: samples.len() - uncovered.len(),
        all_covered: uncovered.is_empty() && !samples.is_empty(),
        first_code,
        last_code,
        uncovered,
        histogram,
        lowest_needed,
        highest_needed,
        headroom_low,
        headroom_high,
        nominal_code,
        nominal_pass,
        needs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(label: &str, margins: &[Option<f64>]) -> SampleResult {
        SampleResult {
            label: label.into(),
            codes: margins
                .iter()
                .enumerate()
                .map(|(i, m)| CodeResult {
                    code: i as i64,
                    pass: m.is_some_and(|m| m >= 0.0),
                    margin: *m,
                })
                .collect(),
        }
    }

    #[test]
    fn needs_headroom_and_untrimmed_yield() {
        let samples = [
            // Needs code 1, the best of two passing codes.
            sample("tt", &[Some(-0.2), Some(0.3), Some(0.1), Some(-0.4)]),
            // Passes untrimmed (code 2) and at 3; 3 is better.
            sample("ss", &[Some(-0.5), Some(-0.1), Some(0.05), Some(0.2)]),
            // A failed run at code 0 does not hide the rest.
            sample("ff", &[None, Some(0.1), Some(-0.1), Some(-0.3)]),
        ];
        let r = summarize(&samples, 2);
        assert!(r.all_covered);
        assert_eq!((r.samples, r.covered), (3, 3));
        let codes: Vec<Option<i64>> = r.needs.iter().map(|n| n.code).collect();
        assert_eq!(codes, [Some(1), Some(3), Some(1)]);
        assert_eq!(r.histogram, BTreeMap::from([(1, 2), (3, 1)]));
        assert_eq!((r.lowest_needed, r.highest_needed), (Some(1), Some(3)));
        assert_eq!((r.headroom_low, r.headroom_high), (Some(1), Some(0)));
        assert_eq!(r.nominal_pass, 2);
        assert!(
            r.summary().contains("leaving 1 below and 0 above"),
            "{}",
            r.summary()
        );
    }

    #[test]
    fn a_sample_no_code_trims_is_named_with_its_closest_code() {
        let samples = [
            sample("tt", &[Some(0.1), Some(0.2)]),
            sample("ss/mc3", &[Some(-0.3), Some(-0.05)]),
        ];
        let r = summarize(&samples, 0);
        assert!(!r.all_covered);
        assert_eq!(r.uncovered.len(), 1);
        assert_eq!(r.uncovered[0].label, "ss/mc3");
        assert_eq!(r.uncovered[0].code, Some(1));
        assert_eq!(r.uncovered[0].margin, Some(-0.05));
        assert!(r.summary().starts_with("1 of 2 samples cannot be trimmed"));
        assert!(!summarize(&[], 0).all_covered);
    }
}

//! Derived AC runs for supply and common-mode rejection.
//!
//! PSRR and CMRR compare the gain from the signal input with the gain from
//! another source: the supply, or the common-mode source of a differential
//! test bench. The circuit's own AC run gives the first; for the second,
//! aispice runs the same AC analysis on a copy of the netlist with every AC
//! stimulus removed and `AC 1` on that one source. The datasets of that run
//! are added to the run's results with a title naming the source, and only
//! the rejection measurements read them.

use crate::measure::Measure;
use crate::sweep::word_spans;
use aispice_core::netlist::{Element, Line, Netlist};
use aispice_core::units;

const TITLE_PREFIX: &str = "aispice: AC 1 on ";

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StimulusError {
    #[error(
        "`{name}` is not an independent source at the top level of the circuit{}",
        list(.sources)
    )]
    NotASource { name: String, sources: Vec<String> },
    #[error(
        "PSRR and CMRR need an AC analysis in the circuit (an `.ac` line), whose input carries `AC 1`"
    )]
    NoAc,
}

fn list(sources: &[String]) -> String {
    if sources.is_empty() {
        "; it has none".into()
    } else {
        format!("; its sources are {}", sources.join(", "))
    }
}

/// Mark a dataset as coming from the run with `AC 1` on `source`.
pub fn mark(ds: &mut crate::dataset::Dataset, source: &str) {
    ds.title = format!("{TITLE_PREFIX}{source}");
}

/// The source a derived run put its stimulus on, for a marked dataset.
pub fn marked_source(ds: &crate::dataset::Dataset) -> Option<&str> {
    ds.title.strip_prefix(TITLE_PREFIX)
}

/// The sources the measurements need a derived run for, each once, in the
/// order they first appear.
pub fn needed_sources<'a>(measures: impl IntoIterator<Item = &'a Measure>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in measures {
        if let Some(s) = m.stimulus_source()
            && !out.iter().any(|o| o.eq_ignore_ascii_case(s))
        {
            out.push(s.to_string());
        }
    }
    out
}

fn is_source(e: &Element) -> bool {
    matches!(e.letter(), 'V' | 'I')
}

/// `rest` without its `AC mag [phase]` part.
fn without_ac(rest: &str) -> String {
    let spans = word_spans(rest);
    let words: Vec<&str> = spans.iter().map(|&(a, b)| &rest[a..b]).collect();
    let Some(k) = words.iter().position(|w| w.eq_ignore_ascii_case("ac")) else {
        return rest.to_string();
    };
    let numeric = |w: &str| units::parse(w).is_some() || w.starts_with('{');
    let mut end = k + 1;
    while end < words.len() && end <= k + 2 && numeric(words[end]) {
        end += 1;
    }
    words[..k]
        .iter()
        .chain(&words[end..])
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip(items: &mut [Line]) {
    for item in items {
        match item {
            Line::Element(e) if is_source(e) => e.rest = without_ac(&e.rest),
            Line::Subckt(s) => strip(&mut s.body),
            _ => {}
        }
    }
}

/// The netlist with every AC stimulus removed and `AC 1` on `source`.
pub fn ac_on(netlist: &Netlist, source: &str) -> Result<Netlist, StimulusError> {
    let has_ac = netlist.directives().any(|d| {
        d.split_whitespace()
            .next()
            .is_some_and(|k| k.eq_ignore_ascii_case(".ac"))
    });
    if !has_ac {
        return Err(StimulusError::NoAc);
    }
    if !netlist
        .elements()
        .any(|e| is_source(e) && e.name.eq_ignore_ascii_case(source))
    {
        return Err(StimulusError::NotASource {
            name: source.to_string(),
            sources: netlist
                .elements()
                .filter(|e| is_source(e))
                .map(|e| e.name.clone())
                .collect(),
        });
    }
    let mut n = netlist.clone();
    strip(&mut n.items);
    for item in &mut n.items {
        if let Line::Element(e) = item
            && is_source(e)
            && e.name.eq_ignore_ascii_case(source)
        {
            e.rest = if e.rest.trim().is_empty() {
                "AC 1".into()
            } else {
                format!("{} AC 1", e.rest.trim())
            };
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aispice_core::netlist::{parse, write};

    #[test]
    fn ac_moves_to_the_named_source() {
        let n = parse(
            "t\nVin in 0 SINE(0 1 1k) AC 1\nVDD vdd 0 1.8\nVcm cm 0 DC 0.9 AC 0.5 180 Rser=1\nI1 vdd x AC 2\nR1 in out 1k\n.subckt s a\nV9 a 0 AC 1\n.ends s\n.ac dec 10 1 1Meg\n.end\n",
        );
        let out = write(&ac_on(&n, "vdd").unwrap());
        assert!(out.contains("Vin in 0 SINE(0 1 1k)\n"), "{out}");
        assert!(out.contains("VDD vdd 0 1.8 AC 1\n"), "{out}");
        assert!(out.contains("Vcm cm 0 DC 0.9 Rser=1\n"), "{out}");
        assert!(out.contains("I1 vdd x\n"), "{out}");
        assert!(out.contains("V9 a 0\n"), "{out}");
        assert_eq!(out.matches("AC 1").count(), 1, "{out}");
        let out = write(&ac_on(&n, "Vcm").unwrap());
        assert!(out.contains("Vcm cm 0 DC 0.9 Rser=1 AC 1\n"), "{out}");
    }

    #[test]
    fn refuses_what_it_cannot_drive() {
        let n = parse("t\nV1 in 0 AC 1\nR1 in 0 1k\n.ac dec 10 1 1k\n.end\n");
        assert_eq!(
            ac_on(&n, "R1").unwrap_err().to_string(),
            "`R1` is not an independent source at the top level of the circuit; its sources are V1"
        );
        assert!(matches!(
            ac_on(&n, "VDD"),
            Err(StimulusError::NotASource { .. })
        ));
        let tran = parse("t\nV1 in 0 AC 1\nR1 in 0 1k\n.tran 1m\n.end\n");
        assert_eq!(ac_on(&tran, "V1"), Err(StimulusError::NoAc));
    }

    #[test]
    fn needed_sources_are_unique() {
        let ms: Vec<Measure> = [
            "psrr_db_at(V(out), VDD, 1k)",
            "cmrr_db_at(V(out), Vcm, 1k)",
            "psrr_db_at(V(out), vdd, 10k)",
            "gain_db_at(V(out), 1k)",
        ]
        .iter()
        .map(|t| Measure::parse(t).unwrap())
        .collect();
        assert_eq!(needed_sources(&ms), vec!["VDD", "Vcm"]);
    }
}

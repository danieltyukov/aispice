//! Parameter sweeps over a netlist, independent of the simulator.
//!
//! LTspice has `.step`, ngspice has `.control` loops, Xyce has `.STEP` with
//! its own rules, and Spectre has `sweep`. Rather than translate between
//! them, aispice writes one netlist per point and runs each on whatever
//! simulator is configured. That also lets the runner spread points over
//! cores and cache them individually.
//!
//! A target is an element name, whose value (the first word after its nodes,
//! or the word after `DC` on a source) is replaced, or a `.param` name, whose
//! assignment is rewritten. Element names win when both exist. Only top-level
//! lines are touched: values inside a `.subckt` belong to the subcircuit.

use crate::expr::format_number;
use aispice_core::netlist::{Element, Line, Netlist};
use aispice_core::units;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// More variants than this is almost always a mistake (a list where a
/// range was meant, or one parameter too many), so it is refused rather
/// than queued for hours of simulation.
pub const DEFAULT_MAX_VARIANTS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SweepParam {
    /// An element (`R1`, `C2`, `V1`) or a `.param` name.
    pub target: String,
    pub values: SweepValues,
}

/// The values to sweep: an explicit list, a range, or the text form
/// (`"1k, 2.2k, 4.7k"`, `"lin 1k 10k 10"`, `"dec 1k 1Meg 10"`,
/// `"oct 1k 8k 2"`; ranges are start, stop, count as in `.step`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SweepValues {
    List(Vec<f64>),
    Range(SweepRange),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SweepRange {
    /// `points` values evenly spaced from `start` to `stop`, both included.
    Lin {
        start: f64,
        stop: f64,
        points: usize,
    },
    /// Logarithmic, `points_per_decade` values per decade from `start`.
    Dec {
        start: f64,
        stop: f64,
        points_per_decade: usize,
    },
    /// Logarithmic, `points_per_octave` values per octave from `start`.
    Oct {
        start: f64,
        stop: f64,
        points_per_octave: usize,
    },
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SweepError {
    #[error("{}", unknown_target_message(.target, .elements, .params))]
    UnknownTarget {
        target: String,
        elements: Vec<String>,
        params: Vec<String>,
    },
    #[error(
        "the value of {target} is `{found}`, not a number; sweep a .param that it uses instead"
    )]
    NotNumeric { target: String, found: String },
    #[error("{0}")]
    BadValues(String),
    #[error("{count} variants is more than the limit of {limit}; use fewer values or parameters")]
    TooMany { count: usize, limit: usize },
    #[error("{0} is swept twice")]
    Duplicate(String),
}

fn unknown_target_message(target: &str, elements: &[String], params: &[String]) -> String {
    let mut s = format!("no element or .param named `{target}`");
    if !elements.is_empty() {
        s.push_str(&format!("; elements: {}", elements.join(", ")));
    }
    if !params.is_empty() {
        s.push_str(&format!("; parameters: {}", params.join(", ")));
    }
    s
}

impl SweepRange {
    pub fn values(&self) -> Result<Vec<f64>, SweepError> {
        let bad = |m: &str| Err(SweepError::BadValues(m.to_string()));
        match *self {
            SweepRange::Lin {
                start,
                stop,
                points,
            } => {
                if !start.is_finite() || !stop.is_finite() {
                    return bad("lin start and stop must be finite");
                }
                if points == 0 || points > DEFAULT_MAX_VARIANTS * 100 {
                    return bad("lin needs between 1 and 100000 points");
                }
                if points == 1 {
                    return Ok(vec![start]);
                }
                Ok((0..points)
                    .map(|i| {
                        if i == points - 1 {
                            stop
                        } else {
                            start + (stop - start) * i as f64 / (points - 1) as f64
                        }
                    })
                    .collect())
            }
            SweepRange::Dec {
                start,
                stop,
                points_per_decade: n,
            } => log_values(start, stop, n, 10.0, "dec"),
            SweepRange::Oct {
                start,
                stop,
                points_per_octave: n,
            } => log_values(start, stop, n, 2.0, "oct"),
        }
    }
}

/// `start * base^(k/n)` up to `stop`, as `.step dec` and `.step oct` do.
fn log_values(
    start: f64,
    stop: f64,
    n: usize,
    base: f64,
    name: &str,
) -> Result<Vec<f64>, SweepError> {
    if !(start > 0.0 && stop > 0.0 && start.is_finite() && stop.is_finite()) {
        return Err(SweepError::BadValues(format!(
            "{name} needs positive start and stop"
        )));
    }
    if stop < start {
        return Err(SweepError::BadValues(format!(
            "{name} stop ({}) is below start ({})",
            format_number(stop),
            format_number(start)
        )));
    }
    if n == 0 {
        return Err(SweepError::BadValues(format!(
            "{name} needs at least one point per step"
        )));
    }
    let steps = ((stop / start).ln() / base.ln() * n as f64 + 1e-9).floor();
    if steps > (DEFAULT_MAX_VARIANTS * 100) as f64 {
        return Err(SweepError::BadValues(format!(
            "{name} range has too many points"
        )));
    }
    Ok((0..=steps as usize)
        .map(|k| start * base.powf(k as f64 / n as f64))
        .collect())
}

impl SweepValues {
    pub fn values(&self) -> Result<Vec<f64>, SweepError> {
        let v = match self {
            SweepValues::List(v) => v.clone(),
            SweepValues::Range(r) => r.values()?,
            SweepValues::Text(t) => parse_values(t)?.values()?,
        };
        if v.is_empty() {
            return Err(SweepError::BadValues("no values to sweep".into()));
        }
        if let Some(x) = v.iter().find(|x| !x.is_finite()) {
            return Err(SweepError::BadValues(format!("{x} is not a finite value")));
        }
        Ok(v)
    }
}

/// Parse the text form of sweep values.
pub fn parse_values(text: &str) -> Result<SweepValues, SweepError> {
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .collect();
    let num = |w: &str| {
        crate::expr::parse_number(w)
            .ok_or_else(|| SweepError::BadValues(format!("`{w}` is not a number")))
    };
    let count = |w: &str| {
        w.parse::<usize>()
            .map_err(|_| SweepError::BadValues(format!("`{w}` is not a whole number of points")))
    };
    match words.first().map(|w| w.to_ascii_lowercase()) {
        Some(kind) if matches!(kind.as_str(), "lin" | "dec" | "oct") => {
            let [_, a, b, n] = words.as_slice() else {
                return Err(SweepError::BadValues(format!(
                    "`{kind}` takes start, stop and a point count, e.g. `{kind} 1k 10k 5`"
                )));
            };
            let (start, stop, n) = (num(a)?, num(b)?, count(n)?);
            Ok(SweepValues::Range(match kind.as_str() {
                "lin" => SweepRange::Lin {
                    start,
                    stop,
                    points: n,
                },
                "dec" => SweepRange::Dec {
                    start,
                    stop,
                    points_per_decade: n,
                },
                _ => SweepRange::Oct {
                    start,
                    stop,
                    points_per_octave: n,
                },
            }))
        }
        Some(_) => Ok(SweepValues::List(
            words.iter().map(|w| num(w)).collect::<Result<_, _>>()?,
        )),
        None => Err(SweepError::BadValues("no values to sweep".into())),
    }
}

/// Byte ranges of the words of an element's remainder, keeping
/// parenthesised and braced groups whole, as the netlist parser does.
pub(crate) fn word_spans(rest: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = None;
    for (i, c) in rest.char_indices() {
        match c {
            '(' | '{' | '[' => {
                depth += 1;
                start.get_or_insert(i);
            }
            ')' | '}' | ']' => {
                depth -= 1;
                start.get_or_insert(i);
            }
            c if c.is_whitespace() && depth <= 0 => {
                if let Some(s) = start.take() {
                    out.push((s, i));
                }
            }
            _ => {
                start.get_or_insert(i);
            }
        }
    }
    if let Some(s) = start {
        out.push((s, rest.len()));
    }
    out
}

/// The span of the value word in an element line, if it has one that can
/// be replaced by a number.
fn value_span(e: &Element) -> Result<(usize, usize), SweepError> {
    let spans = word_spans(&e.rest);
    let mut idx = 0;
    if matches!(e.letter(), 'V' | 'I')
        && spans
            .first()
            .is_some_and(|&(a, b)| e.rest[a..b].eq_ignore_ascii_case("dc"))
    {
        idx = 1;
    }
    let Some(&(a, b)) = spans.get(idx) else {
        return Err(SweepError::NotNumeric {
            target: e.name.clone(),
            found: e.rest.clone(),
        });
    };
    let word = &e.rest[a..b];
    if units::parse(word).is_some() || (word.starts_with('{') && word.ends_with('}')) {
        Ok((a, b))
    } else {
        Err(SweepError::NotNumeric {
            target: e.name.clone(),
            found: word.to_string(),
        })
    }
}

/// `.param` assignments in a directive, or `None` if it is not a `.param`
/// line or is written in a way this does not understand (it is then left
/// alone rather than mangled).
fn param_assignments(text: &str) -> Option<(String, Vec<(String, String)>)> {
    let t = text.trim();
    let kw_end = t.find(char::is_whitespace).unwrap_or(t.len());
    let kw = &t[..kw_end];
    if !kw.eq_ignore_ascii_case(".param") {
        return None;
    }
    let body: Vec<char> = t[kw_end..].chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    let skip_ws = |i: &mut usize| {
        while *i < body.len() && body[*i].is_whitespace() {
            *i += 1;
        }
    };
    loop {
        skip_ws(&mut i);
        if i >= body.len() {
            break;
        }
        let start = i;
        while i < body.len() && (body[i].is_alphanumeric() || body[i] == '_') {
            i += 1;
        }
        if i == start {
            return None;
        }
        let name: String = body[start..i].iter().collect();
        skip_ws(&mut i);
        if body.get(i) != Some(&'=') {
            return None;
        }
        i += 1;
        skip_ws(&mut i);
        let vstart = i;
        match body.get(i) {
            Some('{') => {
                let mut depth = 0;
                while i < body.len() {
                    match body[i] {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            Some('\'') => {
                i += 1;
                while i < body.len() && body[i] != '\'' {
                    i += 1;
                }
                i += 1;
            }
            Some(_) => {
                while i < body.len() && !body[i].is_whitespace() {
                    i += 1;
                }
            }
            None => return None,
        }
        let value: String = body[vstart..i.min(body.len())].iter().collect();
        if value.is_empty() {
            return None;
        }
        out.push((name, value));
    }
    (!out.is_empty()).then(|| (kw.to_string(), out))
}

fn param_names(netlist: &Netlist) -> Vec<String> {
    netlist
        .directives()
        .filter_map(param_assignments)
        .flat_map(|(_, a)| a.into_iter().map(|(n, _)| n))
        .collect()
}

fn unknown(netlist: &Netlist, target: &str) -> SweepError {
    SweepError::UnknownTarget {
        target: target.to_string(),
        elements: netlist.elements().map(|e| e.name.clone()).collect(),
        params: param_names(netlist),
    }
}

/// The current numeric value of an element or `.param`.
pub fn get_value(netlist: &Netlist, target: &str) -> Result<f64, SweepError> {
    if let Some(e) = netlist.element(target) {
        let (a, b) = value_span(e)?;
        let word = &e.rest[a..b];
        return crate::expr::parse_spice(word).ok_or_else(|| SweepError::NotNumeric {
            target: e.name.clone(),
            found: word.to_string(),
        });
    }
    for d in netlist.directives() {
        if let Some((_, assigns)) = param_assignments(d)
            && let Some((name, value)) =
                assigns.iter().find(|(n, _)| n.eq_ignore_ascii_case(target))
        {
            let v = value.trim_start_matches('{').trim_end_matches('}');
            return crate::expr::parse_spice(v).ok_or_else(|| SweepError::NotNumeric {
                target: name.clone(),
                found: value.clone(),
            });
        }
    }
    Err(unknown(netlist, target))
}

/// Set an element's value or a `.param`, writing the number so that it
/// reads back exactly.
pub fn set_value(netlist: &mut Netlist, target: &str, value: f64) -> Result<(), SweepError> {
    if !value.is_finite() {
        return Err(SweepError::BadValues(format!(
            "{value} is not a finite value"
        )));
    }
    let text = format_number(value);
    for item in &mut netlist.items {
        if let Line::Element(e) = item
            && e.name.eq_ignore_ascii_case(target)
        {
            let (a, b) = value_span(e)?;
            e.rest.replace_range(a..b, &text);
            return Ok(());
        }
    }
    for item in &mut netlist.items {
        if let Line::Directive { text: d } = item
            && let Some((kw, mut assigns)) = param_assignments(d)
            && let Some(slot) = assigns
                .iter_mut()
                .find(|(n, _)| n.eq_ignore_ascii_case(target))
        {
            slot.1 = text;
            let body: Vec<String> = assigns.iter().map(|(n, v)| format!("{n}={v}")).collect();
            *d = format!("{kw} {}", body.join(" "));
            return Ok(());
        }
    }
    Err(unknown(netlist, target))
}

/// One netlist per point of the sweep, with labels like `R1=2k C1=10n`.
/// Several parameters give their cartesian product, the last one varying
/// fastest. No parameters give the netlist itself, labelled `nominal`.
pub fn variants(
    netlist: &Netlist,
    params: &[SweepParam],
) -> Result<Vec<(String, Netlist)>, SweepError> {
    variants_with_limit(netlist, params, DEFAULT_MAX_VARIANTS)
}

pub fn variants_with_limit(
    netlist: &Netlist,
    params: &[SweepParam],
    limit: usize,
) -> Result<Vec<(String, Netlist)>, SweepError> {
    if params.is_empty() {
        return Ok(vec![("nominal".into(), netlist.clone())]);
    }
    for (i, p) in params.iter().enumerate() {
        if params[..i]
            .iter()
            .any(|q| q.target.eq_ignore_ascii_case(&p.target))
        {
            return Err(SweepError::Duplicate(p.target.clone()));
        }
        // Fail early, before generating anything, on a bad target.
        get_value(netlist, &p.target).or_else(|e| match e {
            SweepError::NotNumeric { .. } => {
                // A `{expr}` value can still be replaced.
                set_value(&mut netlist.clone(), &p.target, 0.0).map(|_| 0.0)
            }
            e => Err(e),
        })?;
    }
    let lists: Vec<Vec<f64>> = params
        .iter()
        .map(|p| p.values.values())
        .collect::<Result<_, _>>()?;
    let count = lists
        .iter()
        .try_fold(1usize, |acc, l| acc.checked_mul(l.len()))
        .unwrap_or(usize::MAX);
    if count > limit {
        return Err(SweepError::TooMany { count, limit });
    }
    let mut out = Vec::with_capacity(count);
    let mut idx = vec![0usize; lists.len()];
    loop {
        let mut n = netlist.clone();
        let mut label = Vec::with_capacity(lists.len());
        for (k, p) in params.iter().enumerate() {
            let v = lists[k][idx[k]];
            set_value(&mut n, &p.target, v)?;
            label.push(format!("{}={}", p.target, units::format(v)));
        }
        out.push((label.join(" "), n));
        // Odometer, last parameter fastest.
        let mut k = lists.len();
        loop {
            if k == 0 {
                return Ok(out);
            }
            k -= 1;
            idx[k] += 1;
            if idx[k] < lists[k].len() {
                break;
            }
            idx[k] = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aispice_core::netlist::{parse, write};

    const DECK: &str = "RC\nV1 in 0 DC 5 AC 1\nR1 in out 1k tc=0.001\nC1 out 0 {Cval}\nD1 out 0 1N4148\nV2 x 0 SINE(0 1 1k)\n.param Cval=100n Rload = 10k\n.param gain={2*Rload}\n.ac dec 10 1 1Meg\n.end\n";

    fn deck() -> Netlist {
        parse(DECK)
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-12 * b.abs()
    }

    #[test]
    fn values_from_ranges() {
        let lin = SweepRange::Lin {
            start: 1.0,
            stop: 2.0,
            points: 5,
        };
        assert_eq!(lin.values().unwrap(), vec![1.0, 1.25, 1.5, 1.75, 2.0]);
        let dec = parse_values("dec 1k 1Meg 3").unwrap().values().unwrap();
        assert_eq!(dec.len(), 10);
        assert!((dec[3] - 10e3).abs() < 1e-9 && (dec[9] - 1e6).abs() < 1e-6);
        let oct = parse_values("oct 1k 8k 1").unwrap().values().unwrap();
        assert_eq!(oct.len(), 4);
        assert!((oct[3] - 8e3).abs() < 1e-9);
        assert_eq!(
            parse_values("1k, 2.2k 4.7k").unwrap().values().unwrap(),
            vec![1e3, 2.2e3, 4.7e3]
        );
        assert_eq!(
            parse_values("lin 0 1 1").unwrap().values().unwrap(),
            vec![0.0]
        );
        for bad in [
            "",
            "dec 0 1k 3",
            "dec 1k 1 3",
            "lin 1 2",
            "lin 1 2 x",
            "1k, nope",
            "dec 1 10 0",
        ] {
            assert!(parse_values(bad).and_then(|v| v.values()).is_err(), "{bad}");
        }
        let json: SweepParam =
            serde_json::from_str(r#"{"target":"R1","values":[1000,2000]}"#).unwrap();
        assert_eq!(json.values.values().unwrap(), vec![1e3, 2e3]);
        let json: SweepParam = serde_json::from_str(
            r#"{"target":"R1","values":{"kind":"dec","start":1,"stop":100,"points_per_decade":1}}"#,
        )
        .unwrap();
        assert_eq!(json.values.values().unwrap().len(), 3);
        let json: SweepParam =
            serde_json::from_str(r#"{"target":"R1","values":"lin 1 3 3"}"#).unwrap();
        assert_eq!(json.values.values().unwrap(), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn reads_and_writes_values() {
        let mut n = deck();
        assert_eq!(get_value(&n, "R1").unwrap(), 1e3);
        assert_eq!(get_value(&n, "v1").unwrap(), 5.0);
        assert!(approx(get_value(&n, "Cval").unwrap(), 100e-9));
        assert_eq!(get_value(&n, "Rload").unwrap(), 10e3);
        assert!(matches!(
            get_value(&n, "C1"),
            Err(SweepError::NotNumeric { .. })
        ));
        assert!(matches!(
            get_value(&n, "gain"),
            Err(SweepError::NotNumeric { .. })
        ));
        assert!(matches!(
            get_value(&n, "D1"),
            Err(SweepError::NotNumeric { .. })
        ));
        let err = get_value(&n, "R9").unwrap_err().to_string();
        assert!(err.contains("elements: V1, R1, C1, D1, V2"), "{err}");
        assert!(err.contains("parameters: Cval, Rload, gain"), "{err}");

        set_value(&mut n, "R1", 2.2e3).unwrap();
        assert_eq!(n.element("R1").unwrap().rest, "2.2k tc=0.001");
        set_value(&mut n, "V1", 3.3).unwrap();
        assert_eq!(n.element("V1").unwrap().rest, "DC 3.3 AC 1");
        set_value(&mut n, "C1", 47e-9).unwrap();
        assert_eq!(n.element("C1").unwrap().rest, "47n");
        set_value(&mut n, "rload", 1234.5678).unwrap();
        assert!(
            write(&n).contains(".param Cval=100n Rload=1.2345678k\n"),
            "{}",
            write(&n)
        );
        assert_eq!(get_value(&n, "Rload").unwrap(), 1234.5678);
        assert!(matches!(
            set_value(&mut n, "V2", 1.0),
            Err(SweepError::NotNumeric { .. })
        ));
        assert!(set_value(&mut n, "R1", f64::NAN).is_err());
        // Everything else is untouched.
        assert!(write(&n).contains(".param gain={2*Rload}\n"));
        assert!(write(&n).contains("D1 out 0 1N4148\n"));
    }

    #[test]
    fn cartesian_product_with_labels() {
        let params = vec![
            SweepParam {
                target: "R1".into(),
                values: SweepValues::List(vec![1e3, 2e3]),
            },
            SweepParam {
                target: "Cval".into(),
                values: SweepValues::Text("10n 22n 47n".into()),
            },
        ];
        let v = variants(&deck(), &params).unwrap();
        assert_eq!(v.len(), 6);
        let labels: Vec<&str> = v.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "R1=1k Cval=10n",
                "R1=1k Cval=22n",
                "R1=1k Cval=47n",
                "R1=2k Cval=10n",
                "R1=2k Cval=22n",
                "R1=2k Cval=47n"
            ]
        );
        assert_eq!(get_value(&v[4].1, "R1").unwrap(), 2e3);
        assert!(approx(get_value(&v[4].1, "Cval").unwrap(), 22e-9));
        assert_eq!(variants(&deck(), &[]).unwrap()[0].0, "nominal");
        // Limits, duplicates and unknown targets.
        let big = vec![
            SweepParam {
                target: "R1".into(),
                values: SweepValues::Text("lin 1 100 100".into()),
            },
            SweepParam {
                target: "Cval".into(),
                values: SweepValues::Text("lin 1 100 100".into()),
            },
        ];
        assert_eq!(
            variants(&deck(), &big).unwrap_err(),
            SweepError::TooMany {
                count: 10_000,
                limit: DEFAULT_MAX_VARIANTS
            }
        );
        let dup = vec![params[0].clone(), params[0].clone()];
        assert!(matches!(
            variants(&deck(), &dup),
            Err(SweepError::Duplicate(_))
        ));
        let unknown = vec![SweepParam {
            target: "Q7".into(),
            values: SweepValues::List(vec![1.0]),
        }];
        assert!(matches!(
            variants(&deck(), &unknown),
            Err(SweepError::UnknownTarget { .. })
        ));
        // An element whose value is an expression can still be swept.
        let expr = vec![SweepParam {
            target: "C1".into(),
            values: SweepValues::List(vec![1e-9]),
        }];
        let v = variants(&deck(), &expr).unwrap();
        assert_eq!(v[0].1.element("C1").unwrap().rest, "1n");
    }

    #[test]
    fn odd_param_lines_are_left_alone() {
        let mut n = parse("t\nR1 a 0 1k\n.param\n.param x 5\n.param y='a+b' z=2\n.end\n");
        assert!(get_value(&n, "x").is_err());
        assert_eq!(get_value(&n, "z").unwrap(), 2.0);
        set_value(&mut n, "z", 3.0).unwrap();
        assert!(write(&n).contains(".param y='a+b' z=3\n"), "{}", write(&n));
        assert!(write(&n).contains(".param x 5\n"));
    }
}

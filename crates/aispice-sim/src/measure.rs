//! Named measurements on simulation results.
//!
//! Each measurement is one variant of [`Measure`], tagged by `kind` and
//! described with JSON Schema, so the agent can call them as tools and a spec
//! table can name them. They answer the questions a designer asks of a
//! waveform (how fast, how much ringing, where is the corner, how much phase
//! margin) with the textbook definitions, and when a question has no answer
//! (the signal never crosses, the window is empty, the data is NaN) they
//! return no value and say why in `note`. A measurement never panics and never
//! guesses.
//!
//! Values between samples are interpolated linearly in time, and linearly in
//! log frequency for AC data, which is how the response would be drawn on a
//! Bode plot and is far closer to the truth than linear frequency for a sweep
//! with a handful of points per decade.
//!
//! Stepped runs are measured step by step: the result carries one value per
//! step in `per_step`, labelled like the step (`R1=2k`).
//!
//! Measurements also have a compact text form for chat and spec files:
//! `gain_db_at(V(out)/V(in), 1k)`, `crossing(V(out), 2.5, rise, 2)`,
//! `min(V(out), to=1m)`. Positional arguments follow the field order of the
//! variant; any field can be given as `name=value`.

use crate::dataset::{AnalysisKind, Dataset};
use crate::expr::{self, Expr, Func, format_number, parse_number};
use aispice_core::units;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Range;

/// Which way a signal must be going for a crossing to count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Rise,
    Fall,
    #[default]
    Either,
}

/// What a -3 dB bandwidth is measured down from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BwReference {
    /// The gain at the lowest swept frequency: the lowpass convention.
    #[default]
    Dc,
    /// The peak gain anywhere in the sweep: right for bandpass and peaking
    /// responses, and reports both edges when both are in range.
    Peak,
}

fn d10() -> f64 {
    10.0
}
fn d90() -> f64 {
    90.0
}
fn d2() -> f64 {
    2.0
}
fn d50() -> f64 {
    50.0
}
fn one() -> i64 {
    1
}
fn nine() -> usize {
    9
}
fn one_usize() -> usize {
    1
}

/// A measurement to take on simulation results. Expressions use the syntax
/// of [`crate::expr`]: `V(out)`, `V(out)/V(in)`, `I(R1)`, `d(V(out))`.
/// Numbers may be JSON numbers or SPICE strings such as `"1k"` or `"10u"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Measure {
    /// Value of `expr` at one point of the independent variable (a time, or a
    /// frequency for AC data where the magnitude is reported).
    ValueAt {
        expr: String,
        #[serde(deserialize_with = "lenient::num")]
        at: f64,
    },
    /// Minimum of `expr`, and where it occurs, optionally within `from`..`to`.
    Min {
        expr: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Maximum of `expr`, and where it occurs, optionally within `from`..`to`.
    Max {
        expr: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Peak-to-peak: maximum minus minimum.
    Pp {
        expr: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Time-weighted average (trapezoidal), so unevenly spaced simulator
    /// points do not bias it toward busy regions.
    Avg {
        expr: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Root mean square, time-weighted.
    Rms {
        expr: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Integral over the independent variable (charge from a current,
    /// energy from a power).
    Integral {
        expr: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Where `expr` crosses `level`: the `nth` crossing (1 is the first, -1
    /// the last) in the given direction.
    Crossing {
        expr: String,
        #[serde(deserialize_with = "lenient::num")]
        level: f64,
        #[serde(default)]
        direction: Direction,
        #[serde(default = "one")]
        nth: i64,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Time to go from `low_pct` to `high_pct` of the way from the initial to
    /// the final value, on the first rising edge.
    RiseTime {
        expr: String,
        #[serde(default = "d10")]
        low_pct: f64,
        #[serde(default = "d90")]
        high_pct: f64,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Like `rise_time`, for a falling transition.
    FallTime {
        expr: String,
        #[serde(default = "d10")]
        low_pct: f64,
        #[serde(default = "d90")]
        high_pct: f64,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// How far the response goes past its final value, in percent of the
    /// step. With no net step (a load transient on a regulator) it is percent
    /// of the final value instead.
    OvershootPct {
        expr: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// How far the response goes the wrong way past its initial value, in
    /// percent of the step (MATLAB `stepinfo` convention). With no net step
    /// it is the dip below the final value, in percent of the final value.
    UndershootPct {
        expr: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Time from the start of the window until `expr` enters and stays
    /// within `tolerance_pct` of the step size around its final value.
    SettlingTime {
        expr: String,
        #[serde(default = "d2")]
        tolerance_pct: f64,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Time from `from_expr` crossing its `level_pct` point to `to_expr`
    /// crossing its own: propagation delay.
    Delay {
        from_expr: String,
        to_expr: String,
        #[serde(default = "d50")]
        level_pct: f64,
    },
    /// Frequency of a periodic signal, from its rising crossings of the mean.
    Frequency {
        expr: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Period of a periodic signal, from its rising crossings of the mean.
    Period {
        expr: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Percent of each period spent above the midpoint between minimum and
    /// maximum, over whole periods.
    DutyCycle {
        expr: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Total harmonic distortion in percent, from a DFT over the last
    /// `periods` whole periods of `fundamental` at the end of a transient.
    /// Harmonics 2 through `harmonics` are summed.
    Thd {
        expr: String,
        #[serde(deserialize_with = "lenient::num")]
        fundamental: f64,
        #[serde(default = "nine")]
        harmonics: usize,
        #[serde(default = "one_usize")]
        periods: usize,
    },
    /// Gain in dB at a frequency (AC).
    GainDbAt {
        expr: String,
        #[serde(deserialize_with = "lenient::num")]
        freq: f64,
    },
    /// Phase in degrees at a frequency (AC), unwrapped from the lowest
    /// frequency.
    PhaseAt {
        expr: String,
        #[serde(deserialize_with = "lenient::num")]
        freq: f64,
    },
    /// -3 dB bandwidth (AC). For a lowpass this is the corner frequency; with
    /// `reference: peak` and a bandpass response it is the distance between
    /// the two edges, which are reported too.
    #[serde(rename = "bandwidth_3db")]
    Bandwidth3db {
        expr: String,
        #[serde(default)]
        reference: BwReference,
    },
    /// Frequency where the gain falls through 0 dB (AC).
    UnityGainFreq { expr: String },
    /// Phase margin of a loop gain in degrees (AC): 180 plus the phase at the
    /// unity-gain frequency. A sign inversion in the probed quantity is
    /// detected from the low-frequency slope and removed.
    PhaseMargin { expr: String },
    /// Gain margin of a loop gain in dB (AC): how far below 0 dB the gain is
    /// where the phase reaches -180 degrees.
    GainMargin { expr: String },
    /// Highest gain in dB and the frequency where it occurs (AC).
    PeakGain { expr: String },
    /// First frequency where the gain crosses `db` (AC). The level is
    /// absolute: unlike bandwidth_3db it is not measured from the
    /// low-frequency gain.
    FreqAtDb {
        expr: String,
        #[serde(deserialize_with = "lenient::num")]
        db: f64,
    },
    /// How far the gain rises above its low-frequency value, in dB (AC): 0
    /// for a response with no peak, such as a Butterworth lowpass.
    PeakingDb { expr: String },
    /// Q of a second-order lowpass, estimated from an AC run: the gain,
    /// relative to the low-frequency gain, where the phase has turned 90
    /// degrees from its low-frequency value. Exact for a second-order
    /// lowpass (0.707 for Butterworth); also reports that frequency as f0.
    QLowpass { expr: String },
    /// Output noise density at a frequency, from a `.noise` analysis
    /// (V/√Hz).
    OutputNoiseAt {
        #[serde(deserialize_with = "lenient::num")]
        freq: f64,
    },
    /// Input-referred noise density at a frequency, from a `.noise`
    /// analysis (V/√Hz, or A/√Hz for a current source input).
    InputNoiseAt {
        #[serde(deserialize_with = "lenient::num")]
        freq: f64,
    },
    /// Integrated output noise, rms over `from`..`to` in Hz (default: the
    /// whole noise sweep).
    OutputNoiseRms {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Integrated input-referred noise, rms over `from`..`to` in Hz
    /// (default: the whole noise sweep).
    InputNoiseRms {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        from: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "lenient::opt"
        )]
        to: Option<f64>,
    },
    /// Power supply rejection in dB at a frequency: |expr| in the circuit's
    /// AC run (input driven with `AC 1`) over |expr| in a derived AC run with
    /// `AC 1` on the supply source `source` only. `expr` is the output.
    PsrrDbAt {
        expr: String,
        source: String,
        #[serde(deserialize_with = "lenient::num")]
        freq: f64,
    },
    /// Common-mode rejection in dB at a frequency: as `psrr_db_at`, with the
    /// derived run's `AC 1` on the common-mode source `source`.
    CmrrDbAt {
        expr: String,
        source: String,
        #[serde(deserialize_with = "lenient::num")]
        freq: f64,
    },
}

/// Accept numbers either as JSON numbers or as SPICE strings: agents send
/// `"1k"` as often as `1000`.
pub(crate) mod lenient {
    use serde::{Deserialize, Deserializer, de::Error};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Num {
        F(f64),
        S(String),
    }

    fn convert<E: Error>(n: Num) -> Result<f64, E> {
        match n {
            Num::F(v) => Ok(v),
            Num::S(s) => crate::expr::parse_number(&s)
                .ok_or_else(|| E::custom(format!("`{s}` is not a number"))),
        }
    }

    pub fn num<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        convert(Num::deserialize(d)?)
    }

    pub fn opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        Option::<Num>::deserialize(d)?.map(convert).transpose()
    }
}

/// A secondary number a measurement reports, such as the two edges of a
/// bandpass or the amplitude of each harmonic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NamedValue {
    pub name: String,
    pub value: f64,
    pub unit: String,
}

/// The measurement of one step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StepValue {
    /// The step's label (`R1=2k`), empty for an unstepped run.
    pub label: String,
    pub value: Option<f64>,
    pub at: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<NamedValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MeasureResult {
    pub name: String,
    /// The value for an unstepped run. `None` when there is no answer (see
    /// `note`) and for a stepped run, whose values are in `per_step`.
    pub value: Option<f64>,
    pub unit: String,
    /// Where the value was found or taken (a time or frequency), when that
    /// means something: the location of a maximum, the frequency of a gain.
    pub at: Option<f64>,
    /// Unit of `at`: the independent variable's (`s`, `Hz`).
    pub at_unit: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<NamedValue>,
    /// One entry per step; a single entry for an unstepped run.
    pub per_step: Vec<StepValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl MeasureResult {
    fn failed(name: &str, note: String) -> Self {
        MeasureResult {
            name: name.to_string(),
            value: None,
            unit: String::new(),
            at: None,
            at_unit: String::new(),
            extra: Vec::new(),
            per_step: Vec::new(),
            note: Some(note),
        }
    }

    /// One value per step, in step order.
    pub fn values(&self) -> Vec<Option<f64>> {
        self.per_step.iter().map(|s| s.value).collect()
    }

    /// A short line for people: `bw = 1.592kHz`, `vmax = 1.163V at 3.6ms`,
    /// `rise = n/a (never crosses the 90% level)`.
    /// The value alone, for a table beside the name: `1.591 kHz`, or one
    /// value per step of a swept run.
    pub fn value_display(&self) -> String {
        if self.per_step.len() > 1 {
            return self
                .per_step
                .iter()
                .map(|s| {
                    let v = s
                        .value
                        .map_or_else(|| "n/a".into(), |v| spaced(v, &self.unit));
                    if s.label.is_empty() {
                        v
                    } else {
                        format!("{v} ({})", s.label)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
        }
        match self.value {
            Some(v) => spaced(v, &self.unit),
            None => "n/a".into(),
        }
    }

    pub fn display(&self) -> String {
        if self.per_step.len() > 1 {
            let parts: Vec<String> = self
                .per_step
                .iter()
                .map(|s| {
                    let v = match s.value {
                        Some(v) => human(v, &self.unit),
                        None => "n/a".into(),
                    };
                    if s.label.is_empty() {
                        v
                    } else {
                        format!("{v} ({})", s.label)
                    }
                })
                .collect();
            return format!("{}: {}", self.name, parts.join(", "));
        }
        let Some(v) = self.value else {
            return format!(
                "{} = n/a ({})",
                self.name,
                self.note.as_deref().unwrap_or("no value")
            );
        };
        let mut s = format!("{} = {}", self.name, human(v, &self.unit));
        if let Some(at) = self.at {
            s.push_str(&format!(" at {}", human(at, &self.at_unit)));
        }
        if !self.extra.is_empty() {
            let parts: Vec<String> = self
                .extra
                .iter()
                .take(3)
                .map(|e| format!("{} {}", e.name, human(e.value, &e.unit)))
                .collect();
            s.push_str(&format!(" ({})", parts.join(", ")));
        }
        s
    }
}

impl fmt::Display for MeasureResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// Format a value for people. Decibels, percent and degrees get plain
/// decimals, since `500mdB` helps nobody; everything else gets an SI suffix.
/// `human` with a space before the unit and `µ` for micro, for tables that
/// show the value beside its name: `1.591 kHz`, `-3.01 dB`.
pub fn spaced(v: f64, unit: &str) -> String {
    if !v.is_finite() || unit.is_empty() {
        return human(v, unit);
    }
    if matches!(unit, "dB" | "%" | "°") {
        return format!("{} {unit}", plain(v));
    }
    let text = units::format(v).replace("Meg", "M");
    let split = text
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let prefix = &text[split..];
    let prefix = if prefix == "u" { "µ" } else { prefix };
    format!("{} {prefix}{unit}", &text[..split])
}

pub fn human(v: f64, unit: &str) -> String {
    if !v.is_finite() {
        return format!("{v}{unit}");
    }
    if matches!(unit, "dB" | "%" | "°") {
        return format!("{}{unit}", plain(v));
    }
    units::format_with_unit(v, unit)
}

/// Four significant digits, trailing zeros removed.
pub(crate) fn plain(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let mag = v.abs().log10().floor() as i32;
    let decimals = (3 - mag).clamp(0, 12) as usize;
    let mut s = format!("{v:.decimals$}");
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    if s == "-0" { "0".into() } else { s }
}

/// What kind of analysis a measurement needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Domain {
    Any,
    /// A real independent variable: transient or DC sweep.
    Time,
    Transient,
    Ac,
    Noise,
}

impl Domain {
    /// Whether a dataset can answer the measurement. A noise analysis also
    /// has a frequency axis, so it is kept apart from AC; datasets of a
    /// derived run (see [`crate::stimulus`]) only answer rejection
    /// measurements, which find them on their own.
    fn accepts(self, ds: &Dataset) -> bool {
        if crate::stimulus::marked_source(ds).is_some() {
            return false;
        }
        match self {
            Domain::Any => true,
            Domain::Time => ds.axis.is_some() && !expr::is_log_axis(ds),
            Domain::Transient => ds.kind == AnalysisKind::Transient,
            Domain::Ac => {
                ds.kind != AnalysisKind::Noise && ds.axis.is_some() && expr::is_log_axis(ds)
            }
            Domain::Noise => ds.kind == AnalysisKind::Noise && ds.axis.is_some(),
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Domain::Any => "a simulation result",
            Domain::Time => "a transient or DC sweep",
            Domain::Transient => "a transient analysis",
            Domain::Ac => "an AC analysis",
            Domain::Noise => "a noise analysis (.noise)",
        }
    }
}

fn kind_name(k: AnalysisKind) -> &'static str {
    match k {
        AnalysisKind::Transient => "transient",
        AnalysisKind::Ac => "AC",
        AnalysisKind::Dc => "DC sweep",
        AnalysisKind::Op => "operating point",
        AnalysisKind::Noise => "noise",
        AnalysisKind::TransferFunction => "transfer function",
        AnalysisKind::PoleZero => "pole-zero",
        AnalysisKind::Other => "other",
    }
}

impl Measure {
    /// The `kind` tag, as in JSON and the text form.
    pub fn kind(&self) -> &'static str {
        match self {
            Measure::ValueAt { .. } => "value_at",
            Measure::Min { .. } => "min",
            Measure::Max { .. } => "max",
            Measure::Pp { .. } => "pp",
            Measure::Avg { .. } => "avg",
            Measure::Rms { .. } => "rms",
            Measure::Integral { .. } => "integral",
            Measure::Crossing { .. } => "crossing",
            Measure::RiseTime { .. } => "rise_time",
            Measure::FallTime { .. } => "fall_time",
            Measure::OvershootPct { .. } => "overshoot_pct",
            Measure::UndershootPct { .. } => "undershoot_pct",
            Measure::SettlingTime { .. } => "settling_time",
            Measure::Delay { .. } => "delay",
            Measure::Frequency { .. } => "frequency",
            Measure::Period { .. } => "period",
            Measure::DutyCycle { .. } => "duty_cycle",
            Measure::Thd { .. } => "thd",
            Measure::GainDbAt { .. } => "gain_db_at",
            Measure::PhaseAt { .. } => "phase_at",
            Measure::Bandwidth3db { .. } => "bandwidth_3db",
            Measure::UnityGainFreq { .. } => "unity_gain_freq",
            Measure::PhaseMargin { .. } => "phase_margin",
            Measure::GainMargin { .. } => "gain_margin",
            Measure::PeakGain { .. } => "peak_gain",
            Measure::FreqAtDb { .. } => "freq_at_db",
            Measure::PeakingDb { .. } => "peaking_db",
            Measure::QLowpass { .. } => "q_lowpass",
            Measure::OutputNoiseAt { .. } => "output_noise_at",
            Measure::InputNoiseAt { .. } => "input_noise_at",
            Measure::OutputNoiseRms { .. } => "output_noise_rms",
            Measure::InputNoiseRms { .. } => "input_noise_rms",
            Measure::PsrrDbAt { .. } => "psrr_db_at",
            Measure::CmrrDbAt { .. } => "cmrr_db_at",
        }
    }

    /// The source a rejection measurement moves the AC stimulus to.
    pub fn stimulus_source(&self) -> Option<&str> {
        match self {
            Measure::PsrrDbAt { source, .. } | Measure::CmrrDbAt { source, .. } => Some(source),
            _ => None,
        }
    }

    /// The expressions the measurement reads, in field order.
    pub fn exprs(&self) -> Vec<&str> {
        match self {
            Measure::Delay {
                from_expr, to_expr, ..
            } => vec![from_expr, to_expr],
            Measure::ValueAt { expr, .. }
            | Measure::Min { expr, .. }
            | Measure::Max { expr, .. }
            | Measure::Pp { expr, .. }
            | Measure::Avg { expr, .. }
            | Measure::Rms { expr, .. }
            | Measure::Integral { expr, .. }
            | Measure::Crossing { expr, .. }
            | Measure::RiseTime { expr, .. }
            | Measure::FallTime { expr, .. }
            | Measure::OvershootPct { expr, .. }
            | Measure::UndershootPct { expr, .. }
            | Measure::SettlingTime { expr, .. }
            | Measure::Frequency { expr, .. }
            | Measure::Period { expr, .. }
            | Measure::DutyCycle { expr, .. }
            | Measure::Thd { expr, .. }
            | Measure::GainDbAt { expr, .. }
            | Measure::PhaseAt { expr, .. }
            | Measure::Bandwidth3db { expr, .. }
            | Measure::UnityGainFreq { expr }
            | Measure::PhaseMargin { expr }
            | Measure::GainMargin { expr }
            | Measure::PeakGain { expr }
            | Measure::PeakingDb { expr }
            | Measure::QLowpass { expr }
            | Measure::FreqAtDb { expr, .. }
            | Measure::PsrrDbAt { expr, .. }
            | Measure::CmrrDbAt { expr, .. } => vec![expr],
            Measure::OutputNoiseAt { .. } | Measure::OutputNoiseRms { .. } => {
                vec![crate::noise::ONOISE]
            }
            Measure::InputNoiseAt { .. } | Measure::InputNoiseRms { .. } => {
                vec![crate::noise::INOISE]
            }
        }
    }

    fn domain(&self) -> Domain {
        match self {
            Measure::ValueAt { .. }
            | Measure::Min { .. }
            | Measure::Max { .. }
            | Measure::Pp { .. }
            | Measure::Avg { .. }
            | Measure::Rms { .. }
            | Measure::Integral { .. }
            | Measure::Crossing { .. } => Domain::Any,
            Measure::RiseTime { .. }
            | Measure::FallTime { .. }
            | Measure::OvershootPct { .. }
            | Measure::UndershootPct { .. }
            | Measure::SettlingTime { .. }
            | Measure::Delay { .. }
            | Measure::Frequency { .. }
            | Measure::Period { .. }
            | Measure::DutyCycle { .. } => Domain::Time,
            Measure::Thd { .. } => Domain::Transient,
            Measure::GainDbAt { .. }
            | Measure::PhaseAt { .. }
            | Measure::Bandwidth3db { .. }
            | Measure::UnityGainFreq { .. }
            | Measure::PhaseMargin { .. }
            | Measure::GainMargin { .. }
            | Measure::PeakGain { .. }
            | Measure::PeakingDb { .. }
            | Measure::QLowpass { .. }
            | Measure::FreqAtDb { .. }
            | Measure::PsrrDbAt { .. }
            | Measure::CmrrDbAt { .. } => Domain::Ac,
            Measure::OutputNoiseAt { .. }
            | Measure::InputNoiseAt { .. }
            | Measure::OutputNoiseRms { .. }
            | Measure::InputNoiseRms { .. } => Domain::Noise,
        }
    }

    /// Whether the measurement is in decibels, degrees or percent, which
    /// matters for how spec margins are normalised.
    pub fn unit_hint(&self) -> Option<&'static str> {
        match self {
            Measure::GainDbAt { .. }
            | Measure::PeakGain { .. }
            | Measure::PeakingDb { .. }
            | Measure::GainMargin { .. }
            | Measure::PsrrDbAt { .. }
            | Measure::CmrrDbAt { .. } => Some("dB"),
            Measure::PhaseAt { .. } | Measure::PhaseMargin { .. } => Some("°"),
            Measure::OvershootPct { .. }
            | Measure::UndershootPct { .. }
            | Measure::DutyCycle { .. }
            | Measure::Thd { .. } => Some("%"),
            Measure::Frequency { .. }
            | Measure::Bandwidth3db { .. }
            | Measure::UnityGainFreq { .. }
            | Measure::FreqAtDb { .. } => Some("Hz"),
            _ => None,
        }
    }

    fn units(&self, e: &Expr, ds: &Dataset) -> (String, String) {
        let axis = ds
            .axis_vector()
            .map(|v| v.quantity.unit())
            .unwrap_or("")
            .to_string();
        let value = match self {
            Measure::ValueAt { .. }
            | Measure::Min { .. }
            | Measure::Max { .. }
            | Measure::Pp { .. }
            | Measure::Avg { .. }
            | Measure::Rms { .. } => e.unit(ds),
            Measure::Integral { .. } => format!("{}{axis}", e.unit(ds)),
            Measure::OutputNoiseAt { .. } | Measure::InputNoiseAt { .. } => {
                format!("{}/√Hz", e.unit(ds))
            }
            Measure::OutputNoiseRms { .. } | Measure::InputNoiseRms { .. } => e.unit(ds),
            Measure::Crossing { .. }
            | Measure::RiseTime { .. }
            | Measure::FallTime { .. }
            | Measure::SettlingTime { .. }
            | Measure::Delay { .. }
            | Measure::Period { .. } => axis.clone(),
            m => m.unit_hint().unwrap_or("").to_string(),
        };
        (value, axis)
    }

    /// Parse the text form, e.g. `rise_time(V(out), 10, 90)`.
    pub fn parse(text: &str) -> Result<Measure, MeasureParseError> {
        parse_call(text)
    }
}

/// Take a measurement on the results of a run. AC measurements use the first
/// AC dataset, time-domain ones the first transient or DC sweep, and the
/// general ones (`min`, `max`, `value_at`, ...) the first dataset that has
/// every vector the expression names.
pub fn measure(name: &str, m: &Measure, datasets: &[Dataset]) -> MeasureResult {
    let mut parsed = Vec::new();
    for text in m.exprs() {
        match expr::parse(text) {
            Ok(e) => parsed.push(e),
            Err(e) => return MeasureResult::failed(name, e.to_string()),
        }
    }
    if let Some(source) = m.stimulus_source() {
        return rejection(name, m, &parsed[0], source, datasets);
    }
    match pick(m, &parsed, datasets) {
        Ok(ds) => run(name, m, &parsed, ds),
        Err(note) => MeasureResult::failed(name, note),
    }
}

/// PSRR or CMRR: the gain of `e` in the circuit's AC run over its gain in
/// the derived run with `AC 1` on `source`, in dB, step by step.
fn rejection(
    name: &str,
    m: &Measure,
    e: &Expr,
    source: &str,
    datasets: &[Dataset],
) -> MeasureResult {
    let (Measure::PsrrDbAt { freq, .. } | Measure::CmrrDbAt { freq, .. }) = m else {
        return MeasureResult::failed(name, format!("{} is not a rejection measurement", m.kind()));
    };
    let main = match pick(m, std::slice::from_ref(e), datasets) {
        Ok(ds) => ds,
        Err(note) => return MeasureResult::failed(name, note),
    };
    let Some(derived) = datasets.iter().find(|d| {
        crate::stimulus::marked_source(d).is_some_and(|s| s.eq_ignore_ascii_case(source))
            && e.eval(d, 0..0).is_ok()
    }) else {
        return MeasureResult::failed(
            name,
            format!(
                "{} needs a second AC run with `AC 1` on {source}, which aispice adds when it simulates for this measurement; these results do not have it",
                m.kind()
            ),
        );
    };
    let ranges = |ds: &Dataset| -> Vec<(String, Range<usize>)> {
        if ds.steps.is_empty() {
            vec![(String::new(), 0..ds.len())]
        } else {
            ds.steps
                .iter()
                .map(|s| (s.label.clone(), s.range.clone()))
                .collect()
        }
    };
    let (main_steps, derived_steps) = (ranges(main), ranges(derived));
    let per_step: Vec<StepValue> = main_steps
        .iter()
        .enumerate()
        .map(|(i, (label, range))| {
            let other = derived_steps
                .get(i)
                .or_else(|| (derived_steps.len() == 1).then(|| &derived_steps[0]));
            let o = match other {
                None => Outcome::none(format!("the run with AC on {source} has no step {}", i + 1)),
                Some((_, r2)) => {
                    rejection_step(e, main, range.clone(), derived, r2.clone(), *freq, source)
                }
            }
            .finish();
            StepValue {
                label: label.clone(),
                value: o.value,
                at: o.at,
                extra: o.extra,
                note: o.note,
            }
        })
        .collect();
    let mut result = MeasureResult {
        name: name.to_string(),
        value: None,
        unit: "dB".into(),
        at: None,
        at_unit: "Hz".into(),
        extra: Vec::new(),
        per_step,
        note: None,
    };
    if let [only] = result.per_step.as_slice() {
        result.value = only.value;
        result.at = only.at;
        result.extra = only.extra.clone();
        result.note = only.note.clone();
    }
    result
}

fn rejection_step(
    e: &Expr,
    main: &Dataset,
    range: Range<usize>,
    derived: &Dataset,
    derived_range: Range<usize>,
    freq: f64,
    source: &str,
) -> Outcome {
    let (a, b) = match (
        signal(e, main, range, Mode::Db),
        signal(e, derived, derived_range, Mode::Db),
    ) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(note), _) | (_, Err(note)) => return Outcome::none(note),
    };
    let at = |s: &Sig| num::interp(&s.x, &s.y, freq, true);
    match (at(&a), at(&b)) {
        (Some(_), Some(gb)) if gb == f64::NEG_INFINITY => Outcome::none(format!(
            "the output does not respond to {source} at all (infinite rejection)"
        )),
        (Some(ga), Some(gb)) => Outcome::of(ga - gb)
            .at(freq)
            .with("signal_gain", ga, "dB")
            .with(&format!("gain_from_{source}"), gb, "dB"),
        _ => Outcome::none(format!(
            "{} is outside the swept range {}..{}",
            human(freq, "Hz"),
            human(a.x[0], "Hz"),
            human(a.x[a.x.len() - 1], "Hz")
        )),
    }
}

/// [`measure`] on a single dataset.
pub fn measure_dataset(name: &str, m: &Measure, ds: &Dataset) -> MeasureResult {
    measure(name, m, std::slice::from_ref(ds))
}

fn pick<'a>(m: &Measure, exprs: &[Expr], datasets: &'a [Dataset]) -> Result<&'a Dataset, String> {
    let domain = m.domain();
    let mut first_err = None;
    let mut any = false;
    // Measurements that accept any analysis (avg, pp, min, max, rms...) are
    // meant for waveforms, so a run with both a transient and an AC analysis
    // measures the transient one, whatever order the simulator wrote them in.
    let mut candidates: Vec<&Dataset> = datasets.iter().filter(|d| domain.accepts(d)).collect();
    if domain == Domain::Any {
        candidates.sort_by_key(|d| match d.kind {
            AnalysisKind::Transient => 0,
            AnalysisKind::Dc => 1,
            AnalysisKind::Op => 2,
            _ => 3,
        });
    }
    for ds in candidates {
        any = true;
        match exprs.iter().try_for_each(|e| e.eval(ds, 0..0).map(|_| ())) {
            Ok(()) => return Ok(ds),
            Err(e) => {
                first_err.get_or_insert(e.to_string());
            }
        }
    }
    if any {
        return Err(first_err.unwrap_or_default());
    }
    let have: Vec<&str> = datasets.iter().map(|d| kind_name(d.kind)).collect();
    Err(if have.is_empty() {
        format!(
            "{} needs {}, and there are no results",
            m.kind(),
            domain.describe()
        )
    } else {
        format!(
            "{} needs {}; the results contain: {}",
            m.kind(),
            domain.describe(),
            have.join(", ")
        )
    })
}

fn run(name: &str, m: &Measure, exprs: &[Expr], ds: &Dataset) -> MeasureResult {
    let (unit, at_unit) = m.units(&exprs[0], ds);
    let steps: Vec<(String, Range<usize>)> = if ds.steps.is_empty() {
        vec![(String::new(), 0..ds.len())]
    } else {
        ds.steps
            .iter()
            .map(|s| (s.label.clone(), s.range.clone()))
            .collect()
    };
    let ctx = Units {
        axis: at_unit.clone(),
        expr: exprs[0].unit(ds),
    };
    let per_step: Vec<StepValue> = steps
        .into_iter()
        .map(|(label, range)| {
            let o = step(m, exprs, ds, range, &ctx).finish();
            StepValue {
                label,
                value: o.value,
                at: o.at,
                extra: o.extra,
                note: o.note,
            }
        })
        .collect();
    let mut result = MeasureResult {
        name: name.to_string(),
        value: None,
        unit,
        at: None,
        at_unit,
        extra: Vec::new(),
        per_step,
        note: None,
    };
    if let [only] = result.per_step.as_slice() {
        result.value = only.value;
        result.at = only.at;
        result.extra = only.extra.clone();
        result.note = only.note.clone();
    } else {
        let notes: Vec<&str> = result
            .per_step
            .iter()
            .filter_map(|s| s.note.as_deref())
            .collect();
        if !notes.is_empty()
            && notes.len() == result.per_step.len()
            && notes.iter().all(|n| *n == notes[0])
        {
            result.note = Some(format!("every step: {}", notes[0]));
        }
    }
    result
}

struct Units {
    /// Unit of the independent variable.
    axis: String,
    /// Unit of the expression's values, for levels and amplitudes in notes.
    expr: String,
}

#[derive(Debug, Default)]
struct Outcome {
    value: Option<f64>,
    at: Option<f64>,
    extra: Vec<NamedValue>,
    note: Option<String>,
}

impl Outcome {
    fn of(v: f64) -> Self {
        Outcome {
            value: Some(v),
            ..Default::default()
        }
    }
    fn none(note: impl Into<String>) -> Self {
        Outcome {
            note: Some(note.into()),
            ..Default::default()
        }
    }
    fn at(mut self, x: f64) -> Self {
        self.at = Some(x);
        self
    }
    fn with(mut self, name: &str, value: f64, unit: &str) -> Self {
        if value.is_finite() {
            self.extra.push(NamedValue {
                name: name.into(),
                value,
                unit: unit.into(),
            });
        }
        self
    }
    fn note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
    /// A NaN or infinite answer is no answer.
    fn finish(mut self) -> Self {
        if let Some(v) = self.value
            && !v.is_finite()
        {
            self.value = None;
            self.note.get_or_insert_with(|| {
                format!("the result is not a finite number ({v}); the data may contain NaN or a division by zero")
            });
        }
        if self.at.is_some_and(|a| !a.is_finite()) {
            self.at = None;
        }
        self
    }
}

macro_rules! tryo {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(note) => return Outcome::none(note),
        }
    };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Real values, or magnitude for complex data.
    Value,
    Db,
    Phase,
}

/// A real signal over an ascending independent variable.
struct Sig {
    x: Vec<f64>,
    y: Vec<f64>,
    log: bool,
}

fn signal(e: &Expr, ds: &Dataset, range: Range<usize>, mode: Mode) -> Result<Sig, String> {
    if range.is_empty() {
        return Err("this step has no points".into());
    }
    let series = e.eval(ds, range.clone()).map_err(|e| e.to_string())?;
    let mut y = match mode {
        Mode::Value => series.real_or_magnitude(),
        Mode::Db if e.outer_func() == Some(Func::Db) => series.real_or_magnitude(),
        Mode::Db => series.db(),
        Mode::Phase if e.outer_func() == Some(Func::Ph) => series.real_or_magnitude(),
        Mode::Phase => series.phase_deg(),
    };
    let mut x = match ds.axis_vector() {
        Some(v) => {
            let all = v.data.real();
            if all.len() < range.end {
                return Err("the independent variable is shorter than the data".into());
            }
            all[range].to_vec()
        }
        None => (0..y.len()).map(|i| i as f64).collect(),
    };
    if x.len() > 1 && x[0] > x[x.len() - 1] {
        x.reverse();
        y.reverse();
    }
    if !y.iter().any(|v| v.is_finite()) {
        return Err("the expression has no finite values here".into());
    }
    Ok(Sig {
        x,
        y,
        log: expr::is_log_axis(ds),
    })
}

fn step(m: &Measure, exprs: &[Expr], ds: &Dataset, range: Range<usize>, u: &Units) -> Outcome {
    let e = &exprs[0];
    let value_sig = || signal(e, ds, range.clone(), Mode::Value);
    let windowed = |from: &Option<f64>, to: &Option<f64>| -> Result<Sig, String> {
        let s = value_sig()?;
        let (x, y) = num::window(&s.x, &s.y, *from, *to, s.log, &u.axis)?;
        Ok(Sig { x, y, log: s.log })
    };
    match m {
        Measure::ValueAt { at, .. } => {
            let s = tryo!(value_sig());
            if ds.axis.is_none() {
                return Outcome::of(s.y[0]);
            }
            match num::interp(&s.x, &s.y, *at, s.log) {
                Some(v) => Outcome::of(v).at(*at),
                None => Outcome::none(format!(
                    "{} is outside the simulated range {}..{}",
                    human(*at, &u.axis),
                    human(s.x[0], &u.axis),
                    human(s.x[s.x.len() - 1], &u.axis)
                )),
            }
        }
        Measure::Min { from, to, .. } | Measure::Max { from, to, .. } => {
            let s = tryo!(windowed(from, to));
            let Some(ex) = num::extremes(&s.x, &s.y) else {
                return Outcome::none("no finite values in the window");
            };
            if matches!(m, Measure::Min { .. }) {
                Outcome::of(ex.min).at(ex.min_at)
            } else {
                Outcome::of(ex.max).at(ex.max_at)
            }
        }
        Measure::Pp { from, to, .. } => {
            let s = tryo!(windowed(from, to));
            let Some(ex) = num::extremes(&s.x, &s.y) else {
                return Outcome::none("no finite values in the window");
            };
            Outcome::of(ex.max - ex.min)
                .with("min", ex.min, &u.expr)
                .with("max", ex.max, &u.expr)
        }
        Measure::Avg { from, to, .. } | Measure::Rms { from, to, .. } => {
            let s = tryo!(windowed(from, to));
            let rms = matches!(m, Measure::Rms { .. });
            let span = s.x[s.x.len() - 1] - s.x[0];
            if s.x.len() == 1 || span == 0.0 {
                let v = s.y[0];
                return Outcome::of(if rms { v.abs() } else { v });
            }
            let integral = if rms {
                num::trapz_sq(&s.x, &s.y)
            } else {
                num::trapz(&s.x, &s.y)
            };
            match integral {
                Some(i) if rms => Outcome::of((i / span).sqrt()),
                Some(i) => Outcome::of(i / span),
                None => Outcome::none("no finite values in the window"),
            }
        }
        Measure::Integral { from, to, .. } => {
            let s = tryo!(windowed(from, to));
            match num::trapz(&s.x, &s.y) {
                Some(i) => Outcome::of(i),
                None if s.x.len() == 1 => Outcome::of(0.0),
                None => Outcome::none("no finite values in the window"),
            }
        }
        Measure::Crossing {
            level,
            direction,
            nth,
            from,
            to,
            ..
        } => {
            let s = tryo!(windowed(from, to));
            crossing(&s, *level, *direction, *nth, u)
        }
        Measure::RiseTime {
            low_pct,
            high_pct,
            from,
            to,
            ..
        }
        | Measure::FallTime {
            low_pct,
            high_pct,
            from,
            to,
            ..
        } => {
            let s = tryo!(windowed(from, to));
            let want = if matches!(m, Measure::RiseTime { .. }) {
                num::Edge::Rise
            } else {
                num::Edge::Fall
            };
            edge_time(&s, *low_pct, *high_pct, want, u)
        }
        Measure::OvershootPct { from, to, .. } | Measure::UndershootPct { from, to, .. } => {
            let s = tryo!(windowed(from, to));
            shoot(&s, matches!(m, Measure::OvershootPct { .. }))
        }
        Measure::SettlingTime {
            tolerance_pct,
            from,
            to,
            ..
        } => {
            let s = tryo!(windowed(from, to));
            settling(&s, *tolerance_pct, u)
        }
        Measure::Delay { level_pct, .. } => {
            let a = tryo!(signal(&exprs[0], ds, range.clone(), Mode::Value));
            let b = tryo!(signal(&exprs[1], ds, range.clone(), Mode::Value));
            delay(&a, &b, *level_pct, u)
        }
        Measure::Frequency { from, to, .. } | Measure::Period { from, to, .. } => {
            let s = tryo!(windowed(from, to));
            let (period, cycles, t0) = tryo!(period_of(&s, u));
            let o = if matches!(m, Measure::Frequency { .. }) {
                Outcome::of(1.0 / period)
            } else {
                Outcome::of(period)
            };
            o.with("cycles", cycles as f64, "")
                .with("first_edge", t0, &u.axis)
        }
        Measure::DutyCycle { from, to, .. } => {
            let s = tryo!(windowed(from, to));
            duty(&s, u)
        }
        Measure::Thd {
            fundamental,
            harmonics,
            periods,
            ..
        } => {
            let s = tryo!(value_sig());
            thd(&s, *fundamental, *harmonics, *periods, u)
        }
        Measure::GainDbAt { freq, .. } | Measure::PhaseAt { freq, .. } => {
            let mode = if matches!(m, Measure::GainDbAt { .. }) {
                Mode::Db
            } else {
                Mode::Phase
            };
            let s = tryo!(signal(e, ds, range, mode));
            match num::interp(&s.x, &s.y, *freq, true) {
                Some(v) => Outcome::of(v).at(*freq),
                None => Outcome::none(format!(
                    "{} is outside the swept range {}..{}",
                    human(*freq, "Hz"),
                    human(s.x[0], "Hz"),
                    human(s.x[s.x.len() - 1], "Hz")
                )),
            }
        }
        Measure::Bandwidth3db { reference, .. } => {
            let s = tryo!(signal(e, ds, range, Mode::Db));
            bandwidth(&s, *reference)
        }
        Measure::UnityGainFreq { .. } => {
            let s = tryo!(signal(e, ds, range, Mode::Db));
            match unity_gain(&s) {
                Ok(f) => Outcome::of(f),
                Err(note) => Outcome::none(note),
            }
        }
        Measure::PhaseMargin { .. } | Measure::GainMargin { .. } => {
            let db = tryo!(signal(e, ds, range.clone(), Mode::Db));
            let ph = tryo!(signal(e, ds, range, Mode::Phase));
            if matches!(m, Measure::PhaseMargin { .. }) {
                phase_margin(&db, &ph)
            } else {
                gain_margin(&db, &ph)
            }
        }
        Measure::PeakGain { .. } => {
            let s = tryo!(signal(e, ds, range, Mode::Db));
            match num::extremes(&s.x, &s.y) {
                Some(ex) => Outcome::of(ex.max).at(ex.max_at),
                None => Outcome::none("no finite gain values"),
            }
        }
        Measure::PeakingDb { .. } => {
            let s = tryo!(signal(e, ds, range, Mode::Db));
            match (num::first_last_finite(&s.y), num::extremes(&s.x, &s.y)) {
                (Some((low, _)), Some(ex)) => Outcome::of((ex.max - low).max(0.0)).at(ex.max_at),
                _ => Outcome::none("no finite gain values"),
            }
        }
        Measure::QLowpass { .. } => {
            let db = tryo!(signal(e, ds, range.clone(), Mode::Db));
            let ph = tryo!(signal(e, ds, range, Mode::Phase));
            q_lowpass(&db, &ph)
        }
        Measure::OutputNoiseAt { freq } | Measure::InputNoiseAt { freq } => {
            let s = tryo!(value_sig());
            match num::interp(&s.x, &s.y, *freq, true) {
                Some(v) => Outcome::of(v).at(*freq),
                None => Outcome::none(format!(
                    "{} is outside the noise sweep {}..{}",
                    human(*freq, "Hz"),
                    human(s.x[0], "Hz"),
                    human(s.x[s.x.len() - 1], "Hz")
                )),
            }
        }
        Measure::OutputNoiseRms { from, to } | Measure::InputNoiseRms { from, to } => {
            let s = tryo!(value_sig());
            let last = s.x[s.x.len() - 1];
            match crate::noise::integrate_rms(&s.x, &s.y, *from, *to) {
                Ok(v) => Outcome::of(v)
                    .with("from", from.unwrap_or(s.x[0]), "Hz")
                    .with("to", to.unwrap_or(last), "Hz"),
                Err(note) => Outcome::none(note),
            }
        }
        // Measured by `rejection`, which needs two datasets.
        Measure::PsrrDbAt { .. } | Measure::CmrrDbAt { .. } => {
            Outcome::none("a rejection measurement needs the derived AC run")
        }
        Measure::FreqAtDb { db, .. } => {
            let s = tryo!(signal(e, ds, range, Mode::Db));
            match num::crossings(&s.x, &s.y, *db, true).first() {
                Some(&(f, _)) => Outcome::of(f),
                None => {
                    let ex = num::extremes(&s.x, &s.y);
                    Outcome::none(match ex {
                        Some(ex) => format!(
                            "the gain never crosses {} (it spans {} to {})",
                            human(*db, "dB"),
                            human(ex.min, "dB"),
                            human(ex.max, "dB")
                        ),
                        None => "no finite gain values".into(),
                    })
                }
            }
        }
    }
}

fn ordinal(n: i64) -> String {
    let suffix = match (n.abs() % 10, n.abs() % 100) {
        (1, x) if x != 11 => "st",
        (2, x) if x != 12 => "nd",
        (3, x) if x != 13 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn crossing(s: &Sig, level: f64, dir: Direction, nth: i64, u: &Units) -> Outcome {
    if nth == 0 {
        return Outcome::none("nth counts from 1 (or from -1 for the last crossing)");
    }
    let all = num::crossings(&s.x, &s.y, level, s.log);
    let hits: Vec<f64> = all
        .iter()
        .filter(|(_, e)| match dir {
            Direction::Rise => *e == num::Edge::Rise,
            Direction::Fall => *e == num::Edge::Fall,
            Direction::Either => true,
        })
        .map(|(x, _)| *x)
        .collect();
    let idx = if nth > 0 {
        usize::try_from(nth - 1).ok()
    } else {
        hits.len()
            .checked_sub(usize::try_from(-nth).unwrap_or(usize::MAX))
    };
    if let Some(x) = idx.and_then(|i| hits.get(i)) {
        return Outcome::of(*x);
    }
    let dir_text = match dir {
        Direction::Rise => " rising",
        Direction::Fall => " falling",
        Direction::Either => "",
    };
    if hits.is_empty() {
        let span = num::extremes(&s.x, &s.y)
            .map(|ex| {
                format!(
                    "; it spans {} to {}",
                    human(ex.min, &u.expr),
                    human(ex.max, &u.expr)
                )
            })
            .unwrap_or_default();
        Outcome::none(format!(
            "never crosses {}{dir_text}{span}",
            human(level, &u.expr)
        ))
    } else {
        Outcome::none(format!(
            "crosses {}{dir_text} only {} times, so there is no {} crossing",
            human(level, &u.expr),
            hits.len(),
            ordinal(nth)
        ))
    }
}

/// The net transition of a signal: initial and final values, and whether
/// their difference is large enough to be a step rather than noise.
struct Transition {
    y0: f64,
    yf: f64,
    step: f64,
    significant: bool,
}

fn transition(s: &Sig) -> Option<Transition> {
    let (y0, yf) = num::first_last_finite(&s.y)?;
    let ex = num::extremes(&s.x, &s.y)?;
    let scale = ex.max.abs().max(ex.min.abs()).max(ex.max - ex.min);
    let step = yf - y0;
    Some(Transition {
        y0,
        yf,
        step,
        significant: step.abs() > 1e-6 * scale && step != 0.0,
    })
}

fn edge_time(s: &Sig, low_pct: f64, high_pct: f64, want: num::Edge, u: &Units) -> Outcome {
    if !(0.0..=100.0).contains(&low_pct)
        || !(0.0..=100.0).contains(&high_pct)
        || low_pct >= high_pct
    {
        return Outcome::none(format!(
            "low_pct ({low_pct}) must be below high_pct ({high_pct}), both within 0..100"
        ));
    }
    let Some(tr) = transition(s) else {
        return Outcome::none("no finite values in the window");
    };
    if !tr.significant {
        return Outcome::none(format!(
            "initial and final values are equal ({}), so there is no net transition; give from/to around one edge",
            human(tr.y0, &u.expr)
        ));
    }
    let dir = if tr.step > 0.0 {
        num::Edge::Rise
    } else {
        num::Edge::Fall
    };
    if dir != want {
        return Outcome::none(match dir {
            num::Edge::Rise => format!(
                "the signal rises from {} to {}; use rise_time",
                human(tr.y0, &u.expr),
                human(tr.yf, &u.expr)
            ),
            num::Edge::Fall => format!(
                "the signal falls from {} to {}; use fall_time",
                human(tr.y0, &u.expr),
                human(tr.yf, &u.expr)
            ),
        });
    }
    let l1 = tr.y0 + tr.step * low_pct / 100.0;
    let l2 = tr.y0 + tr.step * high_pct / 100.0;
    let first = |level: f64, after: f64| {
        num::crossings(&s.x, &s.y, level, s.log)
            .into_iter()
            .find(|&(x, e)| e == dir && x >= after)
            .map(|(x, _)| x)
    };
    let Some(t1) = first(l1, f64::NEG_INFINITY) else {
        return Outcome::none(format!(
            "never crosses the {low_pct}% level ({})",
            human(l1, &u.expr)
        ));
    };
    let Some(t2) = first(l2, t1) else {
        return Outcome::none(format!(
            "crosses {low_pct}% but never reaches the {high_pct}% level ({})",
            human(l2, &u.expr)
        ));
    };
    Outcome::of(t2 - t1)
        .with("t_low", t1, &u.axis)
        .with("t_high", t2, &u.axis)
}

fn shoot(s: &Sig, over: bool) -> Outcome {
    let Some(tr) = transition(s) else {
        return Outcome::none("no finite values in the window");
    };
    let Some(ex) = num::extremes(&s.x, &s.y) else {
        return Outcome::none("no finite values in the window");
    };
    if tr.significant {
        let a = tr.step.abs();
        let (amount, at) = match (over, tr.step > 0.0) {
            (true, true) => (ex.max - tr.yf, ex.max_at),
            (true, false) => (tr.yf - ex.min, ex.min_at),
            (false, true) => (tr.y0 - ex.min, ex.min_at),
            (false, false) => (ex.max - tr.y0, ex.max_at),
        };
        let pct = (amount / a * 100.0).max(0.0);
        let o = Outcome::of(pct);
        return if pct > 0.0 { o.at(at) } else { o };
    }
    if tr.yf == 0.0 {
        return Outcome::none(
            "there is no net transition and the final value is zero, so a percentage is undefined",
        );
    }
    let (amount, at) = if over {
        (ex.max - tr.yf, ex.max_at)
    } else {
        (tr.yf - ex.min, ex.min_at)
    };
    let pct = (amount / tr.yf.abs() * 100.0).max(0.0);
    let o = Outcome::of(pct).note("no net transition, so this is percent of the final value");
    if pct > 0.0 { o.at(at) } else { o }
}

fn settling(s: &Sig, tol_pct: f64, u: &Units) -> Outcome {
    if tol_pct.is_nan() || tol_pct <= 0.0 {
        return Outcome::none("tolerance_pct must be positive");
    }
    let Some(tr) = transition(s) else {
        return Outcome::none("no finite values in the window");
    };
    let reference = if tr.significant {
        tr.step.abs()
    } else {
        tr.yf.abs()
    };
    let band = reference * tol_pct / 100.0;
    if band == 0.0 {
        return Outcome::none("no net transition and a zero final value, so the band is empty");
    }
    let n = s.x.len();
    let outside = |i: usize| s.y[i].is_finite() && (s.y[i] - tr.yf).abs() > band;
    let Some(k) = (0..n).rev().find(|&i| outside(i)) else {
        return Outcome::of(0.0).with("t_settled", s.x[0], &u.axis);
    };
    if k == n - 1 {
        return Outcome::none(format!(
            "does not settle within ±{tol_pct}% by the end of the window"
        ));
    }
    // Interpolate where the deviation re-enters the band between k and k+1.
    let (ya, yb) = (s.y[k], s.y[k + 1]);
    let edge = tr.yf + band.copysign(ya - tr.yf);
    let t = if yb != ya {
        ((edge - ya) / (yb - ya)).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let ts = num::lerp_x(s.x[k], s.x[k + 1], t, s.log);
    let mut o = Outcome::of(ts - s.x[0]).with("t_settled", ts, &u.axis);
    if !tr.significant {
        o = o.note("no net transition, so the band is relative to the final value");
    }
    o
}

/// The `pct` point of a signal: on its net transition when it has one,
/// otherwise between its minimum and maximum (a clock or a pulse train).
/// A level, the direction a crossing of it must have (if the signal has a
/// net transition), and every crossing.
type LevelCrossings = (f64, Option<num::Edge>, Vec<(f64, num::Edge)>);

fn level_crossings(s: &Sig, pct: f64) -> Option<LevelCrossings> {
    let tr = transition(s)?;
    if tr.significant {
        let level = tr.y0 + tr.step * pct / 100.0;
        let dir = if tr.step > 0.0 {
            num::Edge::Rise
        } else {
            num::Edge::Fall
        };
        Some((level, Some(dir), num::crossings(&s.x, &s.y, level, s.log)))
    } else {
        let ex = num::extremes(&s.x, &s.y)?;
        let level = ex.min + (ex.max - ex.min) * pct / 100.0;
        Some((level, None, num::crossings(&s.x, &s.y, level, s.log)))
    }
}

fn delay(a: &Sig, b: &Sig, pct: f64, u: &Units) -> Outcome {
    if !(0.0..=100.0).contains(&pct) {
        return Outcome::none("level_pct must be within 0..100");
    }
    let Some((la, da, ca)) = level_crossings(a, pct) else {
        return Outcome::none("from_expr has no finite values");
    };
    let Some(t_from) = ca
        .iter()
        .find(|(_, e)| da.is_none_or(|d| d == *e))
        .map(|(x, _)| *x)
    else {
        return Outcome::none(format!(
            "from_expr never crosses its {pct}% level ({})",
            human(la, "")
        ));
    };
    let Some((lb, db, cb)) = level_crossings(b, pct) else {
        return Outcome::none("to_expr has no finite values");
    };
    let t_to = match db {
        Some(d) => cb.iter().find(|(_, e)| *e == d).map(|(x, _)| *x),
        None => cb.iter().find(|(x, _)| *x >= t_from).map(|(x, _)| *x),
    };
    let Some(t_to) = t_to else {
        return Outcome::none(format!(
            "to_expr never crosses its {pct}% level ({}) after from_expr does",
            human(lb, "")
        ));
    };
    Outcome::of(t_to - t_from)
        .with("t_from", t_from, &u.axis)
        .with("t_to", t_to, &u.axis)
}

/// Period from rising crossings of the mean, with a little hysteresis so a
/// noisy or ringing edge counts once.
fn period_of(s: &Sig, u: &Units) -> Result<(f64, usize, f64), String> {
    let ex = num::extremes(&s.x, &s.y).ok_or("no finite values in the window")?;
    let pp = ex.max - ex.min;
    if pp == 0.0 {
        return Err("the signal is constant".into());
    }
    let span = s.x[s.x.len() - 1] - s.x[0];
    let mean = if span > 0.0 {
        num::trapz(&s.x, &s.y).map(|i| i / span).unwrap_or(f64::NAN)
    } else {
        f64::NAN
    };
    if !mean.is_finite() {
        return Err("the window has no extent".into());
    }
    let rises: Vec<f64> = num::swings(&s.x, &s.y, mean, 0.02 * pp, s.log)
        .into_iter()
        .filter(|(_, e)| *e == num::Edge::Rise)
        .map(|(x, _)| x)
        .collect();
    if rises.len() < 2 {
        return Err(format!(
            "fewer than two rising crossings of the mean ({}); the window must hold at least one full period",
            human(mean, &u.expr)
        ));
    }
    let period = (rises[rises.len() - 1] - rises[0]) / (rises.len() - 1) as f64;
    Ok((period, rises.len() - 1, rises[0]))
}

fn duty(s: &Sig, u: &Units) -> Outcome {
    let Some(ex) = num::extremes(&s.x, &s.y) else {
        return Outcome::none("no finite values in the window");
    };
    let pp = ex.max - ex.min;
    if pp == 0.0 {
        return Outcome::none("the signal is constant");
    }
    let mid = (ex.max + ex.min) / 2.0;
    let edges = num::swings(&s.x, &s.y, mid, 0.02 * pp, s.log);
    let rises: Vec<usize> = edges
        .iter()
        .enumerate()
        .filter(|(_, (_, e))| *e == num::Edge::Rise)
        .map(|(i, _)| i)
        .collect();
    if rises.len() < 2 {
        return Outcome::none(format!(
            "fewer than two rising edges through the midpoint ({}); the window must hold at least one full period",
            human(mid, &u.expr)
        ));
    }
    let (first, last) = (rises[0], rises[rises.len() - 1]);
    let mut high = 0.0;
    for i in first..last {
        if edges[i].1 == num::Edge::Rise
            && let Some((tf, num::Edge::Fall)) = edges.get(i + 1)
        {
            high += tf - edges[i].0;
        }
    }
    let total = edges[last].0 - edges[first].0;
    if total <= 0.0 {
        return Outcome::none("the periods found have no extent");
    }
    Outcome::of(high / total * 100.0).with("cycles", (rises.len() - 1) as f64, "")
}

fn thd(s: &Sig, f0: f64, harmonics: usize, periods: usize, u: &Units) -> Outcome {
    if !(f0 > 0.0 && f0.is_finite()) {
        return Outcome::none("fundamental must be a positive frequency");
    }
    if harmonics < 2 {
        return Outcome::none("harmonics must be at least 2 (the fundamental is harmonic 1)");
    }
    if harmonics > 1000 {
        return Outcome::none("harmonics is capped at 1000");
    }
    let period = 1.0 / f0;
    let n = s.x.len();
    let span = s.x[n - 1] - s.x[0];
    let available = (span / period + 1e-9).floor() as usize;
    if available < 1 {
        return Outcome::none(format!(
            "needs at least one full period of {} ({}), and the transient is {} long",
            human(f0, "Hz"),
            human(period, &u.axis),
            human(span, &u.axis)
        ));
    }
    let k = periods.clamp(1, available);
    let per_period = (16 * harmonics).max(256);
    let total = (k * per_period).min(1 << 20);
    let t_end = s.x[n - 1];
    let t_start = t_end - k as f64 * period;
    let dt = (t_end - t_start) / total as f64;
    let samples = num::resample(&s.x, &s.y, t_start, dt, total);
    if samples.iter().any(|v| !v.is_finite()) {
        return Outcome::none("the signal has non-finite values in the analysed periods");
    }
    let amp = |h: usize| -> f64 {
        let bin = k * h;
        let (mut re, mut im) = (0.0, 0.0);
        for (j, y) in samples.iter().enumerate() {
            let angle = 2.0 * std::f64::consts::PI * ((bin * j) % total) as f64 / total as f64;
            re += y * angle.cos();
            im -= y * angle.sin();
        }
        2.0 * re.hypot(im) / total as f64
    };
    let a1 = amp(1);
    if a1 == 0.0 || !a1.is_finite() {
        return Outcome::none(format!(
            "no component at the fundamental ({})",
            human(f0, "Hz")
        ));
    }
    let mut sum_sq = 0.0;
    let mut out = Outcome::default();
    out.extra.push(NamedValue {
        name: "fundamental".into(),
        value: a1,
        unit: u.expr.clone(),
    });
    for h in 2..=harmonics {
        let a = amp(h);
        sum_sq += a * a;
        out.extra.push(NamedValue {
            name: format!("h{h}"),
            value: a,
            unit: u.expr.clone(),
        });
    }
    out.value = Some(sum_sq.sqrt() / a1 * 100.0);
    // Points per period in the original data, to warn about harmonics that
    // the simulator's time step cannot resolve.
    let window_points = s.x.iter().filter(|&&t| t >= t_start).count();
    let per = window_points as f64 / k as f64;
    if per < 4.0 * harmonics as f64 {
        out.note = Some(format!(
            "only about {} simulator points per period; harmonics this high are unreliable, reduce the maximum time step",
            per.round()
        ));
    }
    out
}

const HALF_POWER_DB: f64 = 3.010_299_956_639_812;

fn bandwidth(s: &Sig, reference: BwReference) -> Outcome {
    let Some(ex) = num::extremes(&s.x, &s.y) else {
        return Outcome::none("no finite gain values");
    };
    let n = s.x.len();
    match reference {
        BwReference::Dc => {
            let Some((r, _)) = num::first_last_finite(&s.y) else {
                return Outcome::none("no finite gain values");
            };
            let level = r - HALF_POWER_DB;
            match num::crossings(&s.x, &s.y, level, true)
                .into_iter()
                .find(|(_, e)| *e == num::Edge::Fall)
            {
                Some((f, _)) => Outcome::of(f),
                None => Outcome::none(format!(
                    "the gain never falls 3 dB below its low-frequency value ({}) in the swept range",
                    human(r, "dB")
                )),
            }
        }
        BwReference::Peak => {
            let ipk = s.x.partition_point(|&f| f < ex.max_at).min(n - 1);
            let level = ex.max - HALF_POWER_DB;
            let all = num::crossings(&s.x, &s.y, level, true);
            let f_high = all
                .iter()
                .find(|(f, e)| *e == num::Edge::Fall && *f >= s.x[ipk])
                .map(|(f, _)| *f);
            let f_low = all
                .iter()
                .rev()
                .find(|(f, e)| *e == num::Edge::Rise && *f <= s.x[ipk])
                .map(|(f, _)| *f);
            match (f_low, f_high) {
                (Some(lo), Some(hi)) => Outcome::of(hi - lo)
                    .with("f_low", lo, "Hz")
                    .with("f_high", hi, "Hz")
                    .with("f_center", (lo * hi).sqrt(), "Hz"),
                (None, Some(hi)) => Outcome::of(hi),
                (Some(lo), None) => Outcome::none(
                    "only a lower -3 dB edge is in the swept range (a highpass response), so the bandwidth is unbounded here",
                )
                .with("f_low", lo, "Hz"),
                (None, None) => Outcome::none(format!(
                    "the gain stays within 3 dB of its peak ({}) over the swept range",
                    human(ex.max, "dB")
                )),
            }
        }
    }
}

/// Q of a second-order lowpass: at f0 the phase has turned 90 degrees and
/// the gain, relative to the low-frequency gain, is Q.
fn q_lowpass(db: &Sig, ph: &Sig) -> Outcome {
    let (Some((g0, _)), Some((p0, _))) =
        (num::first_last_finite(&db.y), num::first_last_finite(&ph.y))
    else {
        return Outcome::none("no finite gain values");
    };
    // A lowpass starts at 0 degrees (or 180 when it inverts); the lowest
    // swept frequency is already a little way down the slope, so take the
    // nearest multiple of 180 as the starting phase.
    let start = (p0 / 180.0).round() * 180.0;
    let turned = num::crossings(&ph.x, &ph.y, start - 90.0, true)
        .into_iter()
        .find(|(_, e)| *e == num::Edge::Fall)
        .map(|(f, _)| f);
    let Some(f0) = turned else {
        return Outcome::none(
            "the phase never turns 90 degrees from its low-frequency value in the swept range, so this is not a second-order lowpass response there (or the sweep stops too early); use poles_zeros for Q",
        );
    };
    match num::interp(&db.x, &db.y, f0, true) {
        Some(g) => Outcome::of(10f64.powf((g - g0) / 20.0)).with("f0", f0, "Hz"),
        None => Outcome::none("no gain value at the 90 degree point"),
    }
}

fn unity_gain(s: &Sig) -> Result<f64, String> {
    let all = num::crossings(&s.x, &s.y, 0.0, true);
    if let Some((f, _)) = all.iter().find(|(_, e)| *e == num::Edge::Fall) {
        return Ok(*f);
    }
    if let Some((f, _)) = all.first() {
        return Ok(*f);
    }
    let ex = num::extremes(&s.x, &s.y).ok_or("no finite gain values")?;
    Err(if ex.min > 0.0 {
        format!(
            "the gain stays above 0 dB (lowest {}) in the swept range",
            human(ex.min, "dB")
        )
    } else {
        format!(
            "the gain stays below 0 dB (highest {}) in the swept range",
            human(ex.max, "dB")
        )
    })
}

/// How many half turns to remove from the measured phase to get the loop
/// gain's own phase. A minimum-phase response with low-frequency slope of
/// `n` x 20 dB/decade has a phase near `n` x 90 degrees there; any leftover
/// odd multiple of 180 is a sign inversion in what was probed (an inverting
/// input, a probe the wrong way round), and an even one is a 360 wrap.
fn half_turns(db: &Sig, ph: &Sig) -> i64 {
    let n = db.x.len();
    if n < 2 {
        return 0;
    }
    let f0 = db.x[0];
    let j = (1..n)
        .find(|&j| db.x[j] >= f0 * 10f64.powf(0.2))
        .unwrap_or(n - 1);
    let decades = (db.x[j] / f0).log10();
    let slope = if decades > 0.0 {
        (db.y[j] - db.y[0]) / (20.0 * decades)
    } else {
        0.0
    };
    let expected = slope * 90.0;
    let offset = (ph.y[0] - expected) / 180.0;
    if offset.is_finite() {
        offset.round() as i64
    } else {
        0
    }
}

fn phase_margin(db: &Sig, ph: &Sig) -> Outcome {
    let fc = match db_crossing_down(db) {
        Ok(f) => f,
        Err(note) => return Outcome::none(note),
    };
    let k = half_turns(db, ph);
    let Some(phase) = num::interp(&ph.x, &ph.y, fc, true) else {
        return Outcome::none("the phase is not defined at the unity-gain frequency");
    };
    let loop_phase = phase - 180.0 * k as f64;
    let mut o = Outcome::of(180.0 + loop_phase).at(fc);
    if k % 2 != 0 {
        o = o.note("the probed response is inverted (180 degrees at low frequency); measured as its negative");
    }
    o
}

fn db_crossing_down(db: &Sig) -> Result<f64, String> {
    let all = num::crossings(&db.x, &db.y, 0.0, true);
    if let Some((f, _)) = all.iter().find(|(_, e)| *e == num::Edge::Fall) {
        return Ok(*f);
    }
    unity_gain(db)
        .and_then(|_| Err("the gain never falls through 0 dB in the swept range".to_string()))
}

fn gain_margin(db: &Sig, ph: &Sig) -> Outcome {
    let k = half_turns(db, ph);
    let loop_phase: Vec<f64> = ph.y.iter().map(|p| p - 180.0 * k as f64).collect();
    let after = db_crossing_down(db).unwrap_or(f64::NEG_INFINITY);
    let crossings = num::crossings(&ph.x, &loop_phase, -180.0, true);
    let pick = crossings
        .iter()
        .find(|(f, _)| *f >= after)
        .or_else(|| crossings.first());
    let Some(&(f180, _)) = pick else {
        return Outcome::none(
            "the loop phase never reaches -180 degrees in the swept range, so the gain margin is unbounded here",
        );
    };
    match num::interp(&db.x, &db.y, f180, true) {
        Some(g) => Outcome::of(-g).at(f180),
        None => Outcome::none("the gain is not defined at the phase crossover"),
    }
}

/// Numeric helpers on plain slices. `x` is ascending; `log` interpolates
/// against the logarithm of `x`, for frequency axes.
mod num {
    use super::human;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Edge {
        Rise,
        Fall,
    }

    pub fn frac(x0: f64, x1: f64, at: f64, log: bool) -> f64 {
        if x1 == x0 {
            return 0.0;
        }
        if log && x0 > 0.0 && x1 > 0.0 && at > 0.0 {
            (at.ln() - x0.ln()) / (x1.ln() - x0.ln())
        } else {
            (at - x0) / (x1 - x0)
        }
    }

    pub fn lerp_x(x0: f64, x1: f64, t: f64, log: bool) -> f64 {
        if log && x0 > 0.0 && x1 > 0.0 {
            (x0.ln() + t * (x1.ln() - x0.ln())).exp()
        } else {
            x0 + t * (x1 - x0)
        }
    }

    /// `y` at `at`, or `None` outside the data.
    pub fn interp(x: &[f64], y: &[f64], at: f64, log: bool) -> Option<f64> {
        let n = x.len().min(y.len());
        if n == 0 || !at.is_finite() {
            return None;
        }
        let (lo, hi) = (x[0], x[n - 1]);
        let tol = 1e-12 * lo.abs().max(hi.abs());
        if at < lo - tol || at > hi + tol {
            return None;
        }
        if n == 1 {
            return Some(y[0]);
        }
        let at = at.clamp(lo, hi);
        let i = x[..n].partition_point(|&v| v < at);
        if i == 0 {
            return Some(y[0]);
        }
        if i >= n {
            return Some(y[n - 1]);
        }
        if x[i] == at {
            return Some(y[i]);
        }
        let t = frac(x[i - 1], x[i], at, log);
        Some(y[i - 1] + t * (y[i] - y[i - 1]))
    }

    /// Every crossing of `level`, with its direction. A point exactly on the
    /// level counts once, on the segment that arrives at it.
    pub fn crossings(x: &[f64], y: &[f64], level: f64, log: bool) -> Vec<(f64, Edge)> {
        let n = x.len().min(y.len());
        let mut out = Vec::new();
        for i in 0..n.saturating_sub(1) {
            let a = y[i] - level;
            let b = y[i + 1] - level;
            if !(a.is_finite() && b.is_finite()) {
                continue;
            }
            let edge = if a < 0.0 && b >= 0.0 {
                Edge::Rise
            } else if a > 0.0 && b <= 0.0 {
                Edge::Fall
            } else {
                continue;
            };
            let t = a / (a - b);
            out.push((lerp_x(x[i], x[i + 1], t, log), edge));
        }
        out
    }

    /// Crossings of `level` that complete a swing past `level ± h`, so the
    /// chatter of a noisy edge counts once. The first swing only sets the
    /// state, since where the signal came from is unknown.
    pub fn swings(x: &[f64], y: &[f64], level: f64, h: f64, log: bool) -> Vec<(f64, Edge)> {
        let n = x.len().min(y.len());
        let mut out = Vec::new();
        let mut state: Option<Edge> = None;
        let mut last_rise = None;
        let mut last_fall = None;
        for i in 0..n {
            if i > 0 {
                let (a, b) = (y[i - 1] - level, y[i] - level);
                if a.is_finite() && b.is_finite() {
                    if a < 0.0 && b >= 0.0 {
                        last_rise = Some(lerp_x(x[i - 1], x[i], a / (a - b), log));
                    } else if a > 0.0 && b <= 0.0 {
                        last_fall = Some(lerp_x(x[i - 1], x[i], a / (a - b), log));
                    }
                }
            }
            let v = y[i];
            if !v.is_finite() {
                continue;
            }
            if v >= level + h && state != Some(Edge::Rise) {
                if state == Some(Edge::Fall)
                    && let Some(t) = last_rise
                {
                    out.push((t, Edge::Rise));
                }
                state = Some(Edge::Rise);
            } else if v <= level - h && state != Some(Edge::Fall) {
                if state == Some(Edge::Rise)
                    && let Some(t) = last_fall
                {
                    out.push((t, Edge::Fall));
                }
                state = Some(Edge::Fall);
            }
        }
        out
    }

    /// The part of the signal within `from..to`, with interpolated end
    /// points so a window between samples is measured exactly.
    pub fn window(
        x: &[f64],
        y: &[f64],
        from: Option<f64>,
        to: Option<f64>,
        log: bool,
        unit: &str,
    ) -> Result<(Vec<f64>, Vec<f64>), String> {
        let n = x.len().min(y.len());
        if n == 0 {
            return Err("no points".into());
        }
        if from.is_none() && to.is_none() {
            return Ok((x[..n].to_vec(), y[..n].to_vec()));
        }
        let (lo0, hi0) = (x[0], x[n - 1]);
        let lo = from.unwrap_or(lo0);
        let hi = to.unwrap_or(hi0);
        if !lo.is_finite() || !hi.is_finite() {
            return Err("from and to must be finite".into());
        }
        if lo > hi {
            return Err(format!(
                "from ({}) is after to ({})",
                human(lo, unit),
                human(hi, unit)
            ));
        }
        if hi < lo0 || lo > hi0 {
            return Err(format!(
                "the window {}..{} is outside the data, which spans {}..{}",
                human(lo, unit),
                human(hi, unit),
                human(lo0, unit),
                human(hi0, unit)
            ));
        }
        let lo = lo.max(lo0);
        let hi = hi.min(hi0);
        let mut xs = vec![lo];
        let mut ys = vec![interp(x, y, lo, log).unwrap_or(f64::NAN)];
        for i in 0..n {
            if x[i] > lo && x[i] < hi {
                xs.push(x[i]);
                ys.push(y[i]);
            }
        }
        if hi > lo {
            xs.push(hi);
            ys.push(interp(x, y, hi, log).unwrap_or(f64::NAN));
        }
        Ok((xs, ys))
    }

    pub fn trapz(x: &[f64], y: &[f64]) -> Option<f64> {
        let mut acc = 0.0;
        let mut any = false;
        for i in 0..x.len().min(y.len()).saturating_sub(1) {
            let (a, b) = (y[i], y[i + 1]);
            if a.is_finite() && b.is_finite() {
                acc += (x[i + 1] - x[i]) * (a + b) / 2.0;
                any = true;
            }
        }
        any.then_some(acc)
    }

    /// Integral of the square of the piecewise-linear signal, exactly.
    pub fn trapz_sq(x: &[f64], y: &[f64]) -> Option<f64> {
        let mut acc = 0.0;
        let mut any = false;
        for i in 0..x.len().min(y.len()).saturating_sub(1) {
            let (a, b) = (y[i], y[i + 1]);
            if a.is_finite() && b.is_finite() {
                acc += (x[i + 1] - x[i]) * (a * a + a * b + b * b) / 3.0;
                any = true;
            }
        }
        any.then_some(acc)
    }

    pub struct Extremes {
        pub min: f64,
        pub min_at: f64,
        pub max: f64,
        pub max_at: f64,
    }

    pub fn extremes(x: &[f64], y: &[f64]) -> Option<Extremes> {
        let mut ex: Option<Extremes> = None;
        for (&xi, &yi) in x.iter().zip(y) {
            if !yi.is_finite() {
                continue;
            }
            match &mut ex {
                None => {
                    ex = Some(Extremes {
                        min: yi,
                        min_at: xi,
                        max: yi,
                        max_at: xi,
                    })
                }
                Some(e) => {
                    if yi < e.min {
                        e.min = yi;
                        e.min_at = xi;
                    }
                    if yi > e.max {
                        e.max = yi;
                        e.max_at = xi;
                    }
                }
            }
        }
        ex
    }

    pub fn first_last_finite(y: &[f64]) -> Option<(f64, f64)> {
        let first = y.iter().copied().find(|v| v.is_finite())?;
        let last = y.iter().rev().copied().find(|v| v.is_finite())?;
        Some((first, last))
    }

    /// Sample the signal at `t0 + k dt`, linearly, in one pass.
    pub fn resample(x: &[f64], y: &[f64], t0: f64, dt: f64, count: usize) -> Vec<f64> {
        let n = x.len().min(y.len());
        let mut out = Vec::with_capacity(count);
        if n == 0 {
            return vec![f64::NAN; count];
        }
        let mut j = 0;
        for k in 0..count {
            let t = (t0 + dt * k as f64).clamp(x[0], x[n - 1]);
            while j + 2 < n && x[j + 1] < t {
                j += 1;
            }
            if n == 1 {
                out.push(y[0]);
                continue;
            }
            let (x0, x1) = (x[j], x[j + 1]);
            let f = if x1 > x0 {
                ((t - x0) / (x1 - x0)).clamp(0.0, 1.0)
            } else {
                0.0
            };
            out.push(y[j] + f * (y[j + 1] - y[j]));
        }
        out
    }
}

// The text form.

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct MeasureParseError {
    pub message: String,
}

fn perr(message: impl Into<String>) -> MeasureParseError {
    MeasureParseError {
        message: message.into(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Arg {
    Expr,
    Num,
    /// A number shown as a plain decimal (percent, dB), where `100m` would
    /// read oddly.
    Plain,
    Int,
    Direction,
    Reference,
    /// An instance name, such as a source.
    Name,
}

#[derive(Clone, Copy)]
enum Dflt {
    Required,
    Optional,
    Num(f64),
    Int(i64),
    Str(&'static str),
}

#[derive(Clone, Copy)]
struct P {
    name: &'static str,
    arg: Arg,
    dflt: Dflt,
}

const fn p(name: &'static str, arg: Arg, dflt: Dflt) -> P {
    P { name, arg, dflt }
}

const EXPR: P = p("expr", Arg::Expr, Dflt::Required);
const FROM: P = p("from", Arg::Num, Dflt::Optional);
const TO: P = p("to", Arg::Num, Dflt::Optional);

const SIG_VALUE_AT: &[P] = &[EXPR, p("at", Arg::Num, Dflt::Required)];
const SIG_WINDOW: &[P] = &[EXPR, FROM, TO];
const SIG_CROSSING: &[P] = &[
    EXPR,
    p("level", Arg::Num, Dflt::Required),
    p("direction", Arg::Direction, Dflt::Str("either")),
    p("nth", Arg::Int, Dflt::Int(1)),
    FROM,
    TO,
];
const SIG_EDGE: &[P] = &[
    EXPR,
    p("low_pct", Arg::Plain, Dflt::Num(10.0)),
    p("high_pct", Arg::Plain, Dflt::Num(90.0)),
    FROM,
    TO,
];
const SIG_SETTLING: &[P] = &[
    EXPR,
    p("tolerance_pct", Arg::Plain, Dflt::Num(2.0)),
    FROM,
    TO,
];
const SIG_DELAY: &[P] = &[
    p("from_expr", Arg::Expr, Dflt::Required),
    p("to_expr", Arg::Expr, Dflt::Required),
    p("level_pct", Arg::Plain, Dflt::Num(50.0)),
];
const SIG_THD: &[P] = &[
    EXPR,
    p("fundamental", Arg::Num, Dflt::Required),
    p("harmonics", Arg::Int, Dflt::Int(9)),
    p("periods", Arg::Int, Dflt::Int(1)),
];
const SIG_FREQ: &[P] = &[EXPR, p("freq", Arg::Num, Dflt::Required)];
const SIG_BW: &[P] = &[EXPR, p("reference", Arg::Reference, Dflt::Str("dc"))];
const SIG_EXPR: &[P] = &[EXPR];
const SIG_DB: &[P] = &[EXPR, p("db", Arg::Plain, Dflt::Required)];
const SIG_NOISE_AT: &[P] = &[p("freq", Arg::Num, Dflt::Required)];
const SIG_BAND: &[P] = &[FROM, TO];
const SIG_REJECTION: &[P] = &[
    EXPR,
    p("source", Arg::Name, Dflt::Required),
    p("freq", Arg::Num, Dflt::Required),
];

/// Every measurement kind, for help text and error messages.
pub const KINDS: &[&str] = &[
    "value_at",
    "min",
    "max",
    "pp",
    "avg",
    "rms",
    "integral",
    "crossing",
    "rise_time",
    "fall_time",
    "overshoot_pct",
    "undershoot_pct",
    "settling_time",
    "delay",
    "frequency",
    "period",
    "duty_cycle",
    "thd",
    "gain_db_at",
    "phase_at",
    "bandwidth_3db",
    "unity_gain_freq",
    "phase_margin",
    "gain_margin",
    "peak_gain",
    "freq_at_db",
    "peaking_db",
    "q_lowpass",
    "output_noise_at",
    "input_noise_at",
    "output_noise_rms",
    "input_noise_rms",
    "psrr_db_at",
    "cmrr_db_at",
];

fn signature(kind: &str) -> Option<&'static [P]> {
    Some(match kind {
        "value_at" => SIG_VALUE_AT,
        "min" | "max" | "pp" | "avg" | "rms" | "integral" | "overshoot_pct" | "undershoot_pct"
        | "frequency" | "period" | "duty_cycle" => SIG_WINDOW,
        "crossing" => SIG_CROSSING,
        "rise_time" | "fall_time" => SIG_EDGE,
        "settling_time" => SIG_SETTLING,
        "delay" => SIG_DELAY,
        "thd" => SIG_THD,
        "gain_db_at" | "phase_at" => SIG_FREQ,
        "bandwidth_3db" => SIG_BW,
        "unity_gain_freq" | "phase_margin" | "gain_margin" | "peak_gain" | "peaking_db"
        | "q_lowpass" => SIG_EXPR,
        "freq_at_db" => SIG_DB,
        "output_noise_at" | "input_noise_at" => SIG_NOISE_AT,
        "output_noise_rms" | "input_noise_rms" => SIG_BAND,
        "psrr_db_at" | "cmrr_db_at" => SIG_REJECTION,
        _ => return None,
    })
}

/// Split `a, V(x,y), b=2` on commas outside parentheses.
pub(crate) fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '(' | '{' | '[' => {
                depth += 1;
                cur.push(c);
            }
            ')' | '}' | ']' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => out.push(std::mem::take(&mut cur).trim().to_string()),
            c => cur.push(c),
        }
    }
    if !cur.trim().is_empty() || !out.is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Split `kind(args)` into the kind and the text between the outer
/// parentheses. Returns the byte length consumed, so a spec line can carry on
/// after the call.
pub(crate) fn split_call(text: &str) -> Result<(String, String, usize), MeasureParseError> {
    let s = text.trim_start();
    let lead = text.len() - s.len();
    let open = s.find('(').ok_or_else(|| {
        perr(format!(
            "expected a measurement like `max(V(out))`, found `{}`",
            s.trim()
        ))
    })?;
    let kind = s[..open].trim().to_ascii_lowercase();
    if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(perr(format!(
            "`{}` is not a measurement name",
            s[..open].trim()
        )));
    }
    let mut depth = 0i32;
    for (i, c) in s[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let inner = s[open + 1..open + i].to_string();
                    return Ok((kind, inner, lead + open + i + 1));
                }
            }
            _ => {}
        }
    }
    Err(perr(format!("unclosed `(` in `{}`", s.trim())))
}

fn parse_call(text: &str) -> Result<Measure, MeasureParseError> {
    let (kind, inner, used) = split_call(text)?;
    if !text[used..].trim().is_empty() {
        return Err(perr(format!(
            "unexpected `{}` after the measurement",
            text[used..].trim()
        )));
    }
    build_measure(&kind, &inner)
}

pub(crate) fn build_measure(kind: &str, inner: &str) -> Result<Measure, MeasureParseError> {
    let sig = signature(kind).ok_or_else(|| {
        perr(format!(
            "unknown measurement `{kind}`; known: {}",
            KINDS.join(", ")
        ))
    })?;
    let mut obj = serde_json::Map::new();
    obj.insert("kind".into(), kind.into());
    let mut positional = 0usize;
    let mut seen_keyword = false;
    for arg in split_args(inner) {
        if arg.is_empty() {
            return Err(perr(format!("empty argument in {kind}(...)")));
        }
        let (param, value) = match keyword(&arg) {
            Some((name, value)) => {
                seen_keyword = true;
                let param = sig.iter().find(|p| p.name == name).ok_or_else(|| {
                    let names: Vec<&str> = sig.iter().map(|p| p.name).collect();
                    perr(format!(
                        "{kind} has no argument `{name}`; it takes {}",
                        names.join(", ")
                    ))
                })?;
                (*param, value)
            }
            None => {
                if seen_keyword {
                    return Err(perr(format!(
                        "positional argument `{arg}` after a keyword argument in {kind}(...)"
                    )));
                }
                let param = sig
                    .get(positional)
                    .ok_or_else(|| perr(format!("{kind} takes at most {} arguments", sig.len())))?;
                positional += 1;
                (*param, arg.as_str())
            }
        };
        if obj.contains_key(param.name) {
            return Err(perr(format!(
                "`{}` is given twice in {kind}(...)",
                param.name
            )));
        }
        obj.insert(param.name.into(), arg_value(kind, param, value)?);
    }
    for param in sig {
        if matches!(param.dflt, Dflt::Required) && !obj.contains_key(param.name) {
            return Err(perr(format!("{kind}(...) needs `{}`", param.name)));
        }
    }
    serde_json::from_value(serde_json::Value::Object(obj)).map_err(|e| perr(format!("{kind}: {e}")))
}

fn keyword(arg: &str) -> Option<(&str, &str)> {
    let (name, value) = arg.split_once('=')?;
    let name = name.trim();
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then(|| (name, value.trim()))
}

fn arg_value(kind: &str, param: P, value: &str) -> Result<serde_json::Value, MeasureParseError> {
    Ok(match param.arg {
        Arg::Expr => {
            expr::parse(value).map_err(|e| perr(format!("{kind}: {e}")))?;
            value.into()
        }
        Arg::Num | Arg::Plain => {
            let v = parse_number(value).ok_or_else(|| {
                perr(format!(
                    "{kind}: `{}` should be a number, found `{value}`",
                    param.name
                ))
            })?;
            serde_json::Number::from_f64(v)
                .map(serde_json::Value::Number)
                .ok_or_else(|| perr(format!("{kind}: `{value}` is not finite")))?
        }
        Arg::Int => {
            let v: i64 = value.trim_start_matches('+').parse().map_err(|_| {
                perr(format!(
                    "{kind}: `{}` should be a whole number, found `{value}`",
                    param.name
                ))
            })?;
            v.into()
        }
        Arg::Direction => match value.to_ascii_lowercase().as_str() {
            "rise" | "rising" | "up" => "rise".into(),
            "fall" | "falling" | "down" => "fall".into(),
            "either" | "both" | "any" => "either".into(),
            _ => {
                return Err(perr(format!(
                    "{kind}: direction is rise, fall or either, found `{value}`"
                )));
            }
        },
        Arg::Name => {
            let ok = !value.is_empty()
                && !value
                    .chars()
                    .any(|c| c.is_whitespace() || "(){}[]'\",=".contains(c));
            if !ok {
                return Err(perr(format!(
                    "{kind}: `{}` should be an instance name such as VDD, found `{value}`",
                    param.name
                )));
            }
            value.into()
        }
        Arg::Reference => match value.to_ascii_lowercase().as_str() {
            "dc" => "dc".into(),
            "peak" => "peak".into(),
            _ => {
                return Err(perr(format!(
                    "{kind}: reference is dc or peak, found `{value}`"
                )));
            }
        },
    })
}

impl std::str::FromStr for Measure {
    type Err = MeasureParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_call(s)
    }
}

impl fmt::Display for Measure {
    /// The text form, with arguments that hold their default left out, so
    /// the round trip through [`Measure::parse`] is exact.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = self.kind();
        let sig = signature(kind).unwrap_or(&[]);
        let value = serde_json::to_value(self).unwrap_or_default();
        let mut parts = Vec::new();
        let mut gap = false;
        for param in sig {
            let v = value.get(param.name).filter(|v| !v.is_null());
            let is_default = match (param.dflt, v) {
                (_, None) => true,
                (Dflt::Num(d), Some(v)) => v.as_f64() == Some(d),
                (Dflt::Int(d), Some(v)) => v.as_i64() == Some(d),
                (Dflt::Str(d), Some(v)) => v.as_str() == Some(d),
                _ => false,
            };
            let Some(v) = v.filter(|_| !is_default) else {
                gap = true;
                continue;
            };
            let text = match (param.arg, v) {
                (Arg::Num, v) => format_number(v.as_f64().unwrap_or(f64::NAN)),
                (Arg::Plain, v) => v.as_f64().map(|x| x.to_string()).unwrap_or_default(),
                (_, serde_json::Value::String(s)) => s.clone(),
                (_, v) => v.to_string(),
            };
            if gap {
                parts.push(format!("{}={text}", param.name));
            } else {
                parts.push(text);
            }
        }
        write!(f, "{kind}({})", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::Complex;
    use crate::expr::test_support::*;
    use std::f64::consts::PI;

    fn m(text: &str) -> Measure {
        Measure::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    fn val(ds: &Dataset, text: &str) -> f64 {
        let r = measure_dataset("x", &m(text), ds);
        r.value
            .unwrap_or_else(|| panic!("{text}: no value: {:?}", r.note))
    }

    fn none(ds: &Dataset, text: &str) -> String {
        let r = measure_dataset("x", &m(text), ds);
        assert!(
            r.value.is_none(),
            "{text}: expected no value, got {:?}",
            r.value
        );
        r.note.unwrap_or_else(|| panic!("{text}: no note"))
    }

    fn rel(a: f64, b: f64, tol: f64) {
        assert!(
            (a - b).abs() <= tol * b.abs().max(1e-300),
            "{a} vs {b} (rel tol {tol})"
        );
    }

    /// Was a bug found comparing simulators: ngspice and Xyce write the AC
    /// analysis before the transient, and avg/pp measured the AC magnitudes
    /// (a window of 3m..5m read as millihertz). Waveform measurements now use
    /// the transient whatever the order.
    #[test]
    fn waveform_measures_prefer_the_transient() {
        let f = logspace(10.0, 1e6, 50);
        let gain: Vec<Complex> = f.iter().map(|_| Complex::new(100.0, 0.0)).collect();
        let ac_ds = ac(f, vec![("V(out)", gain)]);
        let t = linspace(0.0, 5e-3, 501);
        let v: Vec<f64> = t.iter().map(|t| (2.0 * PI * 1e3 * t).sin()).collect();
        let tran_ds = tran(t, vec![("V(out)", v)]);
        let both = [ac_ds, tran_ds];
        let avg = measure("a", &m("avg(V(out), 1m, 5m)"), &both);
        assert!(avg.value.unwrap().abs() < 1e-3, "{:?}", avg);
        let pp = measure("p", &m("pp(V(out), 3m, 5m)"), &both);
        rel(pp.value.unwrap(), 2.0, 1e-3);
        // A frequency measurement still finds the AC analysis.
        let g = measure("g", &m("gain_db_at(V(out), 1k)"), &both);
        rel(g.value.unwrap(), 40.0, 1e-6);
    }

    fn noise_ds(f: Vec<f64>, on: Vec<f64>, inn: Vec<f64>) -> Dataset {
        use crate::dataset::{Quantity, Step, Vector, VectorData};
        let n = f.len();
        Dataset {
            title: String::new(),
            plotname: "Noise Spectral Density Curves".into(),
            kind: AnalysisKind::Noise,
            axis: Some(0),
            vectors: vec![
                Vector {
                    name: "frequency".into(),
                    quantity: Quantity::Frequency,
                    data: VectorData::Real(f),
                },
                Vector {
                    name: crate::noise::ONOISE.into(),
                    quantity: Quantity::Voltage,
                    data: VectorData::Real(on),
                },
                Vector {
                    name: crate::noise::INOISE.into(),
                    quantity: Quantity::Voltage,
                    data: VectorData::Real(inn),
                },
            ],
            steps: vec![Step {
                range: 0..n,
                label: String::new(),
            }],
        }
    }

    #[test]
    fn noise_densities_and_integrated_noise() {
        let f = logspace(1.0, 1e6, 61);
        // White output noise plus a 1/f corner at 1 kHz on the input side.
        let on = vec![2.879e-9; f.len()];
        let inn: Vec<f64> = f
            .iter()
            .map(|f| 5.758e-9 * (1.0 + 1e3 / f).sqrt())
            .collect();
        let ds = noise_ds(f.clone(), on, inn);
        let r = measure_dataset("vn", &m("output_noise_at(1k)"), &ds);
        rel(r.value.unwrap(), 2.879e-9, 1e-9);
        assert_eq!(r.unit, "V/√Hz");
        assert_eq!(r.at, Some(1e3));
        rel(
            val(&ds, "input_noise_at(1Meg)"),
            5.758e-9 * (1.001f64).sqrt(),
            1e-6,
        );
        let rms = val(&ds, "output_noise_rms()");
        rel(rms, 2.879e-9 * (1e6f64 - 1.0).sqrt(), 1e-9);
        let band = measure_dataset("b", &m("output_noise_rms(10, 10k)"), &ds);
        rel(band.value.unwrap(), 2.879e-9 * (1e4f64 - 10.0).sqrt(), 1e-9);
        assert_eq!(band.unit, "V");
        // White plus 1/f: S^2 = a^2 (1 + fc/f), integral a^2 (b - a' + fc ln(b/a')).
        let want = 5.758e-9 * ((1e4f64 - 10.0) + 1e3 * (1e3f64).ln()).sqrt();
        rel(val(&ds, "input_noise_rms(10, 10k)"), want, 2e-3);
        assert!(none(&ds, "output_noise_at(10Meg)").contains("outside the noise sweep"));
        assert!(none(&ds, "output_noise_rms(0.1, 1k)").contains("outside the swept range"));
        // The text form round-trips with optional bands left out.
        assert_eq!(m("output_noise_rms()").to_string(), "output_noise_rms()");
        assert_eq!(
            m("input_noise_rms(to=1k)").to_string(),
            "input_noise_rms(to=1k)"
        );
    }

    /// A noise analysis has a frequency axis too; AC measurements must not
    /// read it, and noise measurements must not read an AC analysis.
    #[test]
    fn noise_and_ac_results_stay_apart() {
        let f = logspace(1.0, 1e6, 61);
        let noise = noise_ds(f.clone(), vec![1e-9; f.len()], vec![2e-9; f.len()]);
        let gain: Vec<Complex> = f.iter().map(|_| Complex::new(10.0, 0.0)).collect();
        let ac_ds = ac(f, vec![("V(onoise)", gain)]);
        let both = [noise.clone(), ac_ds];
        let g = measure("g", &m("gain_db_at(V(onoise), 1k)"), &both);
        rel(g.value.unwrap(), 20.0, 1e-9);
        let n = measure("n", &m("output_noise_at(1k)"), &both);
        rel(n.value.unwrap(), 1e-9, 1e-9);
        let only_ac = [both[1].clone()];
        let n = measure("n", &m("output_noise_at(1k)"), &only_ac);
        assert!(n.note.unwrap().contains("needs a noise analysis"));
        let only_noise = [noise];
        let g = measure("g", &m("gain_db_at(V(onoise), 1k)"), &only_noise);
        assert!(g.note.unwrap().contains("needs an AC analysis"));
    }

    #[test]
    fn rejection_compares_the_signal_and_derived_runs() {
        let f = logspace(1.0, 1e6, 61);
        let sig: Vec<Complex> = f.iter().map(|_| Complex::new(100.0, 0.0)).collect();
        // The supply reaches the output through a first-order highpass at
        // 1 kHz scaled to 0.01: -40 dB above the corner.
        let sup: Vec<Complex> = f
            .iter()
            .map(|&f| {
                let x = Complex::new(0.0, f / 1e3);
                Complex::new(0.01, 0.0) * x / (Complex::new(1.0, 0.0) + x)
            })
            .collect();
        let main = ac(f.clone(), vec![("V(out)", sig)]);
        let mut derived = ac(f.clone(), vec![("V(out)", sup)]);
        crate::stimulus::mark(&mut derived, "VDD");
        let runs = [main.clone(), derived];
        let r = measure("psrr", &m("psrr_db_at(V(out), VDD, 1Meg)"), &runs);
        rel(r.value.unwrap(), 80.0, 1e-6);
        assert_eq!(r.unit, "dB");
        let r = measure("psrr", &m("psrr_db_at(V(out), vdd, 1k)"), &runs);
        rel(r.value.unwrap(), 80.0 + 3.0103, 1e-4);
        // A plain AC measurement keeps reading the circuit's own run.
        let g = measure("g", &m("gain_db_at(V(out), 1Meg)"), &runs);
        rel(g.value.unwrap(), 40.0, 1e-9);
        // Without the derived run there is no answer, and it says why.
        let r = measure("c", &m("cmrr_db_at(V(out), Vcm, 1k)"), &runs);
        assert!(r.note.unwrap().contains("AC 1` on Vcm"));
        let r = measure("c", &m("cmrr_db_at(V(out), VDD, 1k)"), &[main]);
        assert!(r.value.is_none());
        assert!(Measure::parse("psrr_db_at(V(out), V(dd), 1k)").is_err());
        assert_eq!(
            m("psrr_db_at(V(out), VDD, 1k)").to_string(),
            "psrr_db_at(V(out), VDD, 1k)"
        );
    }

    /// First-order step response v = 1 - exp(-t/tau), on a grid that is
    /// dense near zero as a simulator's would be.
    fn rc_step(tau: f64) -> Dataset {
        let t: Vec<f64> = (0..8001)
            .map(|i| {
                let u = i as f64 / 8000.0;
                20.0 * tau * u.powf(1.5)
            })
            .collect();
        let v: Vec<f64> = t.iter().map(|t| 1.0 - (-t / tau).exp()).collect();
        tran(t, vec![("V(out)", v)])
    }

    /// Underdamped second-order step response with natural frequency `wn`
    /// and damping `zeta`.
    fn second_order(wn: f64, zeta: f64, t_end: f64, n: usize) -> (Vec<f64>, Vec<f64>) {
        let t = linspace(0.0, t_end, n);
        let wd = wn * (1.0 - zeta * zeta).sqrt();
        let phi = (zeta / (1.0 - zeta * zeta).sqrt()).atan();
        let y = t
            .iter()
            .map(|&t| {
                1.0 - (-zeta * wn * t).exp() / (1.0 - zeta * zeta).sqrt()
                    * (wd * t + (PI / 2.0 - phi)).sin()
            })
            .collect();
        (t, y)
    }

    #[test]
    fn json_round_trip_and_lenient_numbers() {
        let json = r#"{"kind":"gain_db_at","expr":"V(out)/V(in)","freq":"1k"}"#;
        let got: Measure = serde_json::from_str(json).unwrap();
        assert_eq!(
            got,
            Measure::GainDbAt {
                expr: "V(out)/V(in)".into(),
                freq: 1e3
            }
        );
        let json = r#"{"kind":"min","expr":"V(out)","to":"1m"}"#;
        let got: Measure = serde_json::from_str(json).unwrap();
        assert_eq!(
            got,
            Measure::Min {
                expr: "V(out)".into(),
                from: None,
                to: Some(1e-3)
            }
        );
        let json = r#"{"kind":"rise_time","expr":"V(out)"}"#;
        let got: Measure = serde_json::from_str(json).unwrap();
        assert!(
            matches!(got, Measure::RiseTime { low_pct, high_pct, .. } if low_pct == 10.0 && high_pct == 90.0)
        );
        let bad = r#"{"kind":"rise_time","expr":"V(out)","lowpct":5}"#;
        assert!(serde_json::from_str::<Measure>(bad).is_err());
        let bad = r#"{"kind":"value_at","expr":"V(out)","at":"soon"}"#;
        assert!(
            serde_json::from_str::<Measure>(bad)
                .unwrap_err()
                .to_string()
                .contains("soon")
        );
        // Every kind's serde tag matches kind().
        for k in KINDS {
            let sig = signature(k).unwrap();
            let args: Vec<&str> = sig
                .iter()
                .filter(|p| matches!(p.dflt, Dflt::Required))
                .map(|p| match p.arg {
                    Arg::Expr => "V(out)",
                    _ => "1",
                })
                .collect();
            let measure = m(&format!("{k}({})", args.join(", ")));
            assert_eq!(measure.kind(), *k);
            let v = serde_json::to_value(&measure).unwrap();
            assert_eq!(v["kind"], *k);
            let back: Measure = serde_json::from_value(v).unwrap();
            assert_eq!(back, measure);
        }
    }

    #[test]
    fn schema_lists_every_kind() {
        let schema = schemars::schema_for!(Measure);
        let text = serde_json::to_string(&schema).unwrap();
        for k in KINDS {
            assert!(
                text.contains(&format!("\"{k}\"")),
                "{k} missing from schema"
            );
        }
        assert!(text.contains("tolerance_pct"));
    }

    #[test]
    fn text_form_round_trips() {
        for text in [
            "gain_db_at(V(out)/V(in), 1k)",
            "crossing(V(out), 2.5, rise, 2)",
            "crossing(V(out), 2.5, nth=-1)",
            "min(V(out), to=1m)",
            "max(V(out), 100u, 1m)",
            "rise_time(V(out))",
            "rise_time(V(out), 20, 80)",
            "fall_time(V(out), high_pct=95)",
            "settling_time(V(out), 0.1)",
            "delay(V(in), V(out))",
            "delay(V(in), V(out), 10)",
            "thd(V(out), 1k)",
            "thd(V(out), 1k, 5, 3)",
            "bandwidth_3db(V(out), peak)",
            "bandwidth_3db(V(a,b))",
            "phase_margin(V(out))",
            "freq_at_db(V(out), -20)",
            "value_at(I(R1), 1.2345678m)",
        ] {
            let parsed = m(text);
            assert_eq!(parsed.to_string(), text);
            assert_eq!(m(&parsed.to_string()), parsed);
        }
        assert_eq!(
            m("CROSSING( V(out) , level = 1 , direction = Rising )").to_string(),
            "crossing(V(out), 1, rise)"
        );
        assert_eq!(
            m("bandwidth_3db(V(out), reference=dc)").to_string(),
            "bandwidth_3db(V(out))"
        );
    }

    #[test]
    fn text_form_errors() {
        for (text, needle) in [
            ("nope(V(out))", "unknown measurement"),
            ("max V(out)", "not a measurement name"),
            ("max", "expected a measurement"),
            ("max(V(out)", "unclosed"),
            ("value_at(V(out))", "needs `at`"),
            ("value_at(V(out), soon)", "should be a number"),
            ("max(V(out), 1, 2, 3)", "at most 3"),
            ("max(V(out), until=3)", "no argument `until`"),
            ("max(V(out), to=1, 2)", "positional argument"),
            ("max(V(out), to=1, to=2)", "twice"),
            ("crossing(V(out), 1, sideways)", "rise, fall or either"),
            ("crossing(V(out), 1, rise, 1.5)", "whole number"),
            ("max(V(out)) extra", "unexpected `extra`"),
            ("max(V(out)+)", "expected a value"),
            ("bandwidth_3db(V(out), top)", "dc or peak"),
        ] {
            let err = Measure::parse(text).unwrap_err().to_string();
            assert!(err.contains(needle), "{text}: {err}");
        }
    }

    #[test]
    fn first_order_step() {
        let tau = 1e-3;
        let ds = rc_step(tau);
        // 10-90% rise time of a first-order system is tau ln 9.
        rel(val(&ds, "rise_time(V(out))"), tau * 9f64.ln(), 1e-4);
        rel(val(&ds, "rise_time(V(out), 20, 80)"), tau * 4f64.ln(), 1e-4);
        // Within 2% of the step around the last sample: about tau ln 50.
        let final_v = 1.0 - (-20f64).exp();
        let band = 0.02 * final_v;
        let want = -tau * (1.0 - final_v + band).ln();
        rel(val(&ds, "settling_time(V(out))"), want, 1e-3);
        assert_eq!(val(&ds, "overshoot_pct(V(out))"), 0.0);
        rel(val(&ds, "crossing(V(out), 0.5)"), tau * 2f64.ln(), 1e-5);
        rel(val(&ds, "value_at(V(out), 1m)"), 1.0 - (-1f64).exp(), 1e-6);
        // Integral of 1 - exp(-t/tau) over 0..T is T - tau (1 - exp(-T/tau)).
        let t_end = 20.0 * tau;
        rel(
            val(&ds, "integral(V(out))"),
            t_end - tau * (1.0 - (-20f64).exp()),
            1e-5,
        );
        rel(
            val(&ds, "avg(V(out), 2m, 4m)"),
            (2e-3 - tau * ((-2f64).exp() - (-4f64).exp())) / 2e-3,
            1e-5,
        );
        let note = none(&ds, "fall_time(V(out))");
        assert!(note.contains("use rise_time"), "{note}");
        let r = measure_dataset("vmax", &m("max(V(out))"), &ds);
        assert_eq!(r.unit, "V");
        assert_eq!(r.at_unit, "s");
        rel(r.at.unwrap(), t_end, 1e-12);
        let r = measure_dataset("rise", &m("rise_time(V(out))"), &ds);
        assert_eq!(r.unit, "s");
        assert!(
            r.display().starts_with("rise = 2.197ms (t_low"),
            "{}",
            r.display()
        );
    }

    #[test]
    fn falling_exponential() {
        let tau = 2e-6;
        let t = linspace(0.0, 20.0 * tau, 8001);
        let v: Vec<f64> = t.iter().map(|t| 5.0 * (-t / tau).exp()).collect();
        let ds = tran(t, vec![("V(x)", v)]);
        rel(val(&ds, "fall_time(V(x))"), tau * 9f64.ln(), 1e-4);
        assert!(none(&ds, "rise_time(V(x))").contains("use fall_time"));
        rel(val(&ds, "crossing(V(x), 2.5, fall)"), tau * 2f64.ln(), 1e-5);
        assert!(none(&ds, "crossing(V(x), 2.5, rise)").contains("never crosses 2.5V rising"));
    }

    #[test]
    fn second_order_step() {
        let wn = 2.0 * PI * 1e3;
        for zeta in [0.2, 0.5, 0.7] {
            let (t, y) = second_order(wn, zeta, 20e-3, 40001);
            let ds = tran(t, vec![("V(out)", y)]);
            let os = 100.0 * (-zeta * PI / (1.0 - zeta * zeta).sqrt()).exp();
            rel(val(&ds, "overshoot_pct(V(out))"), os, 1e-4);
            let r = measure_dataset("os", &m("overshoot_pct(V(out))"), &ds);
            let tp = PI / (wn * (1.0 - zeta * zeta).sqrt());
            rel(r.at.unwrap(), tp, 1e-3);
            assert_eq!(r.unit, "%");
            assert_eq!(val(&ds, "undershoot_pct(V(out))"), 0.0);
            // Settling to 2%: check the defining property directly rather than
            // the 4/(zeta wn) rule of thumb, which is only an envelope bound.
            let ts = val(&ds, "settling_time(V(out))");
            let (tt, yy) = second_order(wn, zeta, 20e-3, 40001);
            let yf = *yy.last().unwrap();
            for (ti, yi) in tt.iter().zip(&yy) {
                if *ti > ts + 1e-9 {
                    assert!((yi - yf).abs() <= 0.02 * yf + 1e-9, "zeta {zeta} t {ti}");
                }
            }
            let before = tt
                .iter()
                .zip(&yy)
                .rfind(|(ti, _)| **ti < ts - 2e-6)
                .unwrap();
            assert!(
                (before.1 - yf).abs() > 0.02 * yf * 0.9,
                "zeta {zeta}: settles too late"
            );
            assert!(ts < 4.5 / (zeta * wn) && ts > 2.0 / (zeta * wn));
        }
    }

    #[test]
    fn sine_statistics() {
        let f = 1e3;
        let t = linspace(0.0, 5e-3, 5001);
        let y: Vec<f64> = t
            .iter()
            .map(|t| 0.5 + 2.0 * (2.0 * PI * f * t).sin())
            .collect();
        let ds = tran(t, vec![("V(s)", y)]);
        rel(val(&ds, "pp(V(s))"), 4.0, 1e-5);
        rel(val(&ds, "avg(V(s))"), 0.5, 1e-6);
        rel(val(&ds, "rms(V(s))"), (0.25f64 + 2.0).sqrt(), 1e-5);
        rel(val(&ds, "frequency(V(s))"), f, 1e-6);
        rel(val(&ds, "period(V(s))"), 1e-3, 1e-6);
        rel(val(&ds, "duty_cycle(V(s))"), 50.0, 1e-4);
        rel(val(&ds, "max(V(s), 1m, 2m)"), 2.5, 1e-5);
        let r = measure_dataset("mx", &m("max(V(s), 1m, 2m)"), &ds);
        rel(r.at.unwrap(), 1.25e-3, 1e-6);
        rel(val(&ds, "min(V(s))"), -1.5, 1e-5);
        // The first rising crossing of the mean is t = 0, a sample on the
        // level; the first counted crossing is the next one.
        rel(val(&ds, "crossing(V(s), 0.5, rise)"), 1e-3, 1e-6);
        rel(val(&ds, "crossing(V(s), 0.5, fall, 2)"), 1.5e-3, 1e-6);
        rel(val(&ds, "crossing(V(s), 0.5, either, -1)"), 4.5e-3, 1e-6);
        assert!(none(&ds, "crossing(V(s), 0.5, rise, 9)").contains("only 4 times"));
        assert!(none(&ds, "crossing(V(s), 0.5, rise, 0)").contains("counts from 1"));
        assert!(none(&ds, "crossing(V(s), 9)").contains("never crosses 9V"));
    }

    #[test]
    fn noisy_square_wave_frequency_and_duty() {
        // 30% duty square wave with slow edges and a little deterministic
        // chatter on top, which must not add crossings.
        let f = 10e3;
        let t = linspace(0.0, 1e-3, 100_001);
        let y: Vec<f64> = t
            .iter()
            .enumerate()
            .map(|(i, &t)| {
                let phase = (t * f).fract();
                let base = if phase < 0.3 { 3.3 } else { 0.0 };
                base + 0.01 * ((i * 7919 % 13) as f64 - 6.0)
            })
            .collect();
        let ds = tran(t, vec![("V(clk)", y)]);
        rel(val(&ds, "frequency(V(clk))"), f, 1e-4);
        rel(val(&ds, "duty_cycle(V(clk))"), 30.0, 1e-2);
    }

    #[test]
    fn thd_of_known_harmonics() {
        let f = 1e3;
        // Non-uniform sampling with a DC offset and a start-up transient that
        // the end-of-run analysis must ignore.
        let t: Vec<f64> = (0..60001)
            .map(|i| {
                let u = i as f64 / 60000.0;
                10e-3 * (u + 0.002 * (2.0 * PI * 37.0 * u).sin())
            })
            .collect();
        let y: Vec<f64> = t
            .iter()
            .map(|&t| {
                let w = 2.0 * PI * f * t;
                0.3 + w.sin()
                    + 0.1 * (3.0 * w).sin()
                    + 0.05 * (5.0 * w + 0.4).cos()
                    + 3.0 * (-t / 1e-4).exp()
            })
            .collect();
        let ds = tran(t, vec![("V(out)", y)]);
        let want = (0.1f64 * 0.1 + 0.05 * 0.05).sqrt() * 100.0;
        rel(val(&ds, "thd(V(out), 1k)"), want, 1e-3);
        rel(val(&ds, "thd(V(out), 1k, 9, 4)"), want, 1e-3);
        // Only up to the 3rd harmonic.
        rel(val(&ds, "thd(V(out), 1k, 3)"), 10.0, 1e-3);
        let r = measure_dataset("thd", &m("thd(V(out), 1k)"), &ds);
        assert_eq!(r.unit, "%");
        rel(r.extra[0].value, 1.0, 1e-4);
        assert_eq!(r.extra[0].name, "fundamental");
        rel(r.extra[2].value, 0.1, 1e-3);
        assert!(none(&ds, "thd(V(out), 10)").contains("at least one full period"));
        assert!(none(&ds, "thd(V(out), 1k, 1)").contains("at least 2"));
        // A pure sine has no distortion.
        let t = linspace(0.0, 3e-3, 30001);
        let y: Vec<f64> = t.iter().map(|&t| (2.0 * PI * f * t).sin()).collect();
        let ds = tran(t, vec![("V(out)", y)]);
        assert!(val(&ds, "thd(V(out), 1k)") < 1e-3);
    }

    #[test]
    fn delay_between_steps_and_clocks() {
        let tau = 1e-6;
        let t = linspace(0.0, 20e-6, 20001);
        let input: Vec<f64> = t
            .iter()
            .map(|&t| if t >= 2e-6 { 1.0 } else { 0.0 })
            .collect();
        let out: Vec<f64> = t
            .iter()
            .map(|&t| {
                if t < 2e-6 {
                    0.0
                } else {
                    1.0 - (-(t - 2e-6) / tau).exp()
                }
            })
            .collect();
        let ds = tran(t.clone(), vec![("V(in)", input), ("V(out)", out)]);
        rel(
            val(&ds, "delay(V(in), V(out))"),
            tau * 2f64.ln() + 0.5e-9,
            1e-3,
        );
        // An inverter: output falls when input rises; no net transition on
        // either clock, so levels come from min and max.
        let clk: Vec<f64> = t
            .iter()
            .map(|&t| if (t * 1e5).fract() < 0.5 { 0.0 } else { 1.0 })
            .collect();
        let inv: Vec<f64> = t
            .iter()
            .map(|&t| {
                // High before the first edge and in the first half of
                // each period, one microsecond behind the clock.
                let s = t - 1e-6;
                if s < 0.0 || (s * 1e5).fract() < 0.5 {
                    1.0
                } else {
                    0.0
                }
            })
            .collect();
        let ds = tran(t, vec![("V(clk)", clk), ("V(inv)", inv)]);
        rel(val(&ds, "delay(V(clk), V(inv))"), 1e-6, 2e-3);
    }

    #[test]
    fn load_transient_without_net_step() {
        // A regulator output that dips and recovers to the same value.
        let t = linspace(0.0, 1e-3, 10001);
        let v: Vec<f64> = t
            .iter()
            .map(|&t| {
                if t < 2e-4 {
                    3.3
                } else {
                    3.3 - 0.165 * (-(t - 2e-4) / 5e-5).exp() * (2.0 * PI * 2e4 * (t - 2e-4)).cos()
                }
            })
            .collect();
        let ds = tran(t, vec![("V(out)", v)]);
        let r = measure_dataset("u", &m("undershoot_pct(V(out))"), &ds);
        rel(r.value.unwrap(), 5.0, 1e-3);
        assert!(r.note.unwrap().contains("percent of the final value"));
        assert!(val(&ds, "overshoot_pct(V(out))") > 0.0);
        assert!(none(&ds, "rise_time(V(out))").contains("no net transition"));
        let ts = val(&ds, "settling_time(V(out), 1)");
        assert!(ts > 2e-4 && ts < 6e-4, "{ts}");
    }

    /// Second-order lowpass 1 / (1 - (f/f0)^2 + j f/(f0 Q)).
    fn lowpass2(f0: f64, q: f64) -> Dataset {
        let f = logspace(10.0, 1e6, 301);
        let h: Vec<Complex> = f
            .iter()
            .map(|&f| {
                let u = f / f0;
                Complex::new(1.0, 0.0) / Complex::new(1.0 - u * u, u / q)
            })
            .collect();
        ac(f, vec![("V(out)", h)])
    }

    /// A Butterworth claim is a claim about Q; these two measure the shape
    /// that bandwidth_3db alone does not pin down.
    #[test]
    fn second_order_shape() {
        let butter = lowpass2(1e3, std::f64::consts::FRAC_1_SQRT_2);
        rel(
            val(&butter, "q_lowpass(V(out))"),
            std::f64::consts::FRAC_1_SQRT_2,
            2e-3,
        );
        assert!(val(&butter, "peaking_db(V(out))").abs() < 1e-6);
        rel(val(&butter, "bandwidth_3db(V(out))"), 1e3, 3e-3);
        let r = measure_dataset("q", &m("q_lowpass(V(out))"), &butter);
        let f0 = r.extra.iter().find(|x| x.name == "f0").unwrap().value;
        rel(f0, 1e3, 2e-3);
        // Peaking: Q = 2 peaks at Q / sqrt(1 - 1/(4 Q^2)).
        let peaky = lowpass2(1e3, 2.0);
        rel(val(&peaky, "q_lowpass(V(out))"), 2.0, 5e-3);
        rel(
            val(&peaky, "peaking_db(V(out))"),
            20.0 * (2.0 / (1.0f64 - 1.0 / 16.0).sqrt()).log10(),
            1e-2,
        );
        // Two real poles (Q = 0.5): no peaking, Q still read off.
        let slow = lowpass2(1e3, 0.5);
        rel(val(&slow, "q_lowpass(V(out))"), 0.5, 5e-3);
        // A first-order response never turns 90 degrees.
        let f = logspace(10.0, 1e6, 61);
        let h: Vec<Complex> = f.iter().map(|&f| poles(f, &[1e3], 1.0)).collect();
        let first = ac(f, vec![("V(out)", h)]);
        assert!(none(&first, "q_lowpass(V(out))").contains("90 degrees"));
    }

    #[test]
    fn single_pole_ac() {
        let fc = 1591.5494309189535;
        let f = logspace(1.0, 1e6, 61);
        let h: Vec<Complex> = f.iter().map(|&f| poles(f, &[fc], 10.0)).collect();
        let vin = vec![Complex::new(1.0, 0.0); f.len()];
        let ds = ac(f, vec![("V(out)", h), ("V(in)", vin)]);
        // Ten points per decade, yet log interpolation finds the corner well.
        rel(val(&ds, "bandwidth_3db(V(out)/V(in))"), fc, 2e-3);
        rel(val(&ds, "bandwidth_3db(V(out)/V(in), peak)"), fc, 2e-3);
        rel(
            val(&ds, "gain_db_at(V(out)/V(in), 100)"),
            20.0 - 10.0 * (1.0 + (100.0 / fc).powi(2)).log10(),
            1e-3,
        );
        rel(
            val(&ds, "phase_at(V(out), 1k)"),
            -(1e3 / fc).atan().to_degrees(),
            2e-3,
        );
        // Gain 10 crosses 0 dB where |H| = 1: f = fc sqrt(99).
        rel(val(&ds, "unity_gain_freq(V(out))"), fc * 99f64.sqrt(), 2e-3);
        rel(
            val(&ds, "phase_margin(V(out))"),
            180.0 - 99f64.sqrt().atan().to_degrees(),
            2e-3,
        );
        rel(
            val(&ds, "peak_gain(V(out))"),
            20.0 - 10.0 * (1.0 + (1.0 / fc).powi(2)).log10(),
            1e-6,
        );
        rel(val(&ds, "freq_at_db(V(out), 0)"), fc * 99f64.sqrt(), 2e-3);
        assert!(none(&ds, "gain_margin(V(out))").contains("never reaches -180"));
        assert!(none(&ds, "gain_db_at(V(out), 10Meg)").contains("outside the swept range"));
        assert!(none(&ds, "freq_at_db(V(out), 40)").contains("never crosses 40dB"));
        let r = measure_dataset("g", &m("gain_db_at(V(out)/V(in), 100)"), &ds);
        assert_eq!(r.unit, "dB");
        assert_eq!(r.display(), "g = 19.98dB at 100Hz");
        // Time-domain measurements refuse AC data with a reason.
        let note = none(&ds, "rise_time(V(out))");
        assert!(
            note.contains("needs a transient or DC sweep; the results contain: AC"),
            "{note}"
        );
        // Already-in-dB expressions are not converted twice.
        rel(
            val(&ds, "gain_db_at(db(V(out)), 100)"),
            val(&ds, "gain_db_at(V(out), 100)"),
            1e-12,
        );
    }

    #[test]
    fn two_pole_loop_margins() {
        // Loop gain A0 / ((1 + s/p1)(1 + s/p2)(1 + s/p3)).
        let a0 = 1e4;
        let ps = [10.0, 1e6, 1e7];
        let f = logspace(0.1, 1e9, 401);
        let h: Vec<Complex> = f.iter().map(|&f| poles(f, &ps, a0)).collect();
        // Exact unity-gain frequency by bisection on the closed form.
        let mag = |f: f64| poles(f, &ps, a0).abs();
        let (mut lo, mut hi) = (1.0f64, 1e9f64);
        for _ in 0..200 {
            let mid = (lo * hi).sqrt();
            if mag(mid) > 1.0 { lo = mid } else { hi = mid }
        }
        let fc = lo;
        let phase = |f: f64| -ps.iter().map(|p| (f / p).atan().to_degrees()).sum::<f64>();
        let pm = 180.0 + phase(fc);
        let (mut lo, mut hi) = (1e3f64, 1e9f64);
        for _ in 0..200 {
            let mid = (lo * hi).sqrt();
            if phase(mid) > -180.0 {
                lo = mid
            } else {
                hi = mid
            }
        }
        let f180 = lo;
        let gm = -20.0 * mag(f180).log10();
        let ds = ac(f.clone(), vec![("V(lg)", h.clone())]);
        rel(val(&ds, "unity_gain_freq(V(lg))"), fc, 2e-3);
        rel(val(&ds, "phase_margin(V(lg))"), pm, 5e-3);
        rel(val(&ds, "gain_margin(V(lg))"), gm, 5e-3);
        let r = measure_dataset("gm", &m("gain_margin(V(lg))"), &ds);
        rel(r.at.unwrap(), f180, 2e-3);
        // The same loop probed with inverted sign gives the same margins.
        let neg: Vec<Complex> = h.iter().map(|c| Complex::new(-c.re, -c.im)).collect();
        let ds = ac(f.clone(), vec![("V(lg)", neg)]);
        let r = measure_dataset("pm", &m("phase_margin(V(lg))"), &ds);
        rel(r.value.unwrap(), pm, 5e-3);
        assert!(r.note.unwrap().contains("inverted"));
        rel(val(&ds, "gain_margin(V(lg))"), gm, 5e-3);
        // An integrator loop (phase -90 at low frequency) is not mistaken
        // for an inverted one.
        let integ: Vec<Complex> = f
            .iter()
            .map(|&f| Complex::new(1e6, 0.0) / Complex::new(0.0, f) / Complex::new(1.0, f / 3e6))
            .collect();
        let ds = ac(f, vec![("V(lg)", integ)]);
        let fc = {
            let m = |f: f64| (1e6 / f) / (1.0 + (f / 3e6).powi(2)).sqrt();
            let (mut lo, mut hi) = (1.0f64, 1e9f64);
            for _ in 0..200 {
                let mid = (lo * hi).sqrt();
                if m(mid) > 1.0 { lo = mid } else { hi = mid }
            }
            lo
        };
        rel(
            val(&ds, "phase_margin(V(lg))"),
            90.0 - (fc / 3e6).atan().to_degrees(),
            5e-3,
        );
    }

    #[test]
    fn bandpass_edges() {
        // Second-order bandpass: H = (s/(Q w0)) / (1 + s/(Q w0) + (s/w0)^2).
        let f0 = 10e3;
        let q = 2.0;
        let f = logspace(10.0, 1e7, 301);
        let h: Vec<Complex> = f
            .iter()
            .map(|&f| {
                let x = f / f0;
                let num = Complex::new(0.0, x / q);
                num / Complex::new(1.0 - x * x, x / q)
            })
            .collect();
        let ds = ac(f, vec![("V(bp)", h)]);
        let r = measure_dataset("bw", &m("bandwidth_3db(V(bp), peak)"), &ds);
        // Edges of a second-order bandpass: f0 (sqrt(1 + 1/(4Q^2)) -+ 1/(2Q)).
        let k = (1.0 + 1.0 / (4.0 * q * q)).sqrt();
        let lo = f0 * (k - 1.0 / (2.0 * q));
        let hi = f0 * (k + 1.0 / (2.0 * q));
        rel(r.value.unwrap(), f0 / q, 3e-3);
        rel(r.extra[0].value, lo, 3e-3);
        rel(r.extra[1].value, hi, 3e-3);
        rel(r.extra[2].value, f0, 3e-3);
        assert!(r.display().contains("f_low"), "{}", r.display());
        // A highpass has only a lower edge.
        let f = logspace(10.0, 1e6, 101);
        let hp: Vec<Complex> = f
            .iter()
            .map(|&f| {
                let s = Complex::new(0.0, f / 1e3);
                s / (Complex::new(1.0, 0.0) + s)
            })
            .collect();
        let ds = ac(f, vec![("V(hp)", hp)]);
        let r = measure_dataset("bw", &m("bandwidth_3db(V(hp), peak)"), &ds);
        assert!(r.value.is_none());
        rel(r.extra[0].value, 1e3, 3e-3);
        assert!(none(&ds, "bandwidth_3db(V(hp))").contains("never falls 3 dB"));
    }

    #[test]
    fn stepped_runs_give_one_value_per_step() {
        let parts: Vec<(String, Dataset)> = [1e-3, 2e-3, 4e-3]
            .iter()
            .map(|&tau| (format!("tau={}", format_number(tau)), rc_step(tau)))
            .collect();
        let ds = stepped(parts.iter().map(|(l, d)| (l.as_str(), d.clone())).collect());
        let r = measure_dataset("rise", &m("rise_time(V(out))"), &ds);
        assert_eq!(r.value, None);
        assert_eq!(r.per_step.len(), 3);
        for (s, tau) in r.per_step.iter().zip([1e-3, 2e-3, 4e-3]) {
            rel(s.value.unwrap(), tau * 9f64.ln(), 1e-4);
        }
        assert_eq!(r.per_step[1].label, "tau=2m");
        assert_eq!(
            r.display(),
            "rise: 2.197ms (tau=1m), 4.394ms (tau=2m), 8.789ms (tau=4m)"
        );
        // A measurement that fails on every step says so once.
        let r = measure_dataset("x", &m("crossing(V(out), 5)"), &ds);
        assert!(r.note.unwrap().starts_with("every step: never crosses"));
    }

    #[test]
    fn edge_cases_never_panic() {
        let t = linspace(0.0, 1e-3, 11);
        let nan = vec![f64::NAN; 11];
        let flat = vec![1.0; 11];
        let mut partial = vec![1.0; 11];
        partial[3] = f64::NAN;
        let ds = tran(
            t,
            vec![("V(nan)", nan), ("V(flat)", flat), ("V(p)", partial)],
        );
        for k in KINDS {
            for e in ["V(nan)", "V(flat)", "V(p)", "V(missing)", "d(V(flat))/0"] {
                let sig = signature(k).unwrap();
                let args: Vec<&str> = sig
                    .iter()
                    .filter(|p| matches!(p.dflt, Dflt::Required))
                    .map(|p| match p.arg {
                        Arg::Expr => e,
                        _ => "1",
                    })
                    .collect();
                let measure = m(&format!("{k}({})", args.join(", ")));
                let r = measure_dataset("x", &measure, &ds);
                if r.value.is_none() {
                    assert!(r.note.is_some(), "{k}({e}) gave no value and no note");
                }
                if let Some(v) = r.value {
                    assert!(v.is_finite(), "{k}({e}) = {v}");
                }
                let _ = r.display();
            }
        }
        // Empty windows and windows outside the data.
        let flat_ds = tran(
            linspace(0.0, 1.0, 5),
            vec![("V(x)", vec![1.0, 2.0, 3.0, 2.0, 1.0])],
        );
        assert!(none(&flat_ds, "max(V(x), 2, 1)").contains("is after"));
        assert!(none(&flat_ds, "max(V(x), 5, 6)").contains("outside the data"));
        rel(val(&flat_ds, "max(V(x), 0.1, 0.2)"), 1.8, 1e-12);
        rel(val(&flat_ds, "avg(V(x), 0.5, 0.5)"), 3.0, 1e-12);
        assert!(none(&flat_ds, "value_at(V(x), 2)").contains("outside the simulated range"));
        assert!(none(&flat_ds, "rise_time(V(x), 90, 10)").contains("must be below"));
        // An empty dataset and no datasets at all.
        let empty = tran(Vec::new(), vec![("V(x)", Vec::new())]);
        assert!(none(&empty, "max(V(x))").contains("no points"));
        let r = measure("x", &m("max(V(x))"), &[]);
        assert!(r.note.unwrap().contains("no results"));
        // A parse error in the expression comes back as a note.
        let r = measure(
            "x",
            &Measure::Max {
                expr: "V(".into(),
                from: None,
                to: None,
            },
            &[flat_ds],
        );
        assert!(r.note.unwrap().contains("unclosed"));
    }

    #[test]
    fn picks_the_dataset_by_analysis() {
        let tr = rc_step(1e-3);
        let f = logspace(1.0, 1e6, 61);
        let h: Vec<Complex> = f.iter().map(|&f| poles(f, &[1e3], 1.0)).collect();
        let a = ac(f, vec![("V(out)", h)]);
        let both = vec![tr, a];
        rel(
            measure("bw", &m("bandwidth_3db(V(out))"), &both)
                .value
                .unwrap(),
            1e3,
            2e-3,
        );
        rel(
            measure("r", &m("rise_time(V(out))"), &both).value.unwrap(),
            1e-3 * 9f64.ln(),
            1e-4,
        );
        // General measurements use the first dataset with the vectors.
        assert_eq!(measure("x", &m("max(V(out))"), &both).at_unit, "s");
        let r = measure("x", &m("max(V(nope))"), &both);
        assert!(r.note.unwrap().contains("unknown vector"));
        // Operating point: a single value.
        let mut op = tran(vec![0.0], vec![("V(out)", vec![2.5])]);
        op.axis = None;
        op.kind = AnalysisKind::Op;
        assert_eq!(val(&op, "value_at(V(out), 0)"), 2.5);
        assert_eq!(val(&op, "avg(V(out))"), 2.5);
        assert_eq!(val(&op, "max(V(out))"), 2.5);
    }

    #[test]
    fn human_formatting() {
        assert_eq!(human(-3.0103, "dB"), "-3.01dB");
        assert_eq!(human(0.5, "dB"), "0.5dB");
        assert_eq!(human(45.234, "°"), "45.23°");
        assert_eq!(human(16.3, "%"), "16.3%");
        assert_eq!(human(1591.6, "Hz"), "1.592kHz");
        assert_eq!(human(2.2e6, "Hz"), "2.2MHz");
        assert_eq!(human(1e-3, "s"), "1ms");
        assert_eq!(human(-0.00001, "dB"), "-0.00001dB");
    }

    #[test]
    fn spaced_formatting() {
        assert_eq!(spaced(1591.0, "Hz"), "1.591 kHz");
        assert_eq!(spaced(2.2e6, "Hz"), "2.2 MHz");
        assert_eq!(spaced(4.7e-6, "s"), "4.7 \u{b5}s");
        assert_eq!(spaced(3.3, "V"), "3.3 V");
        assert_eq!(spaced(-3.0103, "dB"), "-3.01 dB");
        assert_eq!(spaced(12.5, ""), "12.5");
    }
}

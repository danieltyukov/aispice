//! Component sizing with the simulator in the loop.
//!
//! The model chooses which values to size and their ranges; a derivative-free
//! optimizer does the search, because simulations are noisy, piecewise and
//! occasionally fail, and language models are poor at numerical search.
//! Both optimizers work in a normalised space where every parameter runs
//! from 0 to 1, logarithmically for resistors, capacitors and inductors, so a
//! step means the same relative change whether a value is 10 Ω or 1 MΩ.
//!
//! The interface is ask and tell: [`Optimizer::ask`] hands out a batch of
//! candidate values, the caller simulates them (in parallel if it likes) and
//! reports the objective with [`Optimizer::tell`]. [`run`] drives that loop
//! with a budget, stops early once every spec passes, snaps the answer to a
//! preferred-number series and keeps a history for the UI.

use crate::montecarlo::Rng;
use crate::spec::SpecReport;
use crate::sweep::{self, SweepError};
use aispice_core::netlist::Netlist;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// IEC 60063 preferred-number series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ESeries {
    E6,
    E12,
    E24,
    E48,
    E96,
}

const E24: [u32; 24] = [
    10, 11, 12, 13, 15, 16, 18, 20, 22, 24, 27, 30, 33, 36, 39, 43, 47, 51, 56, 62, 68, 75, 82, 91,
];

const E96: [u32; 96] = [
    100, 102, 105, 107, 110, 113, 115, 118, 121, 124, 127, 130, 133, 137, 140, 143, 147, 150, 154,
    158, 162, 165, 169, 174, 178, 182, 187, 191, 196, 200, 205, 210, 215, 221, 226, 232, 237, 243,
    249, 255, 261, 267, 274, 280, 287, 294, 301, 309, 316, 324, 332, 340, 348, 357, 365, 374, 383,
    392, 402, 412, 422, 432, 442, 453, 464, 475, 487, 499, 511, 523, 536, 549, 562, 576, 590, 604,
    619, 634, 649, 665, 681, 698, 715, 732, 750, 768, 787, 806, 825, 845, 866, 887, 909, 931, 953,
    976,
];

impl ESeries {
    /// The mantissas of one decade, and how many digits they have.
    pub fn mantissas(self) -> (Vec<u32>, i32) {
        match self {
            ESeries::E6 => (E24.iter().step_by(4).copied().collect(), 2),
            ESeries::E12 => (E24.iter().step_by(2).copied().collect(), 2),
            ESeries::E24 => (E24.to_vec(), 2),
            ESeries::E48 => (E96.iter().step_by(2).copied().collect(), 3),
            ESeries::E96 => (E96.to_vec(), 3),
        }
    }
}

/// `mantissa x 10^exp` computed so the result is the double nearest the
/// decimal, which is what a value written as `4.7k` parses to.
fn decimal(mantissa: u32, exp: i32) -> f64 {
    if exp >= 0 {
        f64::from(mantissa) * 10f64.powi(exp)
    } else {
        f64::from(mantissa) / 10f64.powi(-exp)
    }
}

/// The nearest value in the series, nearest in ratio (log space), which is
/// how the series are designed. Zero, negative and non-finite values come
/// back unchanged.
pub fn snap(value: f64, series: ESeries) -> f64 {
    let (lo, hi) = neighbours(value, series);
    if lo == hi || value.is_nan() || value <= 0.0 {
        return lo;
    }
    if (value / lo).ln() <= (hi / value).ln() {
        lo
    } else {
        hi
    }
}

/// The series values just below (or at) and just above (or at) `value`.
pub fn neighbours(value: f64, series: ESeries) -> (f64, f64) {
    if !(value > 0.0 && value.is_finite()) {
        return (value, value);
    }
    let (m, digits) = series.mantissas();
    let decade = value.log10().floor() as i32;
    let mut candidates = Vec::with_capacity(m.len() * 3);
    for d in [decade - 1, decade, decade + 1] {
        for &k in &m {
            candidates.push(decimal(k, d - (digits - 1)));
        }
    }
    let below = candidates
        .iter()
        .copied()
        .filter(|&c| c <= value)
        .fold(f64::NAN, f64::max);
    let above = candidates
        .iter()
        .copied()
        .filter(|&c| c >= value)
        .fold(f64::NAN, f64::min);
    (below, above)
}

/// A value to size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Param {
    /// An element or `.param` name, as in a sweep.
    pub name: String,
    #[serde(deserialize_with = "crate::measure::lenient::num")]
    pub min: f64,
    #[serde(deserialize_with = "crate::measure::lenient::num")]
    pub max: f64,
    /// Search in log space. Defaults to yes for names starting with R, C or
    /// L, whose sensible ranges span decades.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_scale: Option<bool>,
    /// Round the final answer to this series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snap: Option<ESeries>,
    /// Starting value; the middle of the range (in the search space) if
    /// absent.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::measure::lenient::opt"
    )]
    pub initial: Option<f64>,
}

impl Param {
    pub fn new(name: &str, min: f64, max: f64) -> Self {
        Param {
            name: name.into(),
            min,
            max,
            log_scale: None,
            snap: None,
            initial: None,
        }
    }

    pub fn is_log(&self) -> bool {
        let by_name = matches!(
            self.name.chars().next().map(|c| c.to_ascii_uppercase()),
            Some('R' | 'C' | 'L')
        );
        self.log_scale.unwrap_or(by_name) && self.min > 0.0
    }

    /// Value to normalised position in `[0, 1]`.
    pub fn to_unit(&self, v: f64) -> f64 {
        let u = if self.is_log() {
            (v.ln() - self.min.ln()) / (self.max.ln() - self.min.ln())
        } else {
            (v - self.min) / (self.max - self.min)
        };
        if u.is_finite() {
            u.clamp(0.0, 1.0)
        } else {
            0.5
        }
    }

    /// Normalised position to value.
    pub fn from_unit(&self, u: f64) -> f64 {
        let u = u.clamp(0.0, 1.0);
        if self.is_log() {
            (self.min.ln() + u * (self.max.ln() - self.min.ln())).exp()
        } else {
            self.min + u * (self.max - self.min)
        }
    }

    fn validate(&self) -> Result<(), OptimizeError> {
        let bad = |why: &str| {
            Err(OptimizeError::BadParam {
                name: self.name.clone(),
                why: why.into(),
            })
        };
        if !(self.min.is_finite() && self.max.is_finite()) {
            return bad("bounds must be finite");
        }
        if self.min >= self.max {
            return bad("min must be below max");
        }
        if self.log_scale == Some(true) && self.min <= 0.0 {
            return bad("a log-scale range must be positive");
        }
        if let Some(i) = self.initial
            && !(self.min..=self.max).contains(&i)
        {
            return bad("initial is outside min..max");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OptimizeError {
    #[error("parameter {name}: {why}")]
    BadParam { name: String, why: String },
    #[error("no parameters to optimize")]
    NoParams,
    #[error(transparent)]
    Netlist(#[from] SweepError),
}

/// Apply parameter values to a netlist.
pub fn apply(
    netlist: &Netlist,
    params: &[Param],
    values: &[f64],
) -> Result<Netlist, OptimizeError> {
    let mut n = netlist.clone();
    for (p, v) in params.iter().zip(values) {
        sweep::set_value(&mut n, &p.name, *v)?;
    }
    Ok(n)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Sense {
    Minimize,
    Maximize,
}

/// Something to push on once the specs pass: minimise a spec row's value
/// (power, area) or maximise it (bandwidth, margin).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Goal {
    /// Name of the spec row whose value to push.
    pub spec: String,
    pub sense: Sense,
    /// Weight against the spec violations, which are squared fractions of
    /// their limits. Small weights keep the goal from trading a spec away.
    #[serde(default = "default_weight")]
    pub weight: f64,
}

fn default_weight() -> f64 {
    0.01
}

/// How a spec report becomes one number to minimise.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Objective {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<Goal>,
}

/// What a spec with no value (the measurement failed) costs. It must be
/// worse than any ordinary violation so the search backs away from regions
/// where the circuit stops working, but finite so the simplex still moves.
pub const MISSING_PENALTY: f64 = 1e3;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Evaluation {
    pub value: f64,
    pub feasible: bool,
}

impl Evaluation {
    /// For a run that failed outright (simulator error): worst possible.
    pub fn failed() -> Self {
        Evaluation {
            value: f64::INFINITY,
            feasible: false,
        }
    }
}

impl Objective {
    /// Sum of squared normalised violations, plus squared normalised
    /// distance from target for rows that only have a target, plus the goal
    /// term. Zero (before the goal) exactly when every spec passes.
    pub fn score(&self, report: &SpecReport) -> Evaluation {
        let mut v = 0.0;
        for r in &report.rows {
            let limited = r.min.is_some() || r.max.is_some();
            match (r.value, limited, r.target) {
                (None, true, _) | (None, false, Some(_)) => v += MISSING_PENALTY,
                (Some(_), true, _) => {
                    let viol = r.margin.map(|m| (-m).max(0.0)).unwrap_or(0.0);
                    v += viol * viol;
                }
                (Some(x), false, Some(t)) => {
                    let scale = if t.abs() > 1e-300 { t.abs() } else { 1.0 };
                    v += ((x - t) / scale).powi(2);
                }
                _ => {}
            }
        }
        if let Some(goal) = &self.goal {
            match report.row(&goal.spec).and_then(|r| r.value.map(|x| (r, x))) {
                Some((r, x)) => {
                    let scale = [r.target, r.min, r.max]
                        .into_iter()
                        .flatten()
                        .map(f64::abs)
                        .find(|s| *s > 1e-300)
                        .unwrap_or(1.0);
                    let sign = match goal.sense {
                        Sense::Minimize => 1.0,
                        Sense::Maximize => -1.0,
                    };
                    v += goal.weight * sign * x / scale;
                }
                None => v += MISSING_PENALTY,
            }
        }
        Evaluation {
            value: v,
            feasible: report.all_pass,
        }
    }

    /// The score of the worst case over several reports of the same specs,
    /// one per corner: every spec at the report where it is worst (see
    /// [`crate::corner::worst_case`]), and the goal at its worst value for
    /// its sense, the largest when minimising and the smallest when
    /// maximising. Feasible only when every spec passes in every report.
    pub fn score_worst(&self, reports: &[SpecReport]) -> Evaluation {
        if reports.is_empty() {
            return Evaluation::failed();
        }
        let labelled: Vec<(String, SpecReport)> = reports
            .iter()
            .enumerate()
            .map(|(i, r)| (i.to_string(), r.clone()))
            .collect();
        let mut worst = crate::corner::worst_case(&labelled);
        if let Some(goal) = &self.goal
            && let Some(row) = worst.rows.iter_mut().find(|r| r.name == goal.spec)
        {
            let values: Option<Vec<f64>> = reports
                .iter()
                .map(|r| r.row(&goal.spec).and_then(|x| x.value))
                .collect();
            row.value = values.and_then(|v| match goal.sense {
                Sense::Minimize => v.into_iter().reduce(f64::max),
                Sense::Maximize => v.into_iter().reduce(f64::min),
            });
        }
        self.score(&worst)
    }
}

/// A derivative-free optimizer with an ask/tell interface. Points are in
/// parameter units (ohms, farads), not the normalised space.
pub trait Optimizer {
    /// The next batch of points to evaluate. Empty when finished.
    fn ask(&mut self) -> Vec<Vec<f64>>;
    /// Objective values for the last batch, in the same order.
    fn tell(&mut self, values: &[f64]);
    /// The best point seen and its value.
    fn best(&self) -> Option<(Vec<f64>, f64)>;
    /// Converged with no restarts left.
    fn done(&self) -> bool;
}

fn check_params(params: &[Param]) -> Result<(), OptimizeError> {
    if params.is_empty() {
        return Err(OptimizeError::NoParams);
    }
    params.iter().try_for_each(Param::validate)
}

fn to_values(params: &[Param], u: &[f64]) -> Vec<f64> {
    params.iter().zip(u).map(|(p, &x)| p.from_unit(x)).collect()
}

fn start_point(params: &[Param]) -> Vec<f64> {
    params
        .iter()
        .map(|p| p.initial.map(|i| p.to_unit(i)).unwrap_or(0.5))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NelderMeadOptions {
    /// Initial simplex edge in the normalised space.
    pub step: f64,
    /// Restarts after convergence, around the best point so far.
    pub max_restarts: usize,
    /// Converged when every vertex is this close to the best (normalised).
    pub xtol: f64,
    /// ... or when the values across the simplex differ by less than this.
    pub ftol: f64,
}

impl Default for NelderMeadOptions {
    fn default() -> Self {
        NelderMeadOptions {
            step: 0.2,
            max_restarts: 3,
            xtol: 1e-7,
            ftol: 1e-14,
        }
    }
}

#[derive(Debug, Clone)]
enum NmPhase {
    Init,
    Reflect { centroid: Vec<f64> },
    Expand { xr: Vec<f64>, fr: f64 },
    Contract { fr: f64, inside: bool },
    Shrink,
}

/// Nelder-Mead simplex search with restarts, bounded by clamping to the
/// box. Restarts matter: a simplex can collapse onto a face of the box or a
/// ridge, and a fresh one around the best point usually gets it moving.
#[derive(Debug, Clone)]
pub struct NelderMead {
    params: Vec<Param>,
    opts: NelderMeadOptions,
    simplex: Vec<(Vec<f64>, f64)>,
    pending: Vec<Vec<f64>>,
    phase: NmPhase,
    restarts_left: usize,
    best: Option<(Vec<f64>, f64)>,
    finished: bool,
}

impl NelderMead {
    pub fn new(params: &[Param], opts: NelderMeadOptions) -> Result<Self, OptimizeError> {
        check_params(params)?;
        let mut nm = NelderMead {
            params: params.to_vec(),
            opts,
            simplex: Vec::new(),
            pending: Vec::new(),
            phase: NmPhase::Init,
            restarts_left: opts.max_restarts,
            best: None,
            finished: false,
        };
        nm.init_simplex(&start_point(params));
        Ok(nm)
    }

    fn init_simplex(&mut self, x0: &[f64]) {
        let n = x0.len();
        let mut verts = vec![x0.to_vec()];
        for i in 0..n {
            let mut v = x0.to_vec();
            v[i] = if v[i] + self.opts.step <= 1.0 {
                v[i] + self.opts.step
            } else {
                v[i] - self.opts.step
            };
            verts.push(v);
        }
        self.simplex.clear();
        self.pending = verts;
        self.phase = NmPhase::Init;
    }

    fn note_best(&mut self, x: &[f64], f: f64) {
        if self.best.as_ref().is_none_or(|(_, b)| f < *b) {
            self.best = Some((x.to_vec(), f));
        }
    }

    fn sort(&mut self) {
        self.simplex.sort_by(|a, b| a.1.total_cmp(&b.1));
    }

    fn centroid(&self) -> Vec<f64> {
        let n = self.simplex.len() - 1;
        let mut c = vec![0.0; self.simplex[0].0.len()];
        for (x, _) in &self.simplex[..n] {
            for (ci, xi) in c.iter_mut().zip(x) {
                *ci += xi / n as f64;
            }
        }
        c
    }

    fn converged(&self) -> bool {
        let (best, fb) = &self.simplex[0];
        let fw = self.simplex[self.simplex.len() - 1].1;
        let size = self
            .simplex
            .iter()
            .flat_map(|(x, _)| x.iter().zip(best).map(|(a, b)| (a - b).abs()))
            .fold(0.0, f64::max);
        size < self.opts.xtol || (fw - fb).abs() <= self.opts.ftol * (1.0 + fb.abs())
    }

    /// Start the next iteration, or restart, or finish.
    fn next_iteration(&mut self) {
        self.sort();
        if self.converged() {
            if self.restarts_left == 0 {
                self.finished = true;
                self.pending.clear();
                return;
            }
            self.restarts_left -= 1;
            let best = self
                .best
                .clone()
                .map(|(x, _)| x)
                .unwrap_or_else(|| self.simplex[0].0.clone());
            self.init_simplex(&best);
            return;
        }
        let centroid = self.centroid();
        let worst = &self.simplex[self.simplex.len() - 1].0;
        let xr = clamp_unit(&combine(&centroid, worst, -1.0));
        self.pending = vec![xr];
        self.phase = NmPhase::Reflect { centroid };
    }
}

/// `c + t (x - c)`.
fn combine(c: &[f64], x: &[f64], t: f64) -> Vec<f64> {
    c.iter().zip(x).map(|(ci, xi)| ci + t * (xi - ci)).collect()
}

fn clamp_unit(x: &[f64]) -> Vec<f64> {
    x.iter().map(|v| v.clamp(0.0, 1.0)).collect()
}

fn finite_or_inf(v: f64) -> f64 {
    if v.is_nan() { f64::INFINITY } else { v }
}

impl Optimizer for NelderMead {
    fn ask(&mut self) -> Vec<Vec<f64>> {
        if self.finished {
            return Vec::new();
        }
        self.pending
            .iter()
            .map(|u| to_values(&self.params, u))
            .collect()
    }

    fn tell(&mut self, values: &[f64]) {
        if self.finished || values.len() != self.pending.len() {
            return;
        }
        let pts = std::mem::take(&mut self.pending);
        let values: Vec<f64> = values.iter().copied().map(finite_or_inf).collect();
        for (x, &f) in pts.iter().zip(&values) {
            self.note_best(x, f);
        }
        let last = self.simplex.len().saturating_sub(1);
        match std::mem::replace(&mut self.phase, NmPhase::Init) {
            NmPhase::Init => {
                self.simplex = pts.into_iter().zip(values).collect();
                self.next_iteration();
            }
            NmPhase::Reflect { centroid } => {
                let (xr, fr) = (pts[0].clone(), values[0]);
                let fb = self.simplex[0].1;
                let fsw = self.simplex[last - 1].1;
                let fw = self.simplex[last].1;
                if fr < fb {
                    let xe = clamp_unit(&combine(&centroid, &xr, 2.0));
                    self.pending = vec![xe];
                    self.phase = NmPhase::Expand { xr, fr };
                } else if fr < fsw {
                    self.simplex[last] = (xr, fr);
                    self.next_iteration();
                } else {
                    let inside = fr >= fw;
                    let xc = if inside {
                        combine(&centroid, &self.simplex[last].0, 0.5)
                    } else {
                        combine(&centroid, &xr, 0.5)
                    };
                    self.pending = vec![clamp_unit(&xc)];
                    self.phase = NmPhase::Contract { fr, inside };
                }
            }
            NmPhase::Expand { xr, fr, .. } => {
                let (xe, fe) = (pts[0].clone(), values[0]);
                self.simplex[last] = if fe < fr { (xe, fe) } else { (xr, fr) };
                self.next_iteration();
            }
            NmPhase::Contract { fr, inside, .. } => {
                let (xc, fc) = (pts[0].clone(), values[0]);
                let fw = self.simplex[last].1;
                let accept = if inside { fc < fw } else { fc <= fr };
                if accept {
                    self.simplex[last] = (xc, fc);
                    self.next_iteration();
                } else {
                    let best = self.simplex[0].0.clone();
                    self.pending = self.simplex[1..]
                        .iter()
                        .map(|(x, _)| combine(&best, x, 0.5))
                        .collect();
                    self.phase = NmPhase::Shrink;
                }
            }
            NmPhase::Shrink => {
                for (slot, (x, f)) in self.simplex[1..]
                    .iter_mut()
                    .zip(pts.into_iter().zip(values))
                {
                    *slot = (x, f);
                }
                self.next_iteration();
            }
        }
    }

    fn best(&self) -> Option<(Vec<f64>, f64)> {
        self.best
            .as_ref()
            .map(|(u, f)| (to_values(&self.params, u), *f))
    }

    fn done(&self) -> bool {
        self.finished
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CmaEsOptions {
    /// Initial step size in the normalised space.
    pub sigma0: f64,
    /// Population per generation; `None` for the usual `4 + 3 ln n`.
    pub population: Option<usize>,
    /// Restarts with a doubled population from a random point (IPOP), which
    /// is what makes CMA-ES robust on multimodal problems.
    pub max_restarts: usize,
    pub seed: u64,
    /// Converged when the search distribution is this small (normalised).
    pub xtol: f64,
}

impl Default for CmaEsOptions {
    fn default() -> Self {
        CmaEsOptions {
            sigma0: 0.3,
            population: None,
            max_restarts: 2,
            seed: 1,
            xtol: 1e-9,
        }
    }
}

/// CMA-ES, following Hansen's tutorial (2016), bounded by reflecting
/// samples back into the box. Reflection keeps the distribution unbiased
/// near a bound, where clipping would pile samples onto the face.
#[derive(Debug, Clone)]
pub struct CmaEs {
    params: Vec<Param>,
    opts: CmaEsOptions,
    rng: Rng,
    n: usize,
    lambda: usize,
    mu: usize,
    weights: Vec<f64>,
    mueff: f64,
    cc: f64,
    cs: f64,
    c1: f64,
    cmu: f64,
    damps: f64,
    chi_n: f64,
    mean: Vec<f64>,
    sigma: f64,
    c: Vec<Vec<f64>>,
    b: Vec<Vec<f64>>,
    d: Vec<f64>,
    pc: Vec<f64>,
    ps: Vec<f64>,
    generation: usize,
    pending: Vec<Vec<f64>>,
    best: Option<(Vec<f64>, f64)>,
    recent_ranges: Vec<f64>,
    restarts_left: usize,
    finished: bool,
}

impl CmaEs {
    pub fn new(params: &[Param], opts: CmaEsOptions) -> Result<Self, OptimizeError> {
        check_params(params)?;
        let n = params.len();
        let lambda = opts
            .population
            .unwrap_or(4 + (3.0 * (n as f64).ln()).floor() as usize)
            .max(4);
        let mut es = CmaEs {
            params: params.to_vec(),
            opts,
            rng: Rng::new(opts.seed),
            n,
            lambda,
            mu: 0,
            weights: Vec::new(),
            mueff: 0.0,
            cc: 0.0,
            cs: 0.0,
            c1: 0.0,
            cmu: 0.0,
            damps: 0.0,
            chi_n: 0.0,
            mean: start_point(params),
            sigma: opts.sigma0,
            c: Vec::new(),
            b: Vec::new(),
            d: Vec::new(),
            pc: Vec::new(),
            ps: Vec::new(),
            generation: 0,
            pending: Vec::new(),
            best: None,
            recent_ranges: Vec::new(),
            restarts_left: opts.max_restarts,
            finished: false,
        };
        es.reset(lambda);
        es.sample();
        Ok(es)
    }

    fn reset(&mut self, lambda: usize) {
        let n = self.n as f64;
        self.lambda = lambda;
        self.mu = lambda / 2;
        let raw: Vec<f64> = (0..self.mu)
            .map(|i| (self.mu as f64 + 0.5).ln() - ((i + 1) as f64).ln())
            .collect();
        let sum: f64 = raw.iter().sum();
        self.weights = raw.iter().map(|w| w / sum).collect();
        self.mueff = 1.0 / self.weights.iter().map(|w| w * w).sum::<f64>();
        let mueff = self.mueff;
        self.cc = (4.0 + mueff / n) / (n + 4.0 + 2.0 * mueff / n);
        self.cs = (mueff + 2.0) / (n + mueff + 5.0);
        self.c1 = 2.0 / ((n + 1.3).powi(2) + mueff);
        self.cmu =
            (1.0 - self.c1).min(2.0 * (mueff - 2.0 + 1.0 / mueff) / ((n + 2.0).powi(2) + mueff));
        self.damps = 1.0 + 2.0 * (((mueff - 1.0) / (n + 1.0)).sqrt() - 1.0).max(0.0) + self.cs;
        self.chi_n = n.sqrt() * (1.0 - 1.0 / (4.0 * n) + 1.0 / (21.0 * n * n));
        let k = self.n;
        self.c = identity(k);
        self.b = identity(k);
        self.d = vec![1.0; k];
        self.pc = vec![0.0; k];
        self.ps = vec![0.0; k];
        self.sigma = self.opts.sigma0;
        self.generation = 0;
        self.recent_ranges.clear();
    }

    fn sample(&mut self) {
        let mut out = Vec::with_capacity(self.lambda);
        for _ in 0..self.lambda {
            let z: Vec<f64> = (0..self.n).map(|_| self.rng.normal()).collect();
            let x: Vec<f64> = (0..self.n)
                .map(|i| {
                    let y: f64 = (0..self.n).map(|j| self.b[i][j] * self.d[j] * z[j]).sum();
                    reflect(self.mean[i] + self.sigma * y)
                })
                .collect();
            out.push(x);
        }
        self.pending = out;
    }

    fn restart_or_finish(&mut self) {
        if self.restarts_left == 0 {
            self.finished = true;
            self.pending.clear();
            return;
        }
        self.restarts_left -= 1;
        let lambda = self.lambda * 2;
        self.reset(lambda);
        self.mean = (0..self.n).map(|_| self.rng.next_f64()).collect();
        self.sample();
    }
}

fn identity(n: usize) -> Vec<Vec<f64>> {
    (0..n)
        .map(|i| (0..n).map(|j| if i == j { 1.0 } else { 0.0 }).collect())
        .collect()
}

/// Fold a coordinate back into `[0, 1]` by mirroring at the bounds.
fn reflect(x: f64) -> f64 {
    if !x.is_finite() {
        return 0.5;
    }
    let m = x.rem_euclid(2.0);
    if m > 1.0 { 2.0 - m } else { m }
}

/// Eigen-decomposition of a small symmetric matrix by cyclic Jacobi
/// rotations: eigenvalues, and eigenvectors as the columns of the second
/// matrix. Plenty for the handful of parameters a circuit is sized over.
#[allow(clippy::needless_range_loop)] // matrix code reads best indexed
fn jacobi_eigen(a: &[Vec<f64>]) -> (Vec<f64>, Vec<Vec<f64>>) {
    let n = a.len();
    let mut m = a.to_vec();
    let mut v = identity(n);
    for _sweep in 0..100 {
        let off: f64 = (0..n)
            .flat_map(|i| (0..n).filter(move |&j| j != i).map(move |j| (i, j)))
            .map(|(i, j)| m[i][j] * m[i][j])
            .sum();
        if off < 1e-30 {
            break;
        }
        for p in 0..n {
            for q in p + 1..n {
                if m[p][q].abs() < 1e-300 {
                    continue;
                }
                let theta = (m[q][q] - m[p][p]) / (2.0 * m[p][q]);
                let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let t = if theta == 0.0 { 1.0 } else { t };
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                for k in 0..n {
                    let (mkp, mkq) = (m[k][p], m[k][q]);
                    m[k][p] = c * mkp - s * mkq;
                    m[k][q] = s * mkp + c * mkq;
                }
                for k in 0..n {
                    let (mpk, mqk) = (m[p][k], m[q][k]);
                    m[p][k] = c * mpk - s * mqk;
                    m[q][k] = s * mpk + c * mqk;
                }
                for row in v.iter_mut() {
                    let (vkp, vkq) = (row[p], row[q]);
                    row[p] = c * vkp - s * vkq;
                    row[q] = s * vkp + c * vkq;
                }
            }
        }
    }
    ((0..n).map(|i| m[i][i]).collect(), v)
}

impl Optimizer for CmaEs {
    fn ask(&mut self) -> Vec<Vec<f64>> {
        if self.finished {
            return Vec::new();
        }
        self.pending
            .iter()
            .map(|u| to_values(&self.params, u))
            .collect()
    }

    #[allow(clippy::needless_range_loop)] // the update equations read best indexed
    fn tell(&mut self, values: &[f64]) {
        if self.finished || values.len() != self.pending.len() {
            return;
        }
        let n = self.n;
        let xs = std::mem::take(&mut self.pending);
        let values: Vec<f64> = values.iter().copied().map(finite_or_inf).collect();
        let mut order: Vec<usize> = (0..xs.len()).collect();
        order.sort_by(|&a, &b| values[a].total_cmp(&values[b]));
        let fbest = values[order[0]];
        if self.best.as_ref().is_none_or(|(_, b)| fbest < *b) {
            self.best = Some((xs[order[0]].clone(), fbest));
        }
        let old = self.mean.clone();
        let sigma = self.sigma;
        let ys: Vec<Vec<f64>> = order[..self.mu]
            .iter()
            .map(|&k| (0..n).map(|i| (xs[k][i] - old[i]) / sigma).collect())
            .collect();
        let yw: Vec<f64> = (0..n)
            .map(|i| ys.iter().zip(&self.weights).map(|(y, w)| w * y[i]).sum())
            .collect();
        self.mean = (0..n).map(|i| old[i] + sigma * yw[i]).collect();
        // C^(-1/2) yw = B D^-1 B^T yw.
        let bt_yw: Vec<f64> = (0..n)
            .map(|j| (0..n).map(|i| self.b[i][j] * yw[i]).sum::<f64>() / self.d[j])
            .collect();
        let c_inv_sqrt_yw: Vec<f64> = (0..n)
            .map(|i| (0..n).map(|j| self.b[i][j] * bt_yw[j]).sum())
            .collect();
        let k_s = (self.cs * (2.0 - self.cs) * self.mueff).sqrt();
        for i in 0..n {
            self.ps[i] = (1.0 - self.cs) * self.ps[i] + k_s * c_inv_sqrt_yw[i];
        }
        self.generation += 1;
        let ps_norm = self.ps.iter().map(|v| v * v).sum::<f64>().sqrt();
        let denom = (1.0 - (1.0 - self.cs).powi(2 * self.generation as i32)).sqrt();
        let hsig = ps_norm / denom / self.chi_n < 1.4 + 2.0 / (n as f64 + 1.0);
        let k_c = (self.cc * (2.0 - self.cc) * self.mueff).sqrt();
        for i in 0..n {
            self.pc[i] = (1.0 - self.cc) * self.pc[i] + if hsig { k_c * yw[i] } else { 0.0 };
        }
        let delta = if hsig { 0.0 } else { self.cc * (2.0 - self.cc) };
        for i in 0..n {
            for j in 0..n {
                let rank_mu: f64 = ys
                    .iter()
                    .zip(&self.weights)
                    .map(|(y, w)| w * y[i] * y[j])
                    .sum();
                self.c[i][j] = (1.0 - self.c1 - self.cmu) * self.c[i][j]
                    + self.c1 * (self.pc[i] * self.pc[j] + delta * self.c[i][j])
                    + self.cmu * rank_mu;
            }
        }
        self.sigma *= ((self.cs / self.damps) * (ps_norm / self.chi_n - 1.0)).exp();
        self.sigma = self.sigma.min(1.0);
        // Re-symmetrise against rounding, then decompose.
        for i in 0..n {
            for j in 0..i {
                let avg = (self.c[i][j] + self.c[j][i]) / 2.0;
                self.c[i][j] = avg;
                self.c[j][i] = avg;
            }
        }
        let (eig, vecs) = jacobi_eigen(&self.c);
        self.d = eig.iter().map(|e| e.max(1e-30).sqrt()).collect();
        self.b = vecs;

        let range = values[order[order.len() - 1]] - fbest;
        self.recent_ranges.push(if range.is_finite() {
            range
        } else {
            f64::INFINITY
        });
        let dmax = self.d.iter().copied().fold(0.0, f64::max);
        let dmin = self.d.iter().copied().fold(f64::INFINITY, f64::min);
        let window = 10 + (30.0 * n as f64 / self.lambda as f64).ceil() as usize;
        let flat = self.recent_ranges.len() >= window
            && self.recent_ranges[self.recent_ranges.len() - window..]
                .iter()
                .all(|r| *r <= 1e-14 * (1.0 + fbest.abs()));
        let tiny = self.sigma * dmax < self.opts.xtol;
        let ill = dmax / dmin > 1e7;
        if tiny || flat || ill || !self.sigma.is_finite() {
            self.restart_or_finish();
        } else {
            self.sample();
        }
    }

    fn best(&self) -> Option<(Vec<f64>, f64)> {
        self.best
            .as_ref()
            .map(|(u, f)| (to_values(&self.params, u), *f))
    }

    fn done(&self) -> bool {
        self.finished
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunOptions {
    /// Hard limit on evaluations, not counting the final snapping checks.
    pub max_evals: usize,
    /// Stop at the first point where every spec passes. Turn off when there
    /// is a goal to push on.
    pub stop_when_feasible: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            max_evals: 200,
            stop_when_feasible: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryEntry {
    /// 1-based evaluation count.
    pub eval: usize,
    pub values: Vec<f64>,
    pub objective: f64,
    pub feasible: bool,
    /// Best objective so far, for a convergence plot.
    pub best: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Feasible,
    Converged,
    Budget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Snapped {
    pub values: Vec<f64>,
    pub objective: f64,
    pub feasible: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OptimizeResult {
    pub names: Vec<String>,
    /// Best point found, unrounded.
    pub best: Vec<f64>,
    pub objective: f64,
    pub feasible: bool,
    /// The best point rounded to each parameter's series, re-evaluated.
    /// When plain rounding breaks a spec, the neighbouring series values are
    /// tried as well (for up to six snapped parameters).
    pub snapped: Option<Snapped>,
    pub evaluations: usize,
    pub stop: StopReason,
    pub history: Vec<HistoryEntry>,
}

/// Drive an optimizer: ask, evaluate the batch with `evaluate` (which gets
/// parameter values and returns one [`Evaluation`] per point, in order),
/// tell, until feasible, converged or out of budget.
pub fn run<F>(
    opt: &mut dyn Optimizer,
    params: &[Param],
    options: &RunOptions,
    mut evaluate: F,
) -> OptimizeResult
where
    F: FnMut(&[Vec<f64>]) -> Vec<Evaluation>,
{
    let mut history: Vec<HistoryEntry> = Vec::new();
    let mut best: Option<(Vec<f64>, Evaluation)> = None;
    let mut evals = 0usize;
    let record = |points: &[Vec<f64>],
                  results: &[Evaluation],
                  history: &mut Vec<HistoryEntry>,
                  best: &mut Option<(Vec<f64>, Evaluation)>,
                  evals: &mut usize| {
        for (p, r) in points.iter().zip(results) {
            *evals += 1;
            let better = match best {
                None => true,
                // A feasible point beats an infeasible one; otherwise lower wins.
                Some((_, b)) => {
                    (r.feasible && !b.feasible) || (r.feasible == b.feasible && r.value < b.value)
                }
            };
            if better {
                *best = Some((p.clone(), *r));
            }
            history.push(HistoryEntry {
                eval: *evals,
                values: p.clone(),
                objective: r.value,
                feasible: r.feasible,
                best: best.as_ref().map(|(_, b)| b.value).unwrap_or(r.value),
            });
        }
    };
    let stop = loop {
        if evals >= options.max_evals {
            break StopReason::Budget;
        }
        let batch = opt.ask();
        if batch.is_empty() {
            break StopReason::Converged;
        }
        let room = options.max_evals - evals;
        let take = batch.len().min(room);
        let mut results = evaluate(&batch[..take]);
        results.resize(take, Evaluation::failed());
        record(
            &batch[..take],
            &results,
            &mut history,
            &mut best,
            &mut evals,
        );
        let mut values: Vec<f64> = results.iter().map(|r| r.value).collect();
        values.resize(batch.len(), f64::INFINITY);
        opt.tell(&values);
        if options.stop_when_feasible && results.iter().any(|r| r.feasible) {
            break StopReason::Feasible;
        }
        if take < batch.len() {
            break StopReason::Budget;
        }
        if opt.done() {
            break StopReason::Converged;
        }
    };
    let (best_x, best_eval) = best.unwrap_or_else(|| {
        (
            params.iter().map(|p| p.from_unit(0.5)).collect(),
            Evaluation::failed(),
        )
    });
    let snapped = snap_answer(params, &best_x, best_eval, &mut evaluate);
    OptimizeResult {
        names: params.iter().map(|p| p.name.clone()).collect(),
        best: best_x,
        objective: best_eval.value,
        feasible: best_eval.feasible,
        snapped,
        evaluations: evals,
        stop,
        history,
    }
}

fn snap_answer<F>(
    params: &[Param],
    x: &[f64],
    at_best: Evaluation,
    evaluate: &mut F,
) -> Option<Snapped>
where
    F: FnMut(&[Vec<f64>]) -> Vec<Evaluation>,
{
    let snapping: Vec<usize> = (0..params.len())
        .filter(|&i| params[i].snap.is_some())
        .collect();
    if snapping.is_empty() {
        return None;
    }
    let rounded: Vec<f64> = params
        .iter()
        .zip(x)
        .map(|(p, &v)| match p.snap {
            Some(s) => snap(v, s),
            None => v,
        })
        .collect();
    let first = evaluate(std::slice::from_ref(&rounded))
        .into_iter()
        .next()
        .unwrap_or(Evaluation::failed());
    let mut chosen = Snapped {
        values: rounded,
        objective: first.value,
        feasible: first.feasible,
    };
    if chosen.feasible || !at_best.feasible || snapping.len() > 6 {
        return Some(chosen);
    }
    // Rounding to the nearest value broke a spec: try each parameter's
    // series value on either side.
    let mut candidates = Vec::new();
    for mask in 0u32..(1 << snapping.len()) {
        let mut v = x.to_vec();
        for (bit, &i) in snapping.iter().enumerate() {
            let Some(s) = params[i].snap else { continue };
            let (lo, hi) = neighbours(x[i], s);
            v[i] = if mask & (1 << bit) != 0 { hi } else { lo };
        }
        if v != chosen.values && !candidates.contains(&v) {
            candidates.push(v);
        }
    }
    let results = evaluate(&candidates);
    for (v, r) in candidates.into_iter().zip(results) {
        let better = (r.feasible && !chosen.feasible)
            || (r.feasible == chosen.feasible && r.value < chosen.objective);
        if better {
            chosen = Snapped {
                values: v,
                objective: r.value,
                feasible: r.feasible,
            };
        }
    }
    Some(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::Complex;
    use crate::expr::test_support::*;
    use crate::spec::{Spec, evaluate, parse_specs};
    use std::f64::consts::PI;

    #[test]
    fn e_series_tables() {
        assert_eq!(ESeries::E6.mantissas().0, vec![10, 15, 22, 33, 47, 68]);
        assert_eq!(ESeries::E12.mantissas().0.len(), 12);
        assert_eq!(ESeries::E24.mantissas().0.len(), 24);
        assert_eq!(ESeries::E48.mantissas().0.len(), 48);
        assert_eq!(ESeries::E96.mantissas().0.len(), 96);
        assert_eq!(ESeries::E48.mantissas().0[..5], [100, 105, 110, 115, 121]);
        for s in [ESeries::E24, ESeries::E96] {
            let m = s.mantissas().0;
            assert!(m.windows(2).all(|w| w[0] < w[1]));
        }
    }

    #[test]
    fn e_series_rounding() {
        let cases = [
            (4.6e3, ESeries::E12, 4.7e3),
            (1.15, ESeries::E12, 1.2),
            (1.09, ESeries::E12, 1.0),
            (9.5, ESeries::E12, 10.0),
            (9.9e3, ESeries::E24, 10e3),
            (1e-9, ESeries::E24, 1e-9),
            (150.4, ESeries::E96, 150.0),
            (1.234e-6, ESeries::E96, 1.24e-6),
            (5.0e5, ESeries::E6, 4.7e5),
            (3.0e-12, ESeries::E6, 3.3e-12),
            (47e-9, ESeries::E12, 47e-9),
            (0.0, ESeries::E12, 0.0),
            (-5.0, ESeries::E12, -5.0),
        ];
        for (v, s, want) in cases {
            assert_eq!(snap(v, s), want, "{v} in {s:?}");
        }
        // Results are the doubles a SPICE value parses to.
        assert_eq!(
            snap(4.6e3, ESeries::E12),
            crate::expr::parse_spice("4.7k").unwrap()
        );
        assert_eq!(
            snap(46e-9, ESeries::E12),
            crate::expr::parse_spice("47n").unwrap()
        );
        let (lo, hi) = neighbours(5.0e3, ESeries::E12);
        assert_eq!((lo, hi), (4.7e3, 5.6e3));
        // The log midpoint between 1.0 and 1.2 is sqrt(1.2).
        assert_eq!(snap(1.2f64.sqrt() - 1e-9, ESeries::E12), 1.0);
        assert_eq!(snap(1.2f64.sqrt() + 1e-9, ESeries::E12), 1.2);
    }

    #[test]
    fn params_map_to_the_unit_box() {
        let r = Param::new("R1", 100.0, 1e6);
        assert!(r.is_log());
        assert!((r.from_unit(0.5) - 1e4).abs() < 1e-6);
        assert!((r.to_unit(1e3) - 0.25).abs() < 1e-12);
        let g = Param::new("gain", 1.0, 11.0);
        assert!(!g.is_log());
        assert_eq!(g.from_unit(0.5), 6.0);
        let mut forced = Param::new("Vbias", 0.1, 10.0);
        forced.log_scale = Some(true);
        assert!(forced.is_log());
        assert_eq!(r.to_unit(1e9), 1.0);
        assert!(Param::new("R1", 10.0, 1.0).validate().is_err());
        let mut neg = Param::new("x", -1.0, 1.0);
        neg.log_scale = Some(true);
        assert!(neg.validate().is_err());
        let json: Param =
            serde_json::from_str(r#"{"name":"R1","min":"1k","max":"1Meg","snap":"E24"}"#).unwrap();
        assert_eq!(
            (json.min, json.max, json.snap),
            (1e3, 1e6, Some(ESeries::E24))
        );
        assert!(matches!(
            NelderMead::new(&[], Default::default()),
            Err(OptimizeError::NoParams)
        ));
    }

    #[test]
    fn jacobi_matches_a_known_decomposition() {
        let a = vec![
            vec![4.0, 1.0, 0.0],
            vec![1.0, 3.0, 1.0],
            vec![0.0, 1.0, 2.0],
        ];
        let (eig, v) = jacobi_eigen(&a);
        for k in 0..3 {
            // A v = lambda v for each column.
            for i in 0..3 {
                let av: f64 = (0..3).map(|j| a[i][j] * v[j][k]).sum();
                assert!((av - eig[k] * v[i][k]).abs() < 1e-10);
            }
        }
        let mut sorted = eig.clone();
        sorted.sort_by(f64::total_cmp);
        assert!((sorted.iter().sum::<f64>() - 9.0).abs() < 1e-10);
    }

    /// Rosenbrock over log10 of two parameters in 0.01..100: minimum 0 at
    /// a = b = 10.
    fn rosen_params() -> Vec<Param> {
        let mut a = Param::new("a", 0.01, 100.0);
        a.log_scale = Some(true);
        let mut b = Param::new("b", 0.01, 100.0);
        b.log_scale = Some(true);
        vec![a, b]
    }

    /// Evaluations until the best objective first drops below `threshold`.
    fn evals_to(r: &OptimizeResult, threshold: f64) -> Option<usize> {
        r.history
            .iter()
            .find(|h| h.best < threshold)
            .map(|h| h.eval)
    }

    fn rosen(x: &[f64]) -> f64 {
        let (u, v) = (x[0].log10(), x[1].log10());
        (1.0 - u).powi(2) + 100.0 * (v - u * u).powi(2)
    }

    fn analytic(f: impl Fn(&[f64]) -> f64) -> impl FnMut(&[Vec<f64>]) -> Vec<Evaluation> {
        move |batch| {
            batch
                .iter()
                .map(|x| {
                    let v = f(x);
                    Evaluation {
                        value: v,
                        feasible: false,
                    }
                })
                .collect()
        }
    }

    #[test]
    fn nelder_mead_solves_rosenbrock_in_log_space() {
        let params = rosen_params();
        let mut nm = NelderMead::new(&params, NelderMeadOptions::default()).unwrap();
        let opts = RunOptions {
            max_evals: 1500,
            stop_when_feasible: false,
        };
        let r = run(&mut nm, &params, &opts, analytic(rosen));
        let reach = evals_to(&r, 1e-8);
        eprintln!(
            "nelder-mead rosenbrock: below 1e-8 after {reach:?} evaluations; f = {:e} after {} ({:?})",
            r.objective, r.evaluations, r.stop
        );
        assert!(r.objective < 1e-8, "{r:?}");
        assert!(reach.unwrap() <= 400);
        assert!((r.best[0] - 10.0).abs() < 1e-2 && (r.best[1] - 10.0).abs() < 2e-2);
        assert!(r.evaluations <= 1500);
        assert_eq!(r.history.len(), r.evaluations);
        assert!(r.history.windows(2).all(|w| w[1].best <= w[0].best));
    }

    #[test]
    fn cma_es_solves_rosenbrock_in_log_space() {
        let params = rosen_params();
        let mut es = CmaEs::new(&params, CmaEsOptions::default()).unwrap();
        let opts = RunOptions {
            max_evals: 3000,
            stop_when_feasible: false,
        };
        let r = run(&mut es, &params, &opts, analytic(rosen));
        let reach = evals_to(&r, 1e-8);
        eprintln!(
            "cma-es rosenbrock: below 1e-8 after {reach:?} evaluations; f = {:e} after {} ({:?})",
            r.objective, r.evaluations, r.stop
        );
        assert!(r.objective < 1e-8, "{r:?}");
        assert!(reach.unwrap() <= 1500);
        assert!((r.best[0] - 10.0).abs() < 1e-2 && (r.best[1] - 10.0).abs() < 2e-2);
        // Deterministic for a seed.
        let mut again = CmaEs::new(&params, CmaEsOptions::default()).unwrap();
        let r2 = run(&mut again, &params, &opts, analytic(rosen));
        assert_eq!(r.best, r2.best);
        assert_eq!(r.evaluations, r2.evaluations);
    }

    #[test]
    fn cma_es_batches_are_the_population() {
        let params: Vec<Param> = (0..6)
            .map(|i| Param::new(&format!("x{i}"), -1.0, 1.0))
            .collect();
        let mut es = CmaEs::new(&params, CmaEsOptions::default()).unwrap();
        let batch = es.ask();
        assert_eq!(batch.len(), 4 + (3.0 * 6f64.ln()).floor() as usize);
        assert!(batch.iter().flatten().all(|v| (-1.0..=1.0).contains(v)));
        // A sphere in six dimensions with the optimum off-centre.
        let sphere = |x: &[f64]| x.iter().map(|v| (v - 0.3).powi(2)).sum::<f64>();
        let opts = RunOptions {
            max_evals: 4000,
            stop_when_feasible: false,
        };
        let r = run(&mut es, &params, &opts, analytic(sphere));
        let reach = evals_to(&r, 1e-10);
        eprintln!(
            "cma-es 6-d sphere: below 1e-10 after {reach:?} evaluations; f = {:e} after {}",
            r.objective, r.evaluations
        );
        assert!(reach.unwrap() <= 2000);
        let mut nm = NelderMead::new(&params, NelderMeadOptions::default()).unwrap();
        let r = run(&mut nm, &params, &opts, analytic(sphere));
        let reach = evals_to(&r, 1e-10);
        eprintln!(
            "nelder-mead 6-d sphere: below 1e-10 after {reach:?} evaluations; f = {:e} after {}",
            r.objective, r.evaluations
        );
        assert!(reach.unwrap() <= 2000);
    }

    #[test]
    fn optimum_on_a_bound_is_found_by_both() {
        // Minimum of (x - 2)^2 with x limited to 0..1 is at the bound x = 1.
        let params = vec![Param::new("x", 0.0, 1.0), Param::new("y", 0.0, 1.0)];
        let f = |x: &[f64]| (x[0] - 2.0).powi(2) + (x[1] - 0.5).powi(2);
        let opts = RunOptions {
            max_evals: 600,
            stop_when_feasible: false,
        };
        let mut nm = NelderMead::new(&params, NelderMeadOptions::default()).unwrap();
        let r = run(&mut nm, &params, &opts, analytic(f));
        assert!(
            (r.best[0] - 1.0).abs() < 1e-6 && (r.best[1] - 0.5).abs() < 1e-4,
            "{:?}",
            r.best
        );
        let mut es = CmaEs::new(&params, CmaEsOptions::default()).unwrap();
        let r = run(&mut es, &params, &opts, analytic(f));
        assert!(
            (r.best[0] - 1.0).abs() < 1e-4 && (r.best[1] - 0.5).abs() < 1e-4,
            "{:?}",
            r.best
        );
    }

    /// The "simulation" of an RC lowpass driven by V1: an AC dataset from
    /// the closed form, with the input current so a spec can see R.
    fn rc_dataset(r: f64, c: f64) -> crate::dataset::Dataset {
        let f = logspace(10.0, 10e6, 121);
        let mut out = Vec::new();
        let mut vin = Vec::new();
        let mut iin = Vec::new();
        for &fi in &f {
            let zc = Complex::new(0.0, -1.0 / (2.0 * PI * fi * c));
            let z = Complex::new(r, 0.0) + zc;
            let i = Complex::new(1.0, 0.0) / z;
            iin.push(i);
            out.push(i * zc);
            vin.push(Complex::new(1.0, 0.0));
        }
        ac(f, vec![("V(out)", out), ("V(in)", vin), ("I(V1)", iin)])
    }

    fn rc_problem(specs: &str) -> (Vec<Param>, Vec<Spec>) {
        let mut r = Param::new("R1", 100.0, 10e6);
        r.snap = Some(ESeries::E24);
        let mut c = Param::new("C1", 10e-12, 100e-6);
        c.snap = Some(ESeries::E12);
        (vec![r, c], parse_specs(specs).unwrap())
    }

    fn rc_eval(
        specs: Vec<Spec>,
        calls: std::rc::Rc<std::cell::Cell<usize>>,
    ) -> impl FnMut(&[Vec<f64>]) -> Vec<Evaluation> {
        let objective = Objective::default();
        move |batch| {
            batch
                .iter()
                .map(|x| {
                    calls.set(calls.get() + 1);
                    objective.score(&evaluate(&specs, &[rc_dataset(x[0], x[1])]))
                })
                .collect()
        }
    }

    const RC_SPECS: &str = "bw = bandwidth_3db(V(out)/V(in)) in 9.5k..10.5k\n\
                            zin = value_at(mag(V(in)/I(V1)), 1Meg) >= 10k";

    #[test]
    fn sizes_an_rc_filter_with_both_optimizers() {
        for which in ["nelder-mead", "cma-es"] {
            let (params, specs) = rc_problem(RC_SPECS);
            let calls = std::rc::Rc::new(std::cell::Cell::new(0));
            let mut opt: Box<dyn Optimizer> = match which {
                "nelder-mead" => {
                    Box::new(NelderMead::new(&params, NelderMeadOptions::default()).unwrap())
                }
                _ => Box::new(CmaEs::new(&params, CmaEsOptions::default()).unwrap()),
            };
            let r = run(
                opt.as_mut(),
                &params,
                &RunOptions::default(),
                rc_eval(specs.clone(), calls.clone()),
            );
            eprintln!(
                "{which} rc filter: feasible after {} evaluations, R = {:.4e}, C = {:.4e}, snapped {:?}",
                r.evaluations,
                r.best[0],
                r.best[1],
                r.snapped.as_ref().map(|s| (&s.values, s.feasible))
            );
            assert_eq!(r.stop, StopReason::Feasible, "{which}: {:?}", r.stop);
            assert!(r.feasible);
            assert!(
                r.evaluations <= 120,
                "{which}: {} evaluations",
                r.evaluations
            );
            let fc = 1.0 / (2.0 * PI * r.best[0] * r.best[1]);
            assert!((9.5e3..=10.5e3).contains(&fc), "{which}: fc = {fc}");
            assert!(r.best[0] >= 9.9e3);
            // The snapped answer is on the series and still passes.
            let s = r.snapped.unwrap();
            assert_eq!(snap(s.values[0], ESeries::E24), s.values[0]);
            assert_eq!(snap(s.values[1], ESeries::E12), s.values[1]);
            assert!(s.feasible, "{which}: snapped {:?}", s.values);
            let report = evaluate(&specs, &[rc_dataset(s.values[0], s.values[1])]);
            assert!(report.all_pass, "{}", report);
            // Every evaluation went through the callback and is in the history.
            assert!(calls.get() >= r.evaluations);
            assert_eq!(r.history.len(), r.evaluations);
        }
    }

    #[test]
    fn infeasible_specs_end_at_the_best_compromise() {
        // 1 MHz is out of reach with R >= 10k and C >= 1n (at most 15.9 kHz):
        // both optimizers should press against both lower bounds.
        let specs = parse_specs("bw = bandwidth_3db(V(out)/V(in)) >= 1Meg").unwrap();
        let params = vec![Param::new("R1", 10e3, 1e6), Param::new("C1", 1e-9, 1e-6)];
        let fc_max = 1.0 / (2.0 * PI * 10e3 * 1e-9);
        for which in ["nelder-mead", "cma-es"] {
            let calls = std::rc::Rc::new(std::cell::Cell::new(0));
            let mut opt: Box<dyn Optimizer> = match which {
                "nelder-mead" => {
                    Box::new(NelderMead::new(&params, NelderMeadOptions::default()).unwrap())
                }
                _ => Box::new(CmaEs::new(&params, CmaEsOptions::default()).unwrap()),
            };
            let opts = RunOptions {
                max_evals: 300,
                stop_when_feasible: true,
            };
            let r = run(opt.as_mut(), &params, &opts, rc_eval(specs.clone(), calls));
            let fc = 1.0 / (2.0 * PI * r.best[0] * r.best[1]);
            let reach = r
                .history
                .iter()
                .find(|h| 1.0 / (2.0 * PI * h.values[0] * h.values[1]) > 0.97 * fc_max)
                .map(|h| h.eval);
            eprintln!(
                "{which} infeasible: within 3% of the reachable corner after {reach:?} evaluations; best fc = {fc:.1} Hz (limit {fc_max:.1}) after {} evaluations, {:?}",
                r.evaluations, r.stop
            );
            assert!(!r.feasible);
            assert!(r.evaluations <= 300);
            assert!(fc > 0.97 * fc_max, "{which}: fc = {fc}");
            assert!(
                r.best[0] < 10.4e3 && r.best[1] < 1.04e-9,
                "{which}: {:?}",
                r.best
            );
            let expected = (1.0 - fc_max / 1e6).powi(2);
            assert!(
                r.objective >= expected * 0.999,
                "{which}: objective {}",
                r.objective
            );
        }
    }

    #[test]
    fn objective_from_spec_reports() {
        let ds = rc_dataset(10e3, 1.5915e-9);
        let specs = parse_specs(
            "bw = bandwidth_3db(V(out)) >= 20k\n\
             zin = value_at(mag(V(in)/I(V1)), 1Meg) >= 1k\n\
             peak = peak_gain(V(out)) = 1",
        )
        .unwrap();
        let report = evaluate(&specs, &[ds]);
        let s = Objective::default().score(&report);
        assert!(!s.feasible);
        // bw is about 10k against 20k: violation 0.5; zin passes; the
        // target-only row adds the squared distance of ~0 dB from 1 dB.
        let bw_v = (20e3 - report.row("bw").unwrap().value.unwrap()) / 20e3;
        let peak = report.row("peak").unwrap().value.unwrap();
        assert!(
            (s.value - (bw_v * bw_v + (peak - 1.0).powi(2))).abs() < 1e-9,
            "{}",
            s.value
        );
        let goal = Objective {
            goal: Some(Goal {
                spec: "zin".into(),
                sense: Sense::Maximize,
                weight: 0.1,
            }),
        };
        let g = goal.score(&report);
        let zin = report.row("zin").unwrap().value.unwrap();
        assert!((g.value - (s.value - 0.1 * zin / 1e3)).abs() < 1e-9);
        let missing = parse_specs("x = bandwidth_3db(V(nope)) >= 1").unwrap();
        let r = evaluate(&missing, &[rc_dataset(1e3, 1e-9)]);
        assert_eq!(Objective::default().score(&r).value, MISSING_PENALTY);
    }

    /// Corners: the worst report decides, spec by spec, and a goal counts
    /// at its worst corner.
    #[test]
    fn worst_case_objective_over_corners() {
        let specs = parse_specs(
            "bw = bandwidth_3db(V(out)) >= 1k\n\
             peak = peak_gain(V(out))",
        )
        .unwrap();
        let fast = evaluate(&specs, &[rc_dataset(1e3, 100e-9)]);
        let slow = evaluate(&specs, &[rc_dataset(2e3, 100e-9)]);
        let both = Objective::default().score_worst(&[fast.clone(), slow.clone()]);
        assert!(!both.feasible);
        assert_eq!(both.value, Objective::default().score(&slow).value);
        assert!(Objective::default().score_worst(std::slice::from_ref(&fast)).feasible);
        assert!(!Objective::default().score_worst(&[]).feasible);
        let goal = |sense| Objective {
            goal: Some(Goal {
                spec: "bw".into(),
                sense,
                weight: 1.0,
            }),
        };
        let (bw_fast, bw_slow) = (
            fast.row("bw").unwrap().value.unwrap(),
            slow.row("bw").unwrap().value.unwrap(),
        );
        let base = both.value;
        let max = goal(Sense::Maximize).score_worst(&[fast.clone(), slow.clone()]);
        assert!((max.value - (base - bw_slow / 1e3)).abs() < 1e-9);
        let min = goal(Sense::Minimize).score_worst(&[fast, slow]);
        assert!((min.value - (base + bw_fast / 1e3)).abs() < 1e-9);
    }

    #[test]
    fn budget_is_a_hard_limit_and_apply_writes_values() {
        let params = rosen_params();
        let mut es = CmaEs::new(&params, CmaEsOptions::default()).unwrap();
        let opts = RunOptions {
            max_evals: 25,
            stop_when_feasible: false,
        };
        let mut seen = 0;
        let r = run(&mut es, &params, &opts, |batch: &[Vec<f64>]| {
            seen += batch.len();
            batch
                .iter()
                .map(|x| Evaluation {
                    value: rosen(x),
                    feasible: false,
                })
                .collect()
        });
        assert_eq!(r.evaluations, 25);
        assert_eq!(seen, 25);
        assert_eq!(r.stop, StopReason::Budget);
        let n = aispice_core::netlist::parse("t\nR1 a b 1k\nC1 b 0 1n\n.end\n");
        let out = apply(
            &n,
            &[Param::new("R1", 1.0, 1e6), Param::new("C1", 1e-12, 1e-6)],
            &[4.7e3, 22e-9],
        )
        .unwrap();
        assert_eq!(out.element("R1").unwrap().rest, "4.7k");
        assert_eq!(out.element("C1").unwrap().rest, "22n");
        assert!(apply(&n, &[Param::new("R9", 1.0, 2.0)], &[1.5]).is_err());
    }
}

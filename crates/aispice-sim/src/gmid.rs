//! gm/Id lookup tables for a PDK's MOS transistors, and sizing from them.
//!
//! A table is one ngspice run: an instance per channel length, all on the
//! same gate and drain, swept over VGS inside VDS (bulk tied to source),
//! saving the operating point the PSP model reports for each instance (`ids`,
//! `gm`, `gds`, `cgg`, `vth`). It is cached per PDK and device, keyed by the
//! PDK's commit and a fingerprint of its model files, and rebuilt when either
//! changes.
//!
//! Sizing follows the gm/Id method: at a channel length and VDS, the gm/Id
//! the designer wants fixes VGS and the current density Id/W, so a current
//! gives the width (or a width the current). Between table points, VDS is
//! interpolated linearly and L on a log scale, with current density in log,
//! since both vary roughly as powers of L. P-channel tables hold magnitudes
//! (VSG, VSD, |Id|).

use crate::backend::ngspice::Ngspice;
use crate::backend::{SimJob, Simulator};
use crate::dataset::AnalysisKind;
use crate::pdk::{Pdk, PdkRun};
use aispice_core::pdk::{Device, Kit};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Bumped when the table layout changes, so old caches are rebuilt.
pub const FORMAT: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum GmidError {
    #[error("`{0}` is not a MOS transistor of this PDK; tables exist for {1}")]
    NotMos(String, String),
    #[error("the table run was refused: {0}")]
    Policy(String),
    #[error("ngspice: {0}")]
    Sim(String),
    #[error("the table run returned {0}")]
    BadOutput(String),
    #[error("{0}")]
    OutOfRange(String),
    #[error("could not read or write the table cache {}: {message}", .path.display())]
    Cache { path: PathBuf, message: String },
}

/// The bias points and lengths a table covers. Voltages are magnitudes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grid {
    /// Width of every characterised instance, metres, one finger.
    pub w: f64,
    /// Channel lengths, metres, ascending.
    pub l: Vec<f64>,
    pub vds_start: f64,
    pub vds_stop: f64,
    pub vds_step: f64,
    pub vgs_stop: f64,
    pub vgs_step: f64,
}

/// The nominal supply of a device: thick-oxide (`_hv_`) devices run from
/// the high-voltage supply.
pub fn supply(kit: &Kit, device: &Device) -> f64 {
    if device.name.contains("_hv_") {
        kit.vdd_hv
    } else {
        kit.vdd
    }
}

fn points(start: f64, stop: f64, step: f64) -> Vec<f64> {
    let n = ((stop - start) / step).round() as usize + 1;
    (0..n).map(|i| start + step * i as f64).collect()
}

impl Grid {
    /// The default grid for a device: its supply sets the voltages, its
    /// minimum length the shortest L.
    pub fn default_for(kit: &Kit, device: &Device) -> Self {
        let hv = supply(kit, device) > 2.0;
        let l_um: &[f64] = if hv {
            &[0.45, 0.6, 0.8, 1.0, 1.5, 2.0, 3.0, 5.0, 10.0]
        } else {
            &[
                0.13, 0.18, 0.25, 0.35, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 5.0, 10.0,
            ]
        };
        Self {
            w: 5e-6,
            // Divided, not multiplied by 1e-6, so 10 um is exactly 1e-5.
            l: l_um.iter().map(|l| l / 1e6).collect(),
            vds_start: if hv { 0.2 } else { 0.1 },
            vds_stop: if hv { 3.2 } else { 1.2 },
            vds_step: if hv { 0.2 } else { 0.1 },
            vgs_stop: if hv { 3.3 } else { 1.2 },
            vgs_step: if hv { 0.025 } else { 0.01 },
        }
    }

    /// Equal up to rounding: JSON does not bring every float back bit for
    /// bit.
    pub fn same_as(&self, other: &Grid) -> bool {
        let near = |a: f64, b: f64| (a - b).abs() <= 1e-12 * a.abs().max(b.abs());
        near(self.w, other.w)
            && self.l.len() == other.l.len()
            && self.l.iter().zip(&other.l).all(|(a, b)| near(*a, *b))
            && near(self.vds_start, other.vds_start)
            && near(self.vds_stop, other.vds_stop)
            && near(self.vds_step, other.vds_step)
            && near(self.vgs_stop, other.vgs_stop)
            && near(self.vgs_step, other.vgs_step)
    }

    pub fn vds(&self) -> Vec<f64> {
        points(self.vds_start, self.vds_stop, self.vds_step)
    }

    pub fn vgs(&self) -> Vec<f64> {
        points(0.0, self.vgs_stop, self.vgs_step)
    }
}

/// A characterised device. Arrays are indexed `[l][vds][vgs]` and hold
/// magnitudes at the grid's reference width.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub format: u32,
    /// The registered PDK name and what identifies its models.
    pub pdk: String,
    pub commit: Option<String>,
    pub fingerprint: String,
    pub device: String,
    pub pmos: bool,
    /// The corner library section the table was made at.
    pub section: String,
    pub grid: Grid,
    pub vds: Vec<f64>,
    pub vgs: Vec<f64>,
    pub id: Vec<Vec<Vec<f64>>>,
    pub gm: Vec<Vec<Vec<f64>>>,
    pub gds: Vec<Vec<Vec<f64>>>,
    pub cgg: Vec<Vec<Vec<f64>>>,
    pub vth: Vec<Vec<Vec<f64>>>,
}

/// What to size for. Lengths in metres, voltages in volts (magnitudes),
/// current in amperes.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Query {
    pub gm_id: f64,
    pub l: f64,
    pub vds: f64,
    /// The drain current to size the width for.
    pub id: Option<f64>,
    /// Or the width to find the current of.
    pub w: Option<f64>,
}

/// The answer: the bias point, and the device scaled to the requested
/// current or width (`w` and `id` are `None` when neither was given).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sizing {
    pub gm_id: f64,
    pub l: f64,
    pub vds: f64,
    pub vgs: f64,
    pub vth: f64,
    /// Drain current per metre of width, A/m.
    pub id_per_w: f64,
    /// Intrinsic gain gm/gds, independent of width.
    pub gain: f64,
    /// Transit frequency gm / (2 pi Cgg), independent of width.
    pub ft: f64,
    pub w: Option<f64>,
    pub id: Option<f64>,
    pub gm: Option<f64>,
    pub gds: Option<f64>,
    pub cgg: Option<f64>,
    /// Fingers to build the width from, once [`refine`] has chosen them.
    #[serde(default)]
    pub ng: Option<u32>,
    /// Whether the numbers come from simulating this exact device
    /// ([`refine`]) rather than from the table alone.
    #[serde(default)]
    pub verified: bool,
}

/// The table file for a device: `<cache>/<pdk name>/<device>.json`.
pub fn cache_path(cache_dir: &Path, pdk: &str, device: &str) -> PathBuf {
    cache_dir.join(pdk).join(format!("{device}.json"))
}

/// aispice's table cache, `<cache dir>/aispice/gmid`.
pub fn default_cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("aispice").join("gmid"))
}

/// Sizes and modification times of the files a table depends on, so a
/// rebuilt OSDI file or edited model invalidates the cache even when the
/// commit is the same (or unknown).
pub fn fingerprint(pdk: &Pdk, device: &Device) -> String {
    let mut files: Vec<PathBuf> = pdk.osdi.clone();
    files.push(
        pdk.models_dir(crate::dialect::Target::Ngspice)
            .join(device.library),
    );
    files
        .iter()
        .map(|f| {
            let meta = std::fs::metadata(f).ok();
            let size = meta.as_ref().map_or(0, |m| m.len());
            let mtime = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            format!(
                "{}:{size}:{mtime}",
                f.file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// The deck that characterises `device` over `grid`.
pub fn deck(pdk: &Pdk, device: &Device, grid: &Grid) -> String {
    let sign = if device.is_pmos() { -1.0 } else { 1.0 };
    let section = device.family.default_section();
    let mut d = format!(
        "* aispice gm/Id table: {} ({})\nVG g 0 0\nVD d 0 0\n",
        device.name, pdk.name
    );
    for (i, l) in grid.l.iter().enumerate() {
        d.push_str(&format!(
            "XL{i} d g 0 0 {} w={:e} l={:e} ng=1 m=1\n",
            device.name, grid.w, l
        ));
    }
    d.push_str(&format!(
        "{}\n",
        crate::pdk::lib_line(
            &pdk.models_dir(crate::dialect::Target::Ngspice)
                .join(device.library),
            &section
        )
    ));
    let inner = device.name.to_ascii_lowercase();
    let mut saves = Vec::new();
    for i in 0..grid.l.len() {
        for q in ["ids", "gm", "gds", "cgg", "vth"] {
            saves.push(format!("@n.xl{i}.n{inner}[{q}]"));
        }
    }
    for chunk in saves.chunks(10) {
        d.push_str(&format!(".save {}\n", chunk.join(" ")));
    }
    d.push_str(&format!(
        ".dc VG 0 {} {} VD {} {} {}\n.end\n",
        sign * grid.vgs_stop,
        sign * grid.vgs_step,
        sign * grid.vds_start,
        sign * grid.vds_stop,
        sign * grid.vds_step
    ));
    d
}

/// Characterise a device on ngspice. The deck is checked with the netlist
/// policy (it names only the PDK's own files) and runs with the PDK's OSDI
/// files, which are checked to be inside the PDK. Returns the table and the
/// run's warnings, such as that ngspice ran without a sandbox.
pub async fn build(
    pdk: &Pdk,
    device: &Device,
    grid: &Grid,
    ngspice: &Ngspice,
    cancel: &CancellationToken,
) -> Result<(Table, Vec<String>), GmidError> {
    if !device.is_mosfet() {
        return Err(not_mos(pdk, device.name));
    }
    let (ds, warnings) = run_deck(pdk, &deck(pdk, device, grid), ngspice, cancel).await?;
    let (vds, vgs) = (grid.vds(), grid.vgs());
    let n = vds.len() * vgs.len();
    let inner = device.name.to_ascii_lowercase();
    let column = |i: usize, q: &str| -> Result<Vec<f64>, GmidError> {
        let key = format!("@n.xl{i}.n{inner}[{q}]");
        let v = ds
            .vectors
            .iter()
            .find(|v| v.name.to_ascii_lowercase().contains(&key))
            .ok_or_else(|| GmidError::BadOutput(format!("no {key}")))?;
        let data = v.data.real();
        if data.len() != n {
            return Err(GmidError::BadOutput(format!(
                "{} points for {key}, expected {n}",
                data.len()
            )));
        }
        Ok(data.into_iter().map(f64::abs).collect())
    };
    // ngspice sweeps VGS inside VDS: point k is VDS index k / n_vgs.
    let shape =
        |flat: Vec<f64>| -> Vec<Vec<f64>> { flat.chunks(vgs.len()).map(<[f64]>::to_vec).collect() };
    let mut t = Table {
        format: FORMAT,
        pdk: pdk.name.clone(),
        commit: pdk.commit.clone(),
        fingerprint: fingerprint(pdk, device),
        device: device.name.to_string(),
        pmos: device.is_pmos(),
        section: device.family.default_section(),
        grid: grid.clone(),
        vds: vds.clone(),
        vgs: vgs.clone(),
        id: Vec::new(),
        gm: Vec::new(),
        gds: Vec::new(),
        cgg: Vec::new(),
        vth: Vec::new(),
    };
    for i in 0..grid.l.len() {
        t.id.push(shape(column(i, "ids")?));
        t.gm.push(shape(column(i, "gm")?));
        t.gds.push(shape(column(i, "gds")?));
        t.cgg.push(shape(column(i, "cgg")?));
        t.vth.push(shape(column(i, "vth")?));
    }
    Ok((t, warnings))
}

/// Run a deck aispice wrote on ngspice with the PDK's models: the deck is
/// checked with the netlist policy (it names only the PDK's own files) and
/// the OSDI files are checked to be inside the PDK. Returns its DC sweep and
/// the run's warnings.
async fn run_deck(
    pdk: &Pdk,
    text: &str,
    ngspice: &dyn Simulator,
    cancel: &CancellationToken,
) -> Result<(crate::dataset::Dataset, Vec<String>), GmidError> {
    let work = tempfile::tempdir().map_err(|e| GmidError::Sim(e.to_string()))?;
    let policy = aispice_core::netlist::Policy {
        base_dir: work.path().to_path_buf(),
        trusted_dirs: vec![pdk.root.clone()],
        ..Default::default()
    };
    let violations = aispice_core::netlist::check_policy(text, &policy);
    if let Some(v) = violations.first() {
        return Err(GmidError::Policy(format!("{} ({})", v.reason, v.line)));
    }
    let roots = std::slice::from_ref(&pdk.root);
    let osdi = pdk
        .osdi
        .iter()
        .map(|p| aispice_core::netlist::policy::check_module(p, roots, &["osdi"]))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|v| GmidError::Policy(v.reason))?;
    let mut job = SimJob::new(text, work.path().join("run"));
    job.deck_name = "gmid".into();
    job.timeout = Duration::from_secs(600);
    job.pdk = Some(PdkRun {
        osdi,
        xyce_plugins: Vec::new(),
    });
    // The deck reads only the PDK's own files.
    job.reads = vec![pdk.root.clone()];
    let mut out = ngspice
        .run(&job, cancel)
        .await
        .map_err(|e| GmidError::Sim(e.to_string()))?;
    let i = out
        .datasets
        .iter()
        .position(|d| d.kind == AnalysisKind::Dc)
        .ok_or_else(|| GmidError::BadOutput("no DC sweep".into()))?;
    Ok((out.datasets.swap_remove(i), out.warnings))
}

fn not_mos(pdk: &Pdk, name: &str) -> GmidError {
    let mos: Vec<&str> = pdk
        .kit
        .devices
        .iter()
        .filter(|d| d.is_mosfet())
        .map(|d| d.name)
        .collect();
    GmidError::NotMos(name.to_string(), mos.join(", "))
}

/// The cached table, if it was made from the same PDK files, device and
/// grid in this format.
pub fn load_cached(path: &Path, pdk: &Pdk, device: &Device, grid: &Grid) -> Option<Table> {
    let text = std::fs::read_to_string(path).ok()?;
    let t: Table = serde_json::from_str(&text).ok()?;
    (t.format == FORMAT
        && t.commit == pdk.commit
        && t.fingerprint == fingerprint(pdk, device)
        && t.device == device.name
        && t.section == device.family.default_section()
        && t.grid.same_as(grid))
    .then_some(t)
}

/// Write a table to the cache, through a temporary file so a reader never
/// sees half of it.
pub fn save(path: &Path, table: &Table) -> Result<(), GmidError> {
    let err = |e: String| GmidError::Cache {
        path: path.to_path_buf(),
        message: e,
    };
    let dir = path.parent().ok_or_else(|| err("no folder".into()))?;
    std::fs::create_dir_all(dir).map_err(|e| err(e.to_string()))?;
    let text = serde_json::to_string(table).map_err(|e| err(e.to_string()))?;
    let tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| err(e.to_string()))?;
    std::fs::write(tmp.path(), text).map_err(|e| err(e.to_string()))?;
    tmp.persist(path).map_err(|e| err(e.to_string()))?;
    Ok(())
}

/// The table for a device: from the cache when it still matches, else
/// built and cached. Returns whether it came from the cache, and the
/// warnings of the run that built it (none from the cache).
pub async fn table(
    pdk: &Pdk,
    device: &Device,
    grid: &Grid,
    cache_dir: &Path,
    refresh: bool,
    ngspice: &Ngspice,
    cancel: &CancellationToken,
) -> Result<(Table, bool, Vec<String>), GmidError> {
    let path = cache_path(cache_dir, &pdk.name, device.name);
    if !refresh && let Some(t) = load_cached(&path, pdk, device, grid) {
        return Ok((t, true, Vec::new()));
    }
    let (t, warnings) = build(pdk, device, grid, ngspice, cancel).await?;
    save(&path, &t)?;
    Ok((t, false, warnings))
}

/// Where `x` falls on an ascending axis: the two indices around it and the
/// fraction between them. `log` interpolates on a log scale.
fn bracket(axis: &[f64], x: f64, log: bool, what: &str) -> Result<(usize, usize, f64), GmidError> {
    let (first, last) = (axis[0], axis[axis.len() - 1]);
    let tol = 1e-9 * last.abs().max(1e-12);
    if !(first - tol..=last + tol).contains(&x) {
        return Err(GmidError::OutOfRange(format!(
            "{what} {} is outside the table ({} to {})",
            aispice_core::units::format(x),
            aispice_core::units::format(first),
            aispice_core::units::format(last)
        )));
    }
    if axis.len() == 1 {
        return Ok((0, 0, 0.0));
    }
    let i = axis
        .windows(2)
        .position(|w| x <= w[1] + tol)
        .unwrap_or(axis.len() - 2);
    let f = |v: f64| if log { v.ln() } else { v };
    let span = f(axis[i + 1]) - f(axis[i]);
    let t = if span == 0.0 {
        0.0
    } else {
        ((f(x.clamp(first, last)) - f(axis[i])) / span).clamp(0.0, 1.0)
    };
    Ok((i, i + 1, t))
}

/// One VGS sweep of a device: VGS and the operating point at each step.
struct Curve<'a> {
    vgs: &'a [f64],
    id: &'a [f64],
    gm: &'a [f64],
    gds: &'a [f64],
    cgg: &'a [f64],
    vth: &'a [f64],
}

/// The operating point where gm/Id equals the target.
struct Crossing {
    vgs: f64,
    vth: f64,
    id: f64,
    gds: f64,
    cgg: f64,
}

/// Why a curve has no point at the target gm/Id.
enum Miss {
    /// The target is below the curve's lowest gm/Id (at its highest VGS).
    Below { lowest: f64, vgs: f64 },
    /// The target is above the highest gm/Id the device reaches.
    Above { highest: f64 },
}

/// The point on a curve where gm/Id equals the target, on the branch where
/// gm/Id falls as VGS rises (moderate to strong inversion), found by
/// walking down from the highest VGS.
fn crossing(c: &Curve, gm_id: f64) -> Result<Crossing, Miss> {
    let ratio: Vec<f64> =
        c.id.iter()
            .zip(c.gm)
            .map(|(i, g)| if *i > 0.0 { g / i } else { 0.0 })
            .collect();
    let top = ratio.len() - 1;
    if ratio[top] > gm_id {
        return Err(Miss::Below {
            lowest: ratio[top],
            vgs: c.vgs[top],
        });
    }
    let mut k = top;
    while k > 0 && ratio[k - 1] < gm_id {
        k -= 1;
    }
    if k == 0 {
        return Err(Miss::Above {
            highest: ratio.iter().copied().fold(0.0, f64::max),
        });
    }
    // ratio[k - 1] >= target > ratio[k]
    let f = (ratio[k - 1] - gm_id) / (ratio[k - 1] - ratio[k]);
    let lerp = |a: &[f64]| a[k - 1] + f * (a[k] - a[k - 1]);
    // Current varies exponentially in weak inversion: interpolate it in log.
    let id = if c.id[k - 1] > 0.0 && c.id[k] > 0.0 {
        c.id[k - 1] * (c.id[k] / c.id[k - 1]).powf(f)
    } else {
        lerp(c.id)
    };
    Ok(Crossing {
        vgs: lerp(c.vgs),
        vth: lerp(c.vth),
        // gds tracks the current, so scale it with the interpolated current.
        gds: lerp(c.gds) / lerp(c.id) * id,
        cgg: lerp(c.cgg),
        id,
    })
}

fn miss_message(miss: Miss, gm_id: f64, where_: &str) -> GmidError {
    GmidError::OutOfRange(match miss {
        Miss::Below { lowest, vgs } => format!(
            "gm/Id = {gm_id} is below what the device reaches {where_} (lowest {lowest:.1} at VGS = {vgs} V)"
        ),
        Miss::Above { highest } => format!(
            "gm/Id = {gm_id} is above what this device reaches {where_} (at most {highest:.1})"
        ),
    })
}

/// A table curve's crossing, per metre of the table's width.
struct Point {
    vgs: f64,
    vth: f64,
    /// Current per metre of width.
    j: f64,
    gds_id: f64,
    cgg_w: f64,
}

fn on_curve(t: &Table, li: usize, vi: usize, gm_id: f64) -> Result<Point, GmidError> {
    let c = Curve {
        vgs: &t.vgs,
        id: &t.id[li][vi],
        gm: &t.gm[li][vi],
        gds: &t.gds[li][vi],
        cgg: &t.cgg[li][vi],
        vth: &t.vth[li][vi],
    };
    let x = crossing(&c, gm_id).map_err(|m| {
        miss_message(
            m,
            gm_id,
            &format!(
                "at L = {}, VDS = {} V",
                aispice_core::units::format(t.grid.l[li]),
                t.vds[vi]
            ),
        )
    })?;
    Ok(Point {
        vgs: x.vgs,
        vth: x.vth,
        j: x.id / t.grid.w,
        gds_id: x.gds / x.id,
        cgg_w: x.cgg / t.grid.w,
    })
}

/// Size a device from its table.
pub fn size(t: &Table, q: &Query) -> Result<Sizing, GmidError> {
    if !(q.gm_id > 0.0 && q.gm_id.is_finite()) {
        return Err(GmidError::OutOfRange(
            "gm/Id must be a positive number".into(),
        ));
    }
    let (l0, l1, tl) = bracket(&t.grid.l, q.l, true, "L")?;
    let (v0, v1, tv) = bracket(&t.vds, q.vds, false, "VDS")?;
    let corners = [
        ((l0, v0), (1.0 - tl) * (1.0 - tv)),
        ((l1, v0), tl * (1.0 - tv)),
        ((l0, v1), (1.0 - tl) * tv),
        ((l1, v1), tl * tv),
    ];
    let (mut vgs, mut vth, mut ln_j, mut gds_id, mut cgg_w) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for ((li, vi), weight) in corners {
        if weight == 0.0 {
            continue;
        }
        let p = on_curve(t, li, vi, q.gm_id)?;
        vgs += weight * p.vgs;
        vth += weight * p.vth;
        ln_j += weight * p.j.ln();
        gds_id += weight * p.gds_id;
        cgg_w += weight * p.cgg_w;
    }
    let j = ln_j.exp();
    let gain = q.gm_id / gds_id;
    // fT does not depend on width: gm/Cgg = (gm/Id) J / (Cgg/W).
    let ft = q.gm_id * j / (2.0 * std::f64::consts::PI * cgg_w);
    let (w, id) = match (q.id, q.w) {
        (Some(id), _) => (Some(id / j), Some(id)),
        (None, Some(w)) => (Some(w), Some(j * w)),
        (None, None) => (None, None),
    };
    Ok(Sizing {
        gm_id: q.gm_id,
        l: q.l,
        vds: q.vds,
        vgs,
        vth,
        id_per_w: j,
        gain,
        ft,
        w,
        id,
        gm: id.map(|i| q.gm_id * i),
        gds: id.map(|i| gds_id * i),
        cgg: w.map(|w| cgg_w * w),
        ng: None,
        verified: false,
    })
}

/// Fingers at most this wide: IHP's MOS models are valid to W = 10 um.
const MAX_FINGER: f64 = 10e-6;

/// Polish a table answer on the simulator. The table holds one finger of
/// the reference width, and the PDK's models do not scale exactly with width
/// (IHP's current density changes by tens of percent between 2 and 20 um
/// fingers), so the device is built as fingers of about the table's width,
/// its VGS sweep simulated, the VGS where its gm/Id is the target found, and
/// the width rescaled until it carries the current asked for. Each round is
/// one short ngspice run; two or three are usually enough. Returns the
/// sizing and the runs' warnings.
pub async fn refine(
    pdk: &Pdk,
    device: &Device,
    table: &Table,
    q: &Query,
    first: &Sizing,
    ngspice: &Ngspice,
    cancel: &CancellationToken,
) -> Result<(Sizing, Vec<String>), GmidError> {
    let wf = table.grid.w;
    let mut warnings: Vec<String> = Vec::new();
    let mut w = first.w.unwrap_or(wf);
    let mut ng = ((w / wf).round() as u32).max(1);
    let mut last = None;
    for _ in 0..8 {
        while w / f64::from(ng) > MAX_FINGER {
            ng += 1;
        }
        let (ds, ran) = run_deck(
            pdk,
            &single_deck(pdk, device, table, q, w, ng),
            ngspice,
            cancel,
        )
        .await?;
        for r in ran {
            if !warnings.contains(&r) {
                warnings.push(r);
            }
        }
        let column = |name: &str| -> Result<Vec<f64>, GmidError> {
            let key = format!("[{name}]");
            ds.vectors
                .iter()
                .find(|v| {
                    v.name.to_ascii_lowercase().ends_with(&key)
                        || v.name.to_ascii_lowercase().ends_with(&format!("{key})"))
                })
                .map(|v| v.data.real().into_iter().map(f64::abs).collect())
                .ok_or_else(|| GmidError::BadOutput(format!("no {name} in the check run")))
        };
        let vgs: Vec<f64> = ds
            .axis_vector()
            .ok_or_else(|| GmidError::BadOutput("no VGS axis".into()))?
            .data
            .real()
            .into_iter()
            .map(f64::abs)
            .collect();
        let (id, gm, gds, cgg, vth) = (
            column("ids")?,
            column("gm")?,
            column("gds")?,
            column("cgg")?,
            column("vth")?,
        );
        let c = Curve {
            vgs: &vgs,
            id: &id,
            gm: &gm,
            gds: &gds,
            cgg: &cgg,
            vth: &vth,
        };
        let x = crossing(&c, q.gm_id).map_err(|m| {
            miss_message(
                m,
                q.gm_id,
                &format!(
                    "as built (W = {}, ng = {ng})",
                    aispice_core::units::format(w)
                ),
            )
        })?;
        let rescale = match q.id {
            Some(target) => target / x.id,
            None => 1.0,
        };
        last = Some((w, ng, x));
        if (rescale - 1.0).abs() < 1e-3 {
            break;
        }
        w *= rescale;
    }
    let (w, ng, x) = last.expect("at least one round");
    let gm = q.gm_id * x.id;
    let sizing = Sizing {
        gm_id: q.gm_id,
        l: q.l,
        vds: q.vds,
        vgs: x.vgs,
        vth: x.vth,
        id_per_w: x.id / w,
        gain: gm / x.gds,
        ft: gm / (2.0 * std::f64::consts::PI * x.cgg),
        w: Some(w),
        id: Some(x.id),
        gm: Some(gm),
        gds: Some(x.gds),
        cgg: Some(x.cgg),
        ng: Some(ng),
        verified: true,
    };
    Ok((sizing, warnings))
}

/// A deck that sweeps VGS of one device built as `ng` fingers.
fn single_deck(pdk: &Pdk, device: &Device, t: &Table, q: &Query, w: f64, ng: u32) -> String {
    let sign = if device.is_pmos() { -1.0 } else { 1.0 };
    let inner = device.name.to_ascii_lowercase();
    let saves: Vec<String> = ["ids", "gm", "gds", "cgg", "vth"]
        .iter()
        .map(|q| format!("@n.xm1.n{inner}[{q}]"))
        .collect();
    format!(
        "* aispice gm/Id check: {name}\nVG g 0 0\nVD d 0 {vd}\nXM1 d g 0 0 {name} w={w:e} l={l:e} ng={ng} m=1\n{lib}\n.save {saves}\n.dc VG 0 {stop} {step}\n.end\n",
        name = device.name,
        vd = sign * q.vds,
        l = q.l,
        lib = crate::pdk::lib_line(
            &pdk.models_dir(crate::dialect::Target::Ngspice)
                .join(device.library),
            &device.family.default_section()
        ),
        saves = saves.join(" "),
        stop = sign * t.grid.vgs_stop,
        step = sign * t.grid.vgs_step / 5.0,
    )
}

/// The answer in words, for people and models.
pub fn describe(t: &Table, s: &Sizing) -> String {
    use aispice_core::units::format_with_unit as u;
    let (gs, ds) = if t.pmos { ("SG", "SD") } else { ("GS", "DS") };
    let mut text = format!(
        "{} at L = {}, V{ds} = {}, gm/Id = {} S/A:\n",
        t.device,
        u(s.l, "m"),
        u(s.vds, "V"),
        s.gm_id
    );
    if let (Some(w), Some(id)) = (s.w, s.id) {
        text.push_str(&format!("  W = {}, Id = {}\n", u(w, "m"), u(id, "A")));
        if let Some(ng) = s.ng {
            text.push_str(&format!(
                "  SpiceLine: w={} l={} ng={ng} m=1\n",
                aispice_core::units::format(w),
                aispice_core::units::format(s.l)
            ));
        }
    }
    text.push_str(&format!(
        "  V{gs} = {} (Vth {}), current density {} per um of width\n",
        u(s.vgs, "V"),
        u(s.vth, "V"),
        u(s.id_per_w * 1e-6, "A")
    ));
    if let (Some(gm), Some(gds), Some(cgg)) = (s.gm, s.gds, s.cgg) {
        text.push_str(&format!(
            "  gm = {}, gds = {}, Cgg = {}\n",
            u(gm, "S"),
            u(gds, "S"),
            u(cgg, "F")
        ));
    }
    text.push_str(&format!(
        "  intrinsic gain gm/gds = {:.1} ({:.1} dB), fT = {}\n",
        s.gain,
        20.0 * s.gain.log10(),
        u(s.ft, "Hz")
    ));
    if s.verified {
        text.push_str(
            "Checked on ngspice: the device as given (these fingers) simulated at this VGS and VDS has this gm/Id and current.\n",
        );
    }
    text.push_str(&format!(
        "Started from a {} table at W = {} (one finger), section {}, PDK {}{}.\n",
        t.device,
        u(t.grid.w, "m"),
        t.section,
        t.pdk,
        t.commit
            .as_deref()
            .map(|c| format!(" at {}", &c[..c.len().min(12)]))
            .unwrap_or_default()
    ));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table with gm/Id = 25 - 20 VGS on every curve, current density
    /// proportional to 1/L and to (1 + VDS), and constant gds/Id and Cgg/W.
    fn synthetic() -> Table {
        let grid = Grid {
            w: 1e-6,
            l: vec![0.5e-6, 1e-6, 2e-6],
            vds_start: 0.2,
            vds_stop: 1.0,
            vds_step: 0.4,
            vgs_stop: 1.2,
            vgs_step: 0.05,
        };
        let (vds, vgs) = (grid.vds(), grid.vgs());
        let mut t = Table {
            format: FORMAT,
            pdk: "test".into(),
            commit: None,
            fingerprint: String::new(),
            device: "nfet".into(),
            pmos: false,
            section: "mos_tt".into(),
            grid: grid.clone(),
            vds: vds.clone(),
            vgs: vgs.clone(),
            id: vec![],
            gm: vec![],
            gds: vec![],
            cgg: vec![],
            vth: vec![],
        };
        for l in &grid.l {
            let (mut id, mut gm, mut gds, mut cgg, mut vth) =
                (vec![], vec![], vec![], vec![], vec![]);
            for v in &vds {
                let scale = 1e-6 / l * (1.0 + v);
                // Id = scale * exp(25 VGS - 10 VGS^2) gives gm/Id = 25 - 20 VGS.
                let i: Vec<f64> = vgs
                    .iter()
                    .map(|g| scale * (25.0 * g - 10.0 * g * g).exp())
                    .collect();
                gm.push(
                    vgs.iter()
                        .zip(&i)
                        .map(|(g, i)| (25.0 - 20.0 * g) * i)
                        .collect(),
                );
                gds.push(i.iter().map(|i| 0.02 * i).collect());
                cgg.push(vec![2e-15; vgs.len()]);
                vth.push(vec![0.4; vgs.len()]);
                id.push(i);
            }
            t.id.push(id);
            t.gm.push(gm);
            t.gds.push(gds);
            t.cgg.push(cgg);
            t.vth.push(vth);
        }
        t
    }

    fn close(a: f64, b: f64, rel: f64) -> bool {
        (a - b).abs() <= rel * b.abs()
    }

    #[test]
    fn crossing_and_width_on_a_grid_point() {
        let t = synthetic();
        let s = size(
            &t,
            &Query {
                gm_id: 12.0,
                l: 1e-6,
                vds: 0.6,
                id: Some(50e-6),
                w: None,
            },
        )
        .unwrap();
        // gm/Id = 12 at VGS = 0.65.
        assert!(close(s.vgs, 0.65, 2e-3), "{s:?}");
        // Density at L = 1u, VDS = 0.6: 1.6 * exp(25*0.65 - 10*0.65^2) per um.
        let j = 1.6 * (25.0f64 * 0.65 - 10.0 * 0.65 * 0.65).exp() / 1e-6;
        assert!(close(s.id_per_w, j, 0.02), "{} vs {j}", s.id_per_w);
        assert!(close(s.w.unwrap(), 50e-6 / s.id_per_w, 1e-12));
        assert!(close(s.gm.unwrap(), 600e-6, 1e-12));
        assert!(close(s.gain, 12.0 / 0.02, 1e-9), "{}", s.gain);
        assert!(close(s.cgg.unwrap(), 2e-9 * s.w.unwrap(), 1e-9));
        assert!(close(
            s.ft,
            600e-6 / (2.0 * std::f64::consts::PI * s.cgg.unwrap()),
            1e-9
        ));
    }

    #[test]
    fn lengths_interpolate_on_a_log_scale() {
        let t = synthetic();
        let at = |l: f64| {
            size(
                &t,
                &Query {
                    gm_id: 12.0,
                    l,
                    vds: 1.0,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        // Density goes as 1/L, which log-log interpolation follows exactly.
        let ratio = at(0.5e-6).id_per_w / at(0.8e-6).id_per_w;
        assert!(close(ratio, 0.8 / 0.5, 1e-9), "{ratio}");
        // A given width answers with a current.
        let s = size(
            &t,
            &Query {
                gm_id: 12.0,
                l: 2e-6,
                vds: 0.2,
                id: None,
                w: Some(10e-6),
            },
        )
        .unwrap();
        assert!(close(s.id.unwrap(), s.id_per_w * 10e-6, 1e-12));
    }

    #[test]
    fn out_of_range_requests_say_what_the_table_covers() {
        let t = synthetic();
        let q = |gm_id, l, vds| Query {
            gm_id,
            l,
            vds,
            ..Default::default()
        };
        let msg = |r: Result<Sizing, GmidError>| r.unwrap_err().to_string();
        assert!(
            msg(size(&t, &q(12.0, 0.1e-6, 0.6)))
                .contains("L 100n is outside the table (500n to 2u)")
        );
        assert!(msg(size(&t, &q(12.0, 1e-6, 1.5))).contains("VDS"));
        assert!(msg(size(&t, &q(30.0, 1e-6, 0.6))).contains("above what this device reaches"));
        assert!(msg(size(&t, &q(0.5, 1e-6, 0.6))).contains("below what the device reaches"));
        assert!(msg(size(&t, &q(-1.0, 1e-6, 0.6))).contains("positive"));
    }

    /// A run's warnings, such as that ngspice ran without a sandbox, reach
    /// the caller with its sweep instead of being dropped.
    #[tokio::test]
    async fn a_runs_warnings_come_back_with_its_sweep() {
        struct Fake;
        #[async_trait::async_trait]
        impl Simulator for Fake {
            fn id(&self) -> crate::backend::SimId {
                crate::backend::SimId::Ngspice
            }
            async fn detect(&self) -> crate::backend::Detection {
                crate::backend::Detection::default()
            }
            async fn run(
                &self,
                _: &SimJob,
                _: &CancellationToken,
            ) -> Result<crate::backend::SimOutput, crate::backend::SimError> {
                Ok(crate::backend::SimOutput {
                    datasets: vec![crate::dataset::Dataset {
                        title: "gmid".into(),
                        plotname: "DC transfer characteristic".into(),
                        kind: AnalysisKind::Dc,
                        axis: None,
                        vectors: Vec::new(),
                        steps: Vec::new(),
                    }],
                    warnings: vec![
                        "ngspice ran without a sandbox (this system is not Linux)".into(),
                    ],
                    ..Default::default()
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let root = crate::pdk::fake_ihp(dir.path());
        let pdk = Pdk::open("ihp", &root).unwrap();
        let (ds, warnings) = run_deck(&pdk, "* gmid\n.end\n", &Fake, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(ds.kind, AnalysisKind::Dc);
        assert_eq!(
            warnings,
            ["ngspice ran without a sandbox (this system is not Linux)"]
        );
    }

    #[test]
    fn caches_match_only_the_same_pdk_files_and_grid() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::pdk::fake_ihp(dir.path());
        let pdk = Pdk::open("ihp", &root).unwrap();
        let dev = pdk.kit.device("sg13_lv_nmos").unwrap();
        let grid = Grid::default_for(pdk.kit, dev);
        let mut t = synthetic();
        t.device = dev.name.into();
        t.grid = grid.clone();
        t.pdk = pdk.name.clone();
        t.fingerprint = fingerprint(&pdk, dev);
        let path = cache_path(&dir.path().join("cache"), &pdk.name, dev.name);
        assert!(path.ends_with("ihp/sg13_lv_nmos.json"));
        save(&path, &t).unwrap();
        let back = load_cached(&path, &pdk, dev, &grid).expect("cache hit");
        assert_eq!(back.id.len(), t.id.len());
        assert!(close(back.id[1][2][10], t.id[1][2][10], 1e-12));
        let mut other = grid.clone();
        other.w = 1e-6;
        assert!(load_cached(&path, &pdk, dev, &other).is_none());
        // A rebuilt OSDI file changes the fingerprint.
        std::fs::write(&pdk.osdi[0], b"rebuilt").unwrap();
        assert!(load_cached(&path, &pdk, dev, &grid).is_none());
    }

    #[test]
    fn decks_save_each_instance_and_sweep_pmos_negative() {
        let dir = tempfile::tempdir().unwrap();
        let pdk = Pdk::open("ihp", &crate::pdk::fake_ihp(dir.path())).unwrap();
        let p = pdk.kit.device("sg13_lv_pmos").unwrap();
        let grid = Grid::default_for(pdk.kit, p);
        let d = deck(&pdk, p, &grid);
        assert!(
            d.contains("XL11 d g 0 0 sg13_lv_pmos w=5e-6 l=1e-5 ng=1 m=1"),
            "{d}"
        );
        assert!(
            d.contains("XL0 d g 0 0 sg13_lv_pmos w=5e-6 l=1.3e-7 ng=1 m=1"),
            "{d}"
        );
        assert!(d.contains("@n.xl0.nsg13_lv_pmos[cgg]"), "{d}");
        assert!(d.contains(".dc VG 0 -1.2 -0.01 VD -0.1 -1.2 -0.1"), "{d}");
        assert!(d.contains("cornerMOSlv.lib mos_tt"), "{d}");
        assert_eq!((grid.vgs().len(), grid.vds().len()), (121, 12));
        let hv = Grid::default_for(pdk.kit, pdk.kit.device("sg13_hv_nmos").unwrap());
        assert_eq!((hv.vgs().len(), hv.vds().len()), (133, 16));
        assert!((hv.l[0] - 0.45e-6).abs() < 1e-15);
        // The policy refuses every absolute Windows path.
        if cfg!(not(windows)) {
            let policy = aispice_core::netlist::Policy {
                base_dir: dir.path().to_path_buf(),
                trusted_dirs: vec![pdk.root.clone()],
                ..Default::default()
            };
            assert_eq!(aispice_core::netlist::check_policy(&d, &policy), vec![]);
        }
    }
}

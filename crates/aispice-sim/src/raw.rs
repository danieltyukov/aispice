//! Reading `.raw` waveform files from LTspice, ngspice, Xyce and Spectre.
//!
//! Every simulator writes a variant of the Berkeley SPICE raw format: a text
//! header (title, plot name, flags, variable list) followed by the data, in
//! binary after `Binary:` or as text after `Values:`. The variants differ in
//! ways that silently corrupt numbers when ignored, so each one is handled
//! explicitly:
//!
//! - LTspice writes the header in UTF-16LE. Its real data stores column 0 as
//!   float64 and every other column as float32, unless the `double` flag is
//!   set. Complex data is complex128 throughout, frequency included.
//!   `fastaccess` files are column-major. Waveform compression can store
//!   transient time with a negative sign, and `.tran` with a start time stores
//!   time relative to the `Offset` header.
//! - `.step` runs (`stepped` flag) are concatenated in one plot; each run
//!   starts where the sweep axis returns to its first value.
//! - ngspice and Xyce write UTF-8 headers and all-float64 data, and put
//!   several plots one after another in one file. Xyce repeats only from
//!   `Plotname:` on, and writes one plot per `.step` value, which are merged
//!   back into one stepped dataset here.
//! - Xyce reports `.op` as a one-point `DC transfer characteristic` with a
//!   dummy `sweep` variable, and names node voltages bare (`OUT`).
//! - ngspice leaves the imaginary part of the AC frequency uninitialised, so
//!   only the real part of the axis is used.
//! - ngspice's `.pz` writes one complex point per root (`v(pole(1))`,
//!   `v(zero(1))`, in rad/s), and a plot with no variables at all when the
//!   transfer function has no finite poles or zeros.
//! - Noise plots name their spectra differently in each simulator; they are
//!   renamed to [`crate::noise::ONOISE`] and [`crate::noise::INOISE`].

use crate::dataset::{
    AnalysisKind, Complex, Dataset, Quantity, Step, Vector, VectorData, normalize_name,
};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RawError {
    #[error("not a SPICE raw file: {0}")]
    NotRaw(String),
    #[error("malformed raw header: {0}")]
    Header(String),
    #[error("malformed raw data: {0}")]
    Data(String),
    #[error("cannot read raw file: {0}")]
    Io(String),
}

/// Read every plot in a raw file. Files with several plots (ngspice runs with
/// more than one analysis) give one [`Dataset`] each; Xyce's per-step plots
/// are merged into one stepped dataset.
pub fn read_raw(bytes: &[u8]) -> Result<Vec<Dataset>, RawError> {
    let mut reader = Reader::new(bytes);
    let mut plots = Vec::new();
    loop {
        reader.skip_blank();
        if reader.at_end() {
            break;
        }
        match parse_plot(&mut reader) {
            Ok(Some(plot)) => plots.push(plot),
            Ok(None) => break,
            // Trailing bytes after a complete plot are padding, not a plot.
            Err(RawError::NotRaw(_)) if !plots.is_empty() => break,
            Err(e) => return Err(e),
        }
    }
    if plots.is_empty() {
        return Err(RawError::NotRaw(
            "no `Variables:` header with `Binary:` or `Values:` data".into(),
        ));
    }
    let datasets = plots
        .into_iter()
        .map(to_dataset)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(merge_xyce_steps(datasets))
}

/// Read a raw file from disk. Spectre's `-raw` target can be a directory of
/// raw files, one per analysis; every file in it is read in name order.
pub fn read_raw_file(path: &Path) -> Result<Vec<Dataset>, RawError> {
    if path.is_dir() {
        let mut files: Vec<_> = std::fs::read_dir(path)
            .map_err(|e| RawError::Io(format!("{}: {e}", path.display())))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        let mut out = Vec::new();
        for f in files {
            // A raw directory also holds logs and other files; skip what is
            // not a raw file rather than failing the whole read.
            if let Ok(mut ds) = read_raw_file(&f) {
                out.append(&mut ds);
            }
        }
        if out.is_empty() {
            return Err(RawError::NotRaw(format!(
                "{} holds no raw files",
                path.display()
            )));
        }
        return Ok(out);
    }
    let bytes =
        std::fs::read(path).map_err(|e| RawError::Io(format!("{}: {e}", path.display())))?;
    read_raw(&bytes)
}

/// Replace the generic `step 1`, `step 2` labels of a stepped dataset with
/// real ones, such as the `r=1000` lines LTspice writes to its log. Extra or
/// missing labels leave the dataset's own labels in place.
pub fn apply_step_labels(ds: &mut Dataset, labels: &[String]) {
    if labels.len() != ds.steps.len() || !ds.is_stepped() {
        return;
    }
    for (step, label) in ds.steps.iter_mut().zip(labels) {
        step.label = label.clone();
    }
}

#[derive(Debug, Clone, PartialEq)]
struct RawVar {
    name: String,
    kind: String,
}

#[derive(Debug, Clone)]
struct RawPlot {
    title: String,
    plotname: String,
    flags: Vec<String>,
    offset: f64,
    ltspice: bool,
    vars: Vec<RawVar>,
    /// Column-major data, one entry per variable.
    columns: Columns,
}

#[derive(Debug, Clone)]
enum Columns {
    Real(Vec<Vec<f64>>),
    Complex(Vec<Vec<Complex>>),
}

impl RawPlot {
    fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f.eq_ignore_ascii_case(flag))
    }
}

/// A cursor over the file that reads header lines in either UTF-16LE or an
/// 8-bit encoding, and raw bytes for binary data.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    wide: bool,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        let mut r = Self {
            bytes,
            pos: 0,
            wide: false,
        };
        if bytes.starts_with(&[0xFF, 0xFE]) {
            r.pos = 2;
            r.wide = true;
        } else if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            r.pos = 3;
        } else {
            r.wide = looks_wide(bytes);
        }
        r
    }

    fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.pos.min(self.bytes.len())..]
    }

    /// Skip line breaks, spaces and NUL padding between plots.
    fn skip_blank(&mut self) {
        let step = if self.wide { 2 } else { 1 };
        while self.pos + step <= self.bytes.len() {
            let c = self.bytes[self.pos];
            let hi = if self.wide {
                self.bytes[self.pos + 1]
            } else {
                0
            };
            if hi == 0 && matches!(c, b'\n' | b'\r' | b' ' | b'\t' | 0) {
                self.pos += step;
            } else {
                break;
            }
        }
        if self.pos > self.bytes.len() {
            self.pos = self.bytes.len();
        }
    }

    /// The next line without its terminator, or `None` at the end.
    fn line(&mut self) -> Option<String> {
        if self.at_end() {
            return None;
        }
        if self.wide {
            let mut units = Vec::new();
            while self.pos + 1 < self.bytes.len() {
                let u = u16::from_le_bytes([self.bytes[self.pos], self.bytes[self.pos + 1]]);
                self.pos += 2;
                if u == u16::from(b'\n') {
                    break;
                }
                units.push(u);
            }
            if self.pos + 1 == self.bytes.len() {
                self.pos += 1;
            }
            let s = String::from_utf16_lossy(&units);
            Some(s.trim_end_matches('\r').to_string())
        } else {
            let rest = self.remaining();
            let end = rest.iter().position(|&b| b == b'\n');
            let raw = &rest[..end.unwrap_or(rest.len())];
            self.pos += raw.len() + usize::from(end.is_some());
            let s = match std::str::from_utf8(raw) {
                Ok(s) => s.to_string(),
                Err(_) => raw.iter().map(|&b| b as char).collect(),
            };
            Some(s.trim_end_matches('\r').to_string())
        }
    }

    /// Peek at the next line without consuming it.
    fn peek_line(&mut self) -> Option<String> {
        let save = self.pos;
        let line = self.line();
        self.pos = save;
        line
    }
}

/// UTF-16LE text without a byte order mark: ASCII characters each followed by
/// a NUL. LTspice's raw headers always start with `Title:` or a similar key.
fn looks_wide(bytes: &[u8]) -> bool {
    let probe = &bytes[..bytes.len().min(32) & !1];
    probe.len() >= 8
        && probe
            .as_chunks::<2>()
            .0
            .iter()
            .all(|&[lo, hi]| hi == 0 && lo != 0)
}

/// Header keys that can start or continue a plot. A line starting with one of
/// these also ends the ASCII data of the previous plot.
const HEADER_KEYS: &[&str] = &[
    "title",
    "date",
    "plotname",
    "flags",
    "no. variables",
    "no. points",
    "offset",
    "command",
    "variables",
    "values",
    "binary",
    "output",
    "backannotation",
    "dimensions",
];

fn split_key(line: &str) -> Option<(String, &str)> {
    let (key, value) = line.split_once(':')?;
    let key = key.trim().to_ascii_lowercase();
    if key.is_empty() || key.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some((key, value.trim()))
}

fn is_header_line(line: &str) -> bool {
    split_key(line).is_some_and(|(k, _)| HEADER_KEYS.contains(&k.as_str()))
}

fn parse_plot(r: &mut Reader) -> Result<Option<RawPlot>, RawError> {
    let mut title = String::new();
    let mut plotname = String::new();
    let mut flags = Vec::new();
    let mut nvars: Option<usize> = None;
    let mut npoints: Option<usize> = None;
    let mut offset = 0.0;
    let mut command = String::new();
    let mut vars: Vec<RawVar> = Vec::new();
    let mut saw_any = false;
    loop {
        let Some(line) = r.line() else {
            return if saw_any {
                Err(RawError::Header(
                    "header ends before `Binary:` or `Values:`".into(),
                ))
            } else {
                Ok(None)
            };
        };
        if line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = split_key(&line) else {
            if saw_any {
                return Err(RawError::Header(format!("unexpected line `{line}`")));
            }
            return Err(RawError::NotRaw(format!(
                "starts with `{}`",
                line.chars().take(40).collect::<String>()
            )));
        };
        saw_any = true;
        match key.as_str() {
            "title" => title = value.to_string(),
            "plotname" => plotname = value.to_string(),
            "flags" => flags = value.split_whitespace().map(str::to_string).collect(),
            "no. variables" => nvars = Some(parse_count(value, "No. Variables")?),
            "no. points" => npoints = Some(parse_count(value, "No. Points")?),
            "offset" => offset = parse_f64(value).unwrap_or(0.0),
            "command" => command = value.to_string(),
            "variables" => {
                let n = nvars.ok_or_else(|| {
                    RawError::Header("`Variables:` before `No. Variables:`".into())
                })?;
                if !value.is_empty() {
                    vars.push(parse_var(value)?);
                }
                while vars.len() < n {
                    let l = r.line().ok_or_else(|| {
                        RawError::Header(format!("expected {n} variables, found {}", vars.len()))
                    })?;
                    if l.trim().is_empty() {
                        continue;
                    }
                    vars.push(parse_var(&l)?);
                }
            }
            "binary" | "values" => {
                let n = nvars.ok_or_else(|| RawError::Header("no `No. Variables:`".into()))?;
                let points = npoints.ok_or_else(|| RawError::Header("no `No. Points:`".into()))?;
                if vars.len() != n {
                    return Err(RawError::Header(format!(
                        "header lists {} variables, `No. Variables:` says {n}",
                        vars.len()
                    )));
                }
                if n == 0 && analysis_kind(&plotname) != AnalysisKind::PoleZero {
                    return Err(RawError::Header("plot has no variables".into()));
                }
                let complex = flags.iter().any(|f| f.eq_ignore_ascii_case("complex"));
                let ltspice = r.wide || command.to_ascii_lowercase().contains("ltspice");
                let mut plot = RawPlot {
                    title: title.clone(),
                    plotname: plotname.clone(),
                    flags: flags.clone(),
                    offset,
                    ltspice,
                    vars: vars.clone(),
                    columns: Columns::Real(Vec::new()),
                };
                plot.columns = if key == "binary" {
                    read_binary(r, &plot, points, complex)?
                } else {
                    read_ascii(r, n, points, complex)?
                };
                return Ok(Some(plot));
            }
            // Output:, Backannotation:, Date:, Dimensions: and anything a
            // newer simulator adds carry nothing the reader needs.
            _ => {}
        }
    }
}

fn parse_count(value: &str, what: &str) -> Result<usize, RawError> {
    value
        .split_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| RawError::Header(format!("bad {what} `{value}`")))
}

fn parse_var(line: &str) -> Result<RawVar, RawError> {
    let mut words = line.split_whitespace();
    let first = words
        .next()
        .ok_or_else(|| RawError::Header("empty variable line".into()))?;
    // The index is optional in some writers; when the first word is not a
    // number it is the name.
    let name = if first.parse::<usize>().is_ok() {
        words.next()
    } else {
        Some(first)
    }
    .ok_or_else(|| RawError::Header(format!("bad variable line `{line}`")))?;
    let kind = words.next().unwrap_or("notype").to_string();
    Ok(RawVar {
        name: name.to_string(),
        kind,
    })
}

fn parse_f64(s: &str) -> Option<f64> {
    let t = s.trim();
    t.parse::<f64>().ok().or_else(|| {
        // Some writers use a Fortran-style `D` exponent.
        t.replace(['d', 'D'], "e").parse().ok()
    })
}

fn read_binary(
    r: &mut Reader,
    plot: &RawPlot,
    points: usize,
    complex: bool,
) -> Result<Columns, RawError> {
    let n = plot.vars.len();
    if n == 0 {
        // An empty pole-zero plot has no data; the next plot starts here.
        return Ok(Columns::Real(Vec::new()));
    }
    let data = r.remaining();
    let double = plot.has_flag("double");
    let fast = plot.has_flag("fastaccess");
    // Bytes per value of each column.
    let mut widths: Vec<usize> = if complex {
        vec![16; n]
    } else if double || !plot.ltspice {
        vec![8; n]
    } else {
        let mut w = vec![4; n];
        w[0] = 8;
        w
    };
    let mut row: usize = widths.iter().sum();
    // An LTspice-looking file whose size only fits all-double data is read as
    // such rather than as garbage.
    if !complex && plot.ltspice && !double {
        let all_double = 8 * n;
        if data.len() != row * points && data.len() == all_double * points {
            widths = vec![8; n];
            row = all_double;
        }
    }
    let available = data.len().checked_div(row).unwrap_or(0);
    let count = points.min(available);
    if fast && count < points {
        return Err(RawError::Data(format!(
            "fastaccess data holds {} bytes, needs {}",
            data.len(),
            row * points
        )));
    }
    let read_value = |at: usize, width: usize| -> (f64, f64) {
        let b = &data[at..at + width];
        match width {
            4 => (f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64, 0.0),
            8 => (f64::from_le_bytes(b[..8].try_into().expect("8 bytes")), 0.0),
            _ => (
                f64::from_le_bytes(b[..8].try_into().expect("8 bytes")),
                f64::from_le_bytes(b[8..16].try_into().expect("8 bytes")),
            ),
        }
    };
    let mut re: Vec<Vec<f64>> = vec![Vec::with_capacity(count); n];
    let mut im: Vec<Vec<f64>> = if complex {
        vec![Vec::with_capacity(count); n]
    } else {
        Vec::new()
    };
    if fast {
        let mut at = 0;
        for (v, &w) in widths.iter().enumerate() {
            for _ in 0..count {
                let (a, b) = read_value(at, w);
                re[v].push(a);
                if complex {
                    im[v].push(b);
                }
                at += w;
            }
        }
    } else {
        let mut at = 0;
        for _ in 0..count {
            for (v, &w) in widths.iter().enumerate() {
                let (a, b) = read_value(at, w);
                re[v].push(a);
                if complex {
                    im[v].push(b);
                }
                at += w;
            }
        }
    }
    // A short file (a run cut off mid-write) has nothing usable after the
    // last complete point.
    r.pos = if count < points {
        r.bytes.len()
    } else {
        r.pos + count * row
    };
    Ok(if complex {
        Columns::Complex(
            re.into_iter()
                .zip(im)
                .map(|(r, i)| {
                    r.into_iter()
                        .zip(i)
                        .map(|(a, b)| Complex::new(a, b))
                        .collect()
                })
                .collect(),
        )
    } else {
        Columns::Real(re)
    })
}

fn read_ascii(
    r: &mut Reader,
    nvars: usize,
    points: usize,
    complex: bool,
) -> Result<Columns, RawError> {
    // Gather the value tokens up to the next header or the end of the file.
    let mut tokens: Vec<String> = Vec::new();
    while let Some(line) = r.peek_line() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("values:") {
            // LTspice's .log.raw repeats the `Values:` line.
            r.line();
            continue;
        }
        if is_header_line(trimmed) {
            break;
        }
        r.line();
        for t in trimmed.split_whitespace() {
            // Xyce writes complex values as `re, im` with a space.
            match tokens.last_mut() {
                Some(last) if last.ends_with(',') => last.push_str(t),
                _ => tokens.push(t.to_string()),
            }
        }
    }
    let per_point = nvars + 1;
    let count = points.min(tokens.len() / per_point);
    let mut re: Vec<Vec<f64>> = vec![Vec::with_capacity(count); nvars];
    let mut im: Vec<Vec<f64>> = vec![Vec::with_capacity(count); nvars];
    for p in 0..count {
        let base = p * per_point;
        let idx = &tokens[base];
        if idx.parse::<usize>().is_err() {
            return Err(RawError::Data(format!(
                "point {p}: expected an index, found `{idx}`"
            )));
        }
        for v in 0..nvars {
            let tok = &tokens[base + 1 + v];
            let (a, b) = match tok.split_once(',') {
                Some((a, b)) => (parse_f64(a), parse_f64(b)),
                None => (parse_f64(tok), Some(0.0)),
            };
            match (a, b) {
                (Some(a), Some(b)) => {
                    re[v].push(a);
                    im[v].push(b);
                }
                _ => {
                    return Err(RawError::Data(format!(
                        "point {p}, variable {v}: bad number `{tok}`"
                    )));
                }
            }
        }
    }
    Ok(if complex {
        Columns::Complex(
            re.into_iter()
                .zip(im)
                .map(|(r, i)| {
                    r.into_iter()
                        .zip(i)
                        .map(|(a, b)| Complex::new(a, b))
                        .collect()
                })
                .collect(),
        )
    } else {
        Columns::Real(re)
    })
}

/// The analysis behind a plot name. Xyce nests names (`Step Analysis: ...
/// DC Sweep: ... DC transfer characteristic`), so this looks for keywords
/// rather than matching the whole name.
pub fn analysis_kind(plotname: &str) -> AnalysisKind {
    let p = plotname.to_ascii_lowercase();
    if p.contains("transient") {
        AnalysisKind::Transient
    } else if p.contains("noise") {
        AnalysisKind::Noise
    } else if p.contains("ac analysis") || p.starts_with("ac ") || p == "ac" {
        AnalysisKind::Ac
    } else if p.contains("transfer function") {
        AnalysisKind::TransferFunction
    } else if p.contains("pole-zero") {
        AnalysisKind::PoleZero
    } else if p.contains("operating point") {
        AnalysisKind::Op
    } else if p.contains("dc transfer") || p.contains("dc sweep") || p.starts_with("dc ") {
        AnalysisKind::Dc
    } else {
        AnalysisKind::Other
    }
}

fn quantity_of(kind: &str) -> Quantity {
    match kind.to_ascii_lowercase().as_str() {
        "time" | "s" => Quantity::Time,
        "frequency" | "hz" => Quantity::Frequency,
        "voltage" | "v" => Quantity::Voltage,
        "current" | "device_current" | "subckt_current" | "a" => Quantity::Current,
        "param" | "step" | "sweep" => Quantity::Sweep,
        _ => Quantity::Other,
    }
}

/// `name = X value = Y` pairs right after `Step Analysis: Step i of n
/// params:` in a Xyce plot name.
fn xyce_step_params(plotname: &str) -> Option<Vec<(String, String)>> {
    let rest = plotname.trim().strip_prefix("Step Analysis:")?;
    let (_, after) = rest.split_once("params:")?;
    Some(name_value_pairs(after))
}

fn name_value_pairs(text: &str) -> Vec<(String, String)> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 5 < words.len()
        && words[i] == "name"
        && words[i + 1] == "="
        && words[i + 3] == "value"
        && words[i + 4] == "="
    {
        out.push((words[i + 2].to_string(), words[i + 5].to_string()));
        i += 6;
    }
    out
}

/// The swept source of a Xyce DC sweep, from `DC Sweep: Step i of n params:
/// name = V1 value = 0`.
fn xyce_dc_source(plotname: &str) -> Option<String> {
    let (_, after) = plotname.split_once("DC Sweep:")?;
    let (_, params) = after.split_once("params:")?;
    name_value_pairs(params).into_iter().next().map(|(n, _)| n)
}

fn to_dataset(plot: RawPlot) -> Result<Dataset, RawError> {
    let mut kind = analysis_kind(&plot.plotname);
    if plot.vars.is_empty() {
        // A pole-zero analysis that found nothing (parse_plot refuses empty
        // plots of any other kind).
        return Ok(Dataset {
            title: plot.title,
            plotname: plot.plotname,
            kind,
            axis: None,
            vectors: Vec::new(),
            steps: vec![Step {
                range: 0..0,
                label: String::new(),
            }],
        });
    }
    let npoints = match &plot.columns {
        Columns::Real(c) => c.first().map_or(0, Vec::len),
        Columns::Complex(c) => c.first().map_or(0, Vec::len),
    };
    let mut skip_first = false;
    // Xyce's .op: a one-point DC "sweep" of nothing.
    if kind == AnalysisKind::Dc
        && npoints == 1
        && plot.vars[0].name.eq_ignore_ascii_case("sweep")
        && !plot.plotname.contains("DC Sweep:")
    {
        kind = AnalysisKind::Op;
        skip_first = true;
    }
    let first_q = quantity_of(&plot.vars[0].kind);
    let axis = match kind {
        AnalysisKind::Op | AnalysisKind::TransferFunction | AnalysisKind::PoleZero => None,
        AnalysisKind::Dc => Some(0),
        _ if matches!(
            first_q,
            Quantity::Time | Quantity::Frequency | Quantity::Sweep
        ) =>
        {
            Some(0)
        }
        _ => None,
    };

    let mut vectors = Vec::with_capacity(plot.vars.len());
    for (i, var) in plot.vars.iter().enumerate() {
        if skip_first && i == 0 {
            continue;
        }
        let is_axis = axis == Some(i);
        let raw_q = quantity_of(&var.kind);
        let mut quantity = if is_axis && kind == AnalysisKind::Dc {
            Quantity::Sweep
        } else {
            raw_q
        };
        let mut name = if is_axis {
            axis_name(&plot, var, kind)
        } else {
            vector_name(&var.name, raw_q)
        };
        if kind == AnalysisKind::Noise
            && !is_axis
            && let Some((n, q)) = crate::noise::canonical(&var.name, &var.kind)
        {
            name = n.to_string();
            quantity = q;
        }
        let data = match &plot.columns {
            Columns::Real(cols) => {
                let mut v = cols[i].clone();
                if is_axis && kind == AnalysisKind::Transient {
                    for t in &mut v {
                        *t = t.abs() + plot.offset;
                    }
                }
                VectorData::Real(v)
            }
            Columns::Complex(cols) => {
                if is_axis && quantity == Quantity::Frequency {
                    // Frequency is real; ngspice leaves junk in the
                    // imaginary half.
                    VectorData::Real(cols[i].iter().map(|c| c.re).collect())
                } else {
                    VectorData::Complex(cols[i].clone())
                }
            }
        };
        vectors.push(Vector {
            name,
            quantity,
            data,
        });
    }

    let stepped = plot.has_flag("stepped");
    let steps = if stepped && kind == AnalysisKind::Op {
        // One point per step; column 0 holds the stepped parameter.
        let param = plot.vars[0].name.clone();
        let values = match &plot.columns {
            Columns::Real(c) => c[0].clone(),
            Columns::Complex(c) => c[0].iter().map(|z| z.re).collect(),
        };
        values
            .iter()
            .enumerate()
            .map(|(i, v)| Step {
                range: i..i + 1,
                label: format!("{param}={}", aispice_core::units::format(*v)),
            })
            .collect()
    } else if stepped {
        let axis_values: Vec<f64> = axis
            .and_then(|a| vectors.get(a))
            .map(|v| v.data.real())
            .unwrap_or_default();
        split_steps(&axis_values, npoints)
    } else {
        vec![Step {
            range: 0..npoints,
            label: String::new(),
        }]
    };

    Ok(Dataset {
        title: plot.title,
        plotname: plot.plotname,
        kind,
        axis: if skip_first { None } else { axis },
        vectors,
        steps,
    })
}

fn axis_name(plot: &RawPlot, var: &RawVar, kind: AnalysisKind) -> String {
    let lower = var.name.to_ascii_lowercase();
    match quantity_of(&var.kind) {
        Quantity::Time => return "time".into(),
        Quantity::Frequency => return "frequency".into(),
        _ => {}
    }
    if kind == AnalysisKind::Dc {
        if lower == "sweep"
            && let Some(src) = xyce_dc_source(&plot.plotname)
        {
            return src;
        }
        // ngspice: v(v-sweep), i(i-sweep).
        if let Some(inner) = lower
            .strip_prefix("v(")
            .or_else(|| lower.strip_prefix("i("))
            .and_then(|s| s.strip_suffix(')'))
        {
            return var.name[2..2 + inner.len()].to_string();
        }
    }
    var.name.clone()
}

/// aispice's name for a vector: `V(node)` for voltages and `I(device)` for
/// currents, whatever form the simulator wrote.
fn vector_name(raw: &str, quantity: Quantity) -> String {
    let n = normalize_name(raw);
    if n.contains('(') {
        return n;
    }
    match quantity {
        Quantity::Voltage => format!("V({n})"),
        Quantity::Current => format!("I({n})"),
        _ => n,
    }
}

/// Split concatenated `.step` runs where the axis returns to its first value.
fn split_steps(axis: &[f64], npoints: usize) -> Vec<Step> {
    let mut starts = vec![0];
    if let Some(&first) = axis.first() {
        let tol = first.abs() * 1e-9;
        for (i, &x) in axis.iter().enumerate().skip(1) {
            if (x - first).abs() <= tol && i > *starts.last().expect("non-empty") {
                starts.push(i);
            }
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| Step {
            range: s..starts.get(k + 1).copied().unwrap_or(npoints),
            label: format!("step {}", k + 1),
        })
        .collect()
}

/// Merge consecutive Xyce `.step` plots with the same variables into one
/// stepped dataset, labelled from the step parameters in the plot names.
fn merge_xyce_steps(datasets: Vec<Dataset>) -> Vec<Dataset> {
    let mut out: Vec<Dataset> = Vec::new();
    let mut merging = false;
    for ds in datasets {
        let Some(params) = xyce_step_params(&ds.plotname) else {
            merging = false;
            out.push(ds);
            continue;
        };
        let label = params
            .iter()
            .map(|(n, v)| {
                let value = v
                    .parse::<f64>()
                    .map(aispice_core::units::format)
                    .unwrap_or_else(|_| v.clone());
                format!("{n}={value}")
            })
            .collect::<Vec<_>>()
            .join(" ");
        let can_merge = merging
            && out.last().is_some_and(|prev| {
                prev.kind == ds.kind && prev.names() == ds.names() && prev.axis == ds.axis
            });
        if can_merge {
            let prev = out.last_mut().expect("checked");
            let start = prev.len();
            for (pv, v) in prev.vectors.iter_mut().zip(ds.vectors) {
                match (&mut pv.data, v.data) {
                    (VectorData::Real(a), VectorData::Real(b)) => a.extend(b),
                    (VectorData::Complex(a), VectorData::Complex(b)) => a.extend(b),
                    (a, b) => {
                        let mut joined = a.as_complex();
                        joined.extend(b.as_complex());
                        *a = VectorData::Complex(joined);
                    }
                }
            }
            let end = prev.len();
            prev.steps.push(Step {
                range: start..end,
                label,
            });
        } else {
            let mut ds = ds;
            let n = ds.len();
            ds.steps = vec![Step { range: 0..n, label }];
            out.push(ds);
            merging = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn ltspice_header(plot: &str, flags: &str, vars: &[(&str, &str)], points: usize) -> String {
        let mut h = format!(
            "Title: * test\nDate: Thu Jan  1 00:00:00 2026\nPlotname: {plot}\nFlags: {flags}\nNo. Variables: {}\nNo. Points: {points:>12}\nOffset:   0.0000000000000000e+000\nCommand: Linear Technology Corporation LTspice XVII\nVariables:\n",
            vars.len()
        );
        for (i, (n, t)) in vars.iter().enumerate() {
            h.push_str(&format!("\t{i}\t{n}\t{t}\n"));
        }
        h.push_str("Binary:\n");
        h
    }

    #[test]
    fn ltspice_mixed_precision_and_negative_time() {
        let mut bytes = utf16(&ltspice_header(
            "Transient Analysis",
            "real forward",
            &[("time", "time"), ("V(out)", "voltage")],
            3,
        ));
        for (t, v) in [(0.0f64, 0.0f32), (-1e-3, 0.5), (2e-3, 0.75)] {
            bytes.extend(t.to_le_bytes());
            bytes.extend(v.to_le_bytes());
        }
        let ds = read_raw(&bytes).unwrap();
        assert_eq!(ds.len(), 1);
        let d = &ds[0];
        assert_eq!(d.kind, AnalysisKind::Transient);
        assert_eq!(d.axis, Some(0));
        assert_eq!(d.vectors[0].data.real(), vec![0.0, 1e-3, 2e-3]);
        assert_eq!(d.vector("out").unwrap().data.real(), vec![0.0, 0.5, 0.75]);
    }

    #[test]
    fn stepped_runs_split_where_the_axis_restarts() {
        let steps = split_steps(&[0.0, 1.0, 2.0, 0.0, 1.0, 0.0, 3.0], 7);
        let ranges: Vec<_> = steps.iter().map(|s| s.range.clone()).collect();
        assert_eq!(ranges, vec![0..3, 3..5, 5..7]);
        assert_eq!(steps[1].label, "step 2");
    }

    #[test]
    fn plot_names_map_to_kinds() {
        assert_eq!(analysis_kind("Transient Analysis"), AnalysisKind::Transient);
        assert_eq!(analysis_kind("AC Analysis"), AnalysisKind::Ac);
        assert_eq!(analysis_kind("Operating Point"), AnalysisKind::Op);
        assert_eq!(
            analysis_kind("DC transfer characteristic"),
            AnalysisKind::Dc
        );
        assert_eq!(
            analysis_kind("Transfer Function"),
            AnalysisKind::TransferFunction
        );
        assert_eq!(
            analysis_kind("Noise Spectral Density - (V/Hz\u{bd} or A/Hz\u{bd})"),
            AnalysisKind::Noise
        );
        assert_eq!(
            analysis_kind(
                "Step Analysis: Step 1 of 2 params:  name = R value = 1000  Transient Analysis"
            ),
            AnalysisKind::Transient
        );
        assert_eq!(analysis_kind("Pole-Zero Analysis"), AnalysisKind::PoleZero);
        assert_eq!(analysis_kind("unknown"), AnalysisKind::Other);
    }

    #[test]
    fn names_get_voltage_and_current_wrappers() {
        assert_eq!(vector_name("OUT", Quantity::Voltage), "V(OUT)");
        assert_eq!(vector_name("v(out)", Quantity::Voltage), "V(out)");
        assert_eq!(vector_name("V1#branch", Quantity::Current), "I(V1)");
        assert_eq!(vector_name("I(R1)", Quantity::Current), "I(R1)");
        assert_eq!(vector_name("gain", Quantity::Other), "gain");
    }

    #[test]
    fn xyce_step_labels_parse() {
        let p = xyce_step_params(
            "Step Analysis: Step 2 of 2 params:  name = R value = 3000  DC Sweep: Step 2 of 3 params:  name = V1 value = 0  DC transfer characteristic",
        )
        .unwrap();
        assert_eq!(p, vec![("R".to_string(), "3000".to_string())]);
        assert_eq!(
            xyce_dc_source(
                "DC Sweep: Step 2 of 3 params:  name = V1 value = 0  DC transfer characteristic"
            ),
            Some("V1".to_string())
        );
    }

    #[test]
    fn spectre_style_nutascii_reads() {
        // Nutmeg ASCII as Spectre writes it with `-format nutascii`: unit
        // names as variable types and bare node names.
        let text = "Title: spectre test\nDate: 12:00:00 AM, Mon Oct 12, 2026\nPlotname: Transient Analysis `tran': time = (0 s -> 1 ms)\nFlags: real\nNo. Variables: 3\nNo. Points: 2\nVariables:\t0\ttime\ts\n\t1\tout\tV\n\t2\tV1:p\tA\nValues:\n0\t0.000000000000000e+00\n\t0.000000000000000e+00\n\t0.000000000000000e+00\n1\t1.000000000000000e-03\n\t6.321205588285577e-01\n\t-3.678794411714423e-04\n";
        let ds = read_raw(text.as_bytes()).unwrap();
        let d = &ds[0];
        assert_eq!(d.kind, AnalysisKind::Transient);
        assert_eq!(d.names(), vec!["time", "V(out)", "I(V1:p)"]);
        assert!((d.vector("out").unwrap().data.real()[1] - 0.632_120_558_8).abs() < 1e-9);
    }

    #[test]
    fn ngspice_pole_zero_plots() {
        // The header ngspice 42 writes for `.pz` on a series RLC, with the
        // complex pair in rad/s.
        let mut bytes = b"Title: * rlc\nDate: Sat Oct 10 16:30:44  2026\nPlotname: Pole-Zero Analysis\nFlags: complex\nNo. Variables: 2\nNo. Points: 1       \nVariables:\n\t0\tv(pole(1))\tvoltage\n\t1\tv(pole(2))\tvoltage\nBinary:\n".to_vec();
        for v in [-5000.0f64, 31225.0, -5000.0, -31225.0] {
            bytes.extend(v.to_le_bytes());
        }
        let d = &read_raw(&bytes).unwrap()[0];
        assert_eq!(d.kind, AnalysisKind::PoleZero);
        assert_eq!(d.axis, None);
        assert_eq!(d.names(), vec!["V(pole(1))", "V(pole(2))"]);
        assert_eq!(
            d.vectors[1].data.as_complex(),
            vec![Complex::new(-5000.0, -31225.0)]
        );

        // No finite poles or zeros (a resistive divider): an empty plot is a
        // result, not a malformed file.
        let empty = b"Title: * resistive only\nDate: Sat Oct 10 16:31:47  2026\nPlotname: Pole-Zero Analysis\nFlags: real\nNo. Variables: 0\nNo. Points: 1       \nVariables:\nBinary:\n";
        let ds = read_raw(empty).unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].kind, AnalysisKind::PoleZero);
        assert!(ds[0].vectors.is_empty());

        // Any other plot without variables still is.
        let bad = b"Title: x\nPlotname: Transient Analysis\nFlags: real\nNo. Variables: 0\nNo. Points: 1\nVariables:\nBinary:\n";
        assert!(matches!(read_raw(bad), Err(RawError::Header(_))));
    }

    #[test]
    fn noise_plots_use_one_naming() {
        // ngspice 42's header for `.noise V(out) I1`: the input-referred
        // density of a current source input is typed as a current.
        let mut bytes = b"Title: * n\nDate: Sun Oct 11 14:44:07  2026\nPlotname: Noise Spectral Density Curves\nFlags: real\nNo. Variables: 3\nNo. Points: 1       \nVariables:\n\t0\tfrequency\tfrequency\tgrid=3\n\t1\tonoise_spectrum\tvoltage-density\n\t2\tinoise_spectrum\tcurrent-density\nBinary:\n".to_vec();
        for v in [1.0f64, 2.9e-9, 2.9e-12] {
            bytes.extend(v.to_le_bytes());
        }
        bytes.extend(b"Title: * n\nDate: x\nPlotname: Integrated Noise\nFlags: real\nNo. Variables: 2\nNo. Points: 1       \nVariables:\n\t0\tv(onoise_total)\tvoltage\n\t1\ti(inoise_total)\tcurrent\nBinary:\n");
        for v in [2.9e-6f64, 2.9e-9] {
            bytes.extend(v.to_le_bytes());
        }
        let ds = read_raw(&bytes).unwrap();
        assert_eq!(ds[0].kind, AnalysisKind::Noise);
        assert_eq!(ds[0].names(), vec!["frequency", "V(onoise)", "V(inoise)"]);
        assert_eq!(ds[0].vectors[2].quantity, Quantity::Current);
        assert_eq!(ds[0].axis, Some(0));
        assert_eq!(ds[1].kind, AnalysisKind::Noise);
        assert_eq!(ds[1].axis, None);

        // Spectre's nutascii noise plot, with per-device contributions kept.
        let text = "Title: * n\nDate: 2:52:59 PM, Sun Oct 11, 2026\nPlotname: Noise Analysis `noise1': freq = (1 Hz -> 1 MHz)\nFlags: real\nNo. Variables:        4\nNo. Points:       1\nVariables:\t0\tfreq\tHz\n\t\t1\tR2.rn\tV^2/Hz plot=0 grid=0\n\t\t2\tout\tV/sqrt(Hz) plot=0 grid=0\n\t\t3\tin\tV/sqrt(Hz) plot=0 grid=0\nValues:\n 0\t1\t4.1e-18\n\t2.87e-09\t5.75e-09\n";
        let d = &read_raw(text.as_bytes()).unwrap()[0];
        assert_eq!(d.kind, AnalysisKind::Noise);
        assert_eq!(
            d.names(),
            vec!["frequency", "R2.rn", "V(onoise)", "V(inoise)"]
        );
        assert_eq!(d.vector("onoise").unwrap().data.real(), vec![2.87e-9]);
    }

    #[test]
    fn rejects_text_that_is_not_raw() {
        assert!(matches!(
            read_raw(b"hello world\n"),
            Err(RawError::NotRaw(_))
        ));
        assert!(read_raw(b"").is_err());
    }

    #[test]
    fn truncated_binary_keeps_complete_points() {
        let mut bytes = utf16(&ltspice_header(
            "Transient Analysis",
            "real forward",
            &[("time", "time"), ("V(out)", "voltage")],
            3,
        ));
        bytes.extend(0.0f64.to_le_bytes());
        bytes.extend(1.0f32.to_le_bytes());
        bytes.extend(1e-3f64.to_le_bytes());
        let d = &read_raw(&bytes).unwrap()[0];
        assert_eq!(d.len(), 1);
    }
}

//! Xyce in serial mode.
//!
//! `Xyce -r deck.raw deck.cir`, with `-plugin a.so,b.so` first for a PDK run
//! whose compiled device models are Xyce plugins. Measurements come from the console, or from
//! the `deck.cir.mt*` files when the console has none (one file per `.step`
//! run). Xyce's binary raw names a DC sweep's axis `sweep`, so the swept
//! source is taken from the deck's `.dc` line.

use super::process::{self, ProcessSpec};
use super::{
    Detection, SimError, SimId, SimJob, SimOutput, Simulator, Workspace, finish, read_log_limited,
    read_raw_limited, which,
};
use crate::dataset::AnalysisKind;
use crate::log::{parse_xyce_mt, parse_xyce_output};
use crate::pdk::PdkRun;
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default)]
pub struct Xyce {
    /// The executable; `Xyce` on PATH when unset.
    pub exe: Option<PathBuf>,
}

impl Xyce {
    fn exe(&self) -> Option<PathBuf> {
        match &self.exe {
            Some(p) => p.is_file().then(|| p.clone()),
            None => which("Xyce").or_else(|| which("xyce")),
        }
    }

    pub fn args(stem: &str) -> Vec<String> {
        Self::args_for(stem, None)
    }

    /// The argument vector, loading a PDK's plugins when it has any. Xyce
    /// takes them as one comma-separated list; the caller's check refused
    /// paths with commas.
    pub fn args_for(stem: &str, pdk: Option<&PdkRun>) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(p) = pdk.filter(|p| !p.xyce_plugins.is_empty()) {
            let list: Vec<String> = p
                .xyce_plugins
                .iter()
                .map(|f| f.display().to_string())
                .collect();
            args.push("-plugin".into());
            args.push(list.join(","));
        }
        args.extend(["-r".into(), format!("{stem}.raw"), format!("{stem}.cir")]);
        args
    }
}

/// The first source swept by a `.dc` line.
fn dc_source(deck: &str) -> Option<String> {
    deck.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        w.next()
            .filter(|k| k.eq_ignore_ascii_case(".dc"))
            .and(w.next())
            .map(|s| {
                if s.eq_ignore_ascii_case("lin")
                    || s.eq_ignore_ascii_case("dec")
                    || s.eq_ignore_ascii_case("oct")
                {
                    w.next().unwrap_or(s).to_string()
                } else {
                    s.to_string()
                }
            })
    })
}

#[async_trait::async_trait]
impl Simulator for Xyce {
    fn id(&self) -> SimId {
        SimId::Xyce
    }

    async fn detect(&self) -> Detection {
        let Some(exe) = self.exe() else {
            return Detection {
                notes: vec!["Xyce is not on PATH".into()],
                ..Default::default()
            };
        };
        let mut d = Detection {
            found: true,
            path: Some(exe.clone()),
            ..Default::default()
        };
        if let Ok(ws) = Workspace::new() {
            let spec = ProcessSpec::new(&exe, ws.path())
                .arg("-v")
                .env("HOME", ws.path());
            if let Ok(out) =
                process::run(&spec, Duration::from_secs(10), &CancellationToken::new()).await
            {
                d.version = out.combined().lines().find_map(|l| {
                    l.trim()
                        .strip_prefix("Xyce Release ")
                        .map(|v| v.trim().to_string())
                });
            }
        }
        d
    }

    async fn run(&self, job: &SimJob, cancel: &CancellationToken) -> Result<SimOutput, SimError> {
        let exe = self
            .exe()
            .ok_or_else(|| SimError::NotFound("Xyce".into()))?;
        let stem = job.stem();
        let ws = Workspace::new()?;
        ws.write(&format!("{stem}.cir"), job.netlist_text.as_bytes())?;
        let mut spec = ProcessSpec::new(&exe, ws.path()).env("HOME", ws.path());
        for a in Self::args_for(&stem, job.pdk.as_ref()) {
            spec = spec.arg(a);
        }
        let out = process::run(&spec, job.timeout, cancel).await?;
        let log = out.combined();
        let mut report = parse_xyce_output(&log);

        // Measure files: deck.cir.mt0, .mt1, ... one per step.
        let mut mt_files: Vec<String> = std::fs::read_dir(ws.path())
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| n.starts_with(&format!("{stem}.cir.mt")))
                    .collect()
            })
            .unwrap_or_default();
        mt_files.sort();
        if report.measurements.is_empty() {
            let stepped = mt_files.len() > 1;
            for (k, f) in mt_files.iter().enumerate() {
                if let Some(bytes) = read_log_limited(&ws.file(f)) {
                    for mut m in parse_xyce_mt(&String::from_utf8_lossy(&bytes)) {
                        m.step = stepped.then_some(k);
                        report.measurements.push(m);
                    }
                }
            }
        }

        let raw = ws.file(&format!("{stem}.raw"));
        let mut datasets = if raw.exists() {
            read_raw_limited(&raw)?
        } else {
            Vec::new()
        };
        if let Some(src) = dc_source(&job.netlist_text) {
            for ds in datasets.iter_mut().filter(|d| d.kind == AnalysisKind::Dc) {
                if let Some(a) = ds.axis
                    && ds.vectors[a].name.eq_ignore_ascii_case("sweep")
                {
                    ds.vectors[a].name = src.clone();
                }
            }
        }
        let mut keep = vec![format!("{stem}.cir"), format!("{stem}.raw")];
        keep.extend(mt_files);
        let files = ws.keep(&job.run_dir, &keep)?;
        let raw_path = files
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == "raw"))
            .cloned();
        let result = SimOutput {
            datasets,
            log,
            measurements: report.measurements,
            errors: report.errors,
            warnings: report.warnings,
            duration: out.elapsed,
            raw_path,
            files,
        };
        finish(result, out.success(), "Xyce wrote no results")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_and_dc_source() {
        assert_eq!(Xyce::args("d"), vec!["-r", "d.raw", "d.cir"]);
        let pdk = PdkRun {
            osdi: vec![PathBuf::from("/pdk/x.osdi")],
            xyce_plugins: vec![PathBuf::from("/pdk/a.so"), PathBuf::from("/pdk/b.so")],
        };
        assert_eq!(
            Xyce::args_for("d", Some(&pdk)),
            vec!["-plugin", "/pdk/a.so,/pdk/b.so", "-r", "d.raw", "d.cir"]
        );
        let no_plugins = PdkRun::default();
        assert_eq!(Xyce::args_for("d", Some(&no_plugins)), Xyce::args("d"));
        assert_eq!(dc_source("t\nV1 a 0 1\n.dc V1 0 10 1\n"), Some("V1".into()));
        assert_eq!(dc_source("t\n.DC lin Vin 0 1 0.1\n"), Some("Vin".into()));
        assert_eq!(dc_source("t\n.op\n"), None);
    }
}

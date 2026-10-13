//! LTspice: native on Windows and macOS, under Wine on Linux.
//!
//! `LTspice -b deck.net` writes `deck.raw`, `deck.log`, and `deck.op.raw`
//! and `deck.log.raw` when the run has an operating point next to another
//! analysis or stepped measurements. Under Wine every invocation needs an X
//! display; without one LTspice exits 0 in about a second having written
//! nothing, so a run with no log and no raw file is reported as that, not as
//! an empty result. When no display is set (or `headless` is asked for) the
//! run goes through `xvfb-run -a`.
//!
//! `-netlist file.asc` gives LTspice's own netlist of a schematic, the
//! reference aispice's netlister is tested against. macOS LTspice has no
//! `-netlist`, so there aispice's netlister is the only one.

use super::process::{self, ProcessSpec};
use super::{
    Detection, SimError, SimId, SimJob, SimOutput, Simulator, Workspace, finish, read_log_limited,
    read_raw_limited, sanitize_name, which,
};
use crate::log::parse_ltspice_log_bytes;
use crate::raw::apply_step_labels;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
    Wine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    /// LTspice XVII (`XVIIx64.exe`).
    Xvii,
    /// LTspice 24 and later (`LTspice.exe`, ADI).
    Modern,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub exe: PathBuf,
    pub platform: Platform,
    pub edition: Edition,
}

#[derive(Debug, Clone, Default)]
pub struct Ltspice {
    /// The LTspice executable. Searched in the standard install locations
    /// when unset.
    pub exe: Option<PathBuf>,
    /// Wine, for Linux; `wine` on PATH when unset.
    pub wine: Option<PathBuf>,
    /// The Wine prefix; `$WINEPREFIX` or `~/.wine` when unset.
    pub wine_prefix: Option<PathBuf>,
    /// Run under `xvfb-run`. `None` decides by whether a display is set.
    pub headless: Option<bool>,
    /// Extra environment for the LTspice process, such as `DISPLAY` or
    /// `WINEDEBUG`, applied last.
    pub env: Vec<(String, String)>,
}

impl Ltspice {
    fn prefix(&self) -> Option<PathBuf> {
        self.wine_prefix
            .clone()
            .or_else(|| std::env::var_os("WINEPREFIX").map(PathBuf::from))
            .or_else(|| dirs::home_dir().map(|h| h.join(".wine")))
    }

    /// User folders inside the Wine prefix (`drive_c/users/<name>`).
    fn wine_users(&self) -> Vec<PathBuf> {
        let Some(prefix) = self.prefix() else {
            return Vec::new();
        };
        let mut users: Vec<PathBuf> = std::fs::read_dir(prefix.join("drive_c/users"))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        // The current user's folder first.
        let me = std::env::var("USER").unwrap_or_default();
        users.sort_by_key(|p| p.file_name().is_none_or(|n| n.to_string_lossy() != me));
        users
    }

    fn edition_of(exe: &Path) -> Edition {
        let name = exe
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if name.starts_with("xvii") {
            Edition::Xvii
        } else {
            Edition::Modern
        }
    }

    fn platform() -> Platform {
        if cfg!(windows) {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Wine
        }
    }

    /// Where LTspice is installed, if anywhere.
    pub fn find(&self) -> Option<Install> {
        let platform = Self::platform();
        let candidates: Vec<PathBuf> = match &self.exe {
            Some(p) => vec![p.clone()],
            None => match platform {
                Platform::Windows => {
                    let mut c = Vec::new();
                    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                        c.push(PathBuf::from(local).join("Programs\\ADI\\LTspice\\LTspice.exe"));
                    }
                    for pf in ["ProgramFiles", "ProgramW6432"] {
                        if let Some(p) = std::env::var_os(pf) {
                            c.push(PathBuf::from(&p).join("ADI\\LTspice\\LTspice.exe"));
                            c.push(PathBuf::from(&p).join("LTC\\LTspiceXVII\\XVIIx64.exe"));
                        }
                    }
                    c.push(PathBuf::from(
                        "C:\\Program Files\\ADI\\LTspice\\LTspice.exe",
                    ));
                    c.push(PathBuf::from(
                        "C:\\Program Files\\LTC\\LTspiceXVII\\XVIIx64.exe",
                    ));
                    c
                }
                Platform::MacOs => {
                    vec![PathBuf::from(
                        "/Applications/LTspice.app/Contents/MacOS/LTspice",
                    )]
                }
                Platform::Wine => {
                    let mut c = Vec::new();
                    for user in self.wine_users() {
                        c.push(user.join("AppData/Local/Programs/ADI/LTspice/LTspice.exe"));
                    }
                    if let Some(prefix) = self.prefix() {
                        c.push(prefix.join("drive_c/Program Files/ADI/LTspice/LTspice.exe"));
                        c.push(prefix.join("drive_c/Program Files/LTC/LTspiceXVII/XVIIx64.exe"));
                    }
                    c
                }
            },
        };
        candidates
            .into_iter()
            .find(|p| p.is_file())
            .map(|exe| Install {
                edition: Self::edition_of(&exe),
                exe,
                platform,
            })
    }

    /// LTspice's library folders (each holding `sym`, `sub`, `cmp`), the one
    /// LTspice actually reads first.
    pub fn lib_dirs(&self) -> Vec<PathBuf> {
        let Some(install) = self.find() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match (install.platform, install.edition) {
            (Platform::Wine, Edition::Xvii) => {
                for user in self.wine_users() {
                    out.push(user.join("Documents/LTspiceXVII/lib"));
                }
            }
            (Platform::Wine, Edition::Modern) => {
                for user in self.wine_users() {
                    out.push(user.join("AppData/Local/LTspice/lib"));
                }
            }
            (Platform::Windows, Edition::Xvii) => {
                if let Some(docs) = dirs::document_dir() {
                    out.push(docs.join("LTspiceXVII").join("lib"));
                }
            }
            (Platform::Windows, Edition::Modern) => {
                if let Some(local) = dirs::data_local_dir() {
                    out.push(local.join("LTspice").join("lib"));
                }
            }
            (Platform::MacOs, _) => {
                if let Some(home) = dirs::home_dir() {
                    out.push(home.join("Library/Application Support/LTspice/lib"));
                }
            }
        }
        if let Some(dir) = install.exe.parent() {
            out.push(dir.join("lib"));
        }
        out.retain(|d| d.is_dir());
        out.dedup();
        out
    }

    fn headless(&self) -> bool {
        self.headless.unwrap_or_else(|| {
            std::env::var_os("DISPLAY").is_none_or(|d| d.is_empty())
                && std::env::var_os("WAYLAND_DISPLAY").is_none_or(|d| d.is_empty())
        })
    }

    /// The process for `LTspice <args>` run from `cwd`.
    pub fn command(
        &self,
        install: &Install,
        cwd: &Path,
        args: &[String],
    ) -> Result<ProcessSpec, SimError> {
        let spec = match install.platform {
            Platform::Windows | Platform::MacOs => {
                let mut spec = ProcessSpec::new(&install.exe, cwd);
                for a in args {
                    spec = spec.arg(a);
                }
                spec
            }
            Platform::Wine => {
                let wine = match &self.wine {
                    Some(w) => w.clone(),
                    None => which("wine").ok_or_else(|| SimError::NotFound("wine".into()))?,
                };
                let mut spec = if self.headless() {
                    let xvfb = which("xvfb-run").ok_or_else(|| {
                        SimError::NotFound(
                            "xvfb-run (LTspice under Wine needs an X display, and none is set)"
                                .into(),
                        )
                    })?;
                    ProcessSpec::new(xvfb, cwd).arg("-a").arg(wine.as_os_str())
                } else {
                    ProcessSpec::new(wine, cwd)
                };
                spec = spec.arg(install.exe.as_os_str()).env("WINEDEBUG", "-all");
                if let Some(p) = &self.wine_prefix {
                    spec = spec.env("WINEPREFIX", p.as_os_str());
                }
                for a in args {
                    spec = spec.arg(a);
                }
                spec
            }
        };
        Ok(self
            .env
            .iter()
            .fold(spec, |s, (k, v)| s.env(k.as_str(), v.as_str())))
    }

    /// Arguments for a batch run of `file`.
    pub fn batch_args(edition: Edition, file: &str) -> Vec<String> {
        match edition {
            Edition::Xvii => vec!["-b".into(), file.into()],
            // LTspice 24+ reads its settings from the -ini file given after
            // the deck; an empty one keeps the user's settings out of the run.
            Edition::Modern => vec![
                "-Run".into(),
                "-b".into(),
                file.into(),
                "-ini".into(),
                "aispice.ini".into(),
            ],
        }
    }

    /// LTspice's own netlist of a schematic.
    pub async fn netlist_asc(&self, asc_path: &Path) -> Result<String, SimError> {
        self.netlist_asc_with(asc_path, Duration::from_secs(60), &CancellationToken::new())
            .await
    }

    /// [`Ltspice::netlist_asc`] with a timeout and cancellation. The
    /// schematic and the symbol, block and library files it names from its
    /// own folder are copied into a private directory, and LTspice netlists
    /// the copy, so nothing is written next to the user's file.
    pub async fn netlist_asc_with(
        &self,
        asc_path: &Path,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<String, SimError> {
        let install = self
            .find()
            .ok_or_else(|| SimError::NotFound("LTspice".into()))?;
        if install.platform == Platform::MacOs {
            return Err(SimError::Unsupported(
                "LTspice's -netlist on macOS (use aispice's netlister)".into(),
            ));
        }
        let bytes = std::fs::read(asc_path)
            .map_err(|e| SimError::Io(format!("{}: {e}", asc_path.display())))?;
        let stem = sanitize_name(
            &asc_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        let ws = Workspace::new()?;
        // A byte order mark makes LTspice open a dialog and hang.
        let body = bytes
            .strip_prefix(&[0xEF, 0xBB, 0xBF])
            .or_else(|| bytes.strip_prefix(&[0xFF, 0xFE]))
            .unwrap_or(&bytes);
        ws.write(&format!("{stem}.asc"), body)?;
        if let Some(folder) = asc_path.parent() {
            copy_references(body, folder, &ws, 0)?;
        }
        if install.edition == Edition::Modern {
            ws.write("aispice.ini", b"")?;
        }
        let spec = self.command(
            &install,
            ws.path(),
            &["-netlist".to_string(), format!("{stem}.asc")],
        )?;
        let out = process::run(&spec, timeout, cancel).await?;
        let net = ws.file(&format!("{stem}.net"));
        let Some(bytes) = read_log_limited(&net) else {
            return Err(SimError::NoOutput(no_output_message(
                install.platform,
                &out.combined(),
            )));
        };
        let (text, _) = aispice_core::encoding::decode(&bytes);
        let mut lines = text.lines();
        lines.next();
        let mut result = format!("* {}\n", asc_path.display());
        for l in lines {
            result.push_str(l.trim_end_matches('\r'));
            result.push('\n');
        }
        Ok(result)
    }
}

/// Wine prints this on every start; it means nothing for LTspice.
fn is_wine_noise(line: &str) -> bool {
    let l = line.trim();
    l.is_empty()
        || l.contains("Read access denied for device")
        || l.starts_with("X connection to")
        || l.contains("fixme:")
        || l.starts_with("wine: using fast synchronization")
}

fn filter_wine(text: &str) -> String {
    text.lines()
        .filter(|l| !is_wine_noise(l))
        .collect::<Vec<_>>()
        .join("\n")
}

fn no_output_message(platform: Platform, output: &str) -> String {
    let mut msg = String::from("LTspice exited without writing any output.");
    if platform == Platform::Wine {
        msg.push_str(
            " Under Wine this happens when it cannot reach an X display: run headless (xvfb-run) or check DISPLAY.",
        );
    }
    let rest = filter_wine(output);
    if !rest.trim().is_empty() {
        msg.push_str(&format!(" Output: {}", rest.trim()));
    }
    msg
}

/// Copy the files an `.asc` names from its own folder: symbols and
/// hierarchical blocks by symbol name, and `.lib`/`.include` files and
/// `SpiceModel` libraries by file name. Files elsewhere are left to LTspice's
/// library search; nothing outside `folder` is copied.
fn copy_references(
    asc: &[u8],
    folder: &Path,
    ws: &Workspace,
    depth: usize,
) -> Result<(), SimError> {
    if depth > 3 {
        return Ok(());
    }
    let (text, _) = aispice_core::encoding::decode(asc);
    let listing: Vec<(String, PathBuf)> = std::fs::read_dir(folder)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_file())
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().to_ascii_lowercase(),
                        e.path(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let mut wanted: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let mut words = line.split_whitespace();
        match words.next() {
            Some("SYMBOL") => {
                if let Some(name) = words.next() {
                    let base = name.rsplit(['\\', '/']).next().unwrap_or(name);
                    wanted.push(format!("{base}.asy"));
                    wanted.push(format!("{base}.asc"));
                }
            }
            Some("SYMATTR") => {
                let key = words.next().unwrap_or("");
                if key.eq_ignore_ascii_case("SpiceModel") || key.eq_ignore_ascii_case("ModelFile") {
                    let v: Vec<&str> = words.collect();
                    let v = v.join(" ");
                    if v.contains('.') {
                        wanted.push(file_base(&v));
                    }
                }
            }
            Some("TEXT") => {
                if let Some(i) = line.find('!') {
                    for d in line[i + 1..].split("\\n") {
                        let mut w = d.split_whitespace();
                        let kw = w.next().unwrap_or("").to_ascii_lowercase();
                        if matches!(kw.as_str(), ".lib" | ".inc" | ".include")
                            && let Some(f) = w.next()
                        {
                            wanted.push(file_base(f));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for want in wanted {
        let key = want.to_ascii_lowercase();
        if let Some((lower, path)) = listing.iter().find(|(n, _)| *n == key) {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| lower.clone());
            if ws.file(&name).exists() {
                continue;
            }
            let bytes = std::fs::read(path)
                .map_err(|e| SimError::Io(format!("{}: {e}", path.display())))?;
            ws.write(&name, &bytes)?;
            if lower.ends_with(".asc") {
                copy_references(&bytes, folder, ws, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn file_base(s: &str) -> String {
    let s = s.trim().trim_matches('"');
    s.rsplit(['\\', '/']).next().unwrap_or(s).to_string()
}

#[async_trait::async_trait]
impl Simulator for Ltspice {
    fn id(&self) -> SimId {
        SimId::Ltspice
    }

    async fn detect(&self) -> Detection {
        let Some(install) = self.find() else {
            return Detection {
                notes: vec!["no LTspice installation found in the standard locations".into()],
                ..Default::default()
            };
        };
        let mut d = Detection {
            found: true,
            path: Some(install.exe.clone()),
            version: Some(
                match install.edition {
                    Edition::Xvii => "XVII",
                    Edition::Modern => "24 or later",
                }
                .to_string(),
            ),
            notes: Vec::new(),
        };
        if install.platform == Platform::Wine {
            match &self.wine {
                Some(w) if w.is_file() => {
                    d.notes.push(format!("runs under Wine ({})", w.display()))
                }
                Some(w) => {
                    d.found = false;
                    d.notes
                        .push(format!("configured Wine {} does not exist", w.display()));
                }
                None => match which("wine") {
                    Some(w) => d.notes.push(format!("runs under Wine ({})", w.display())),
                    None => {
                        d.found = false;
                        d.notes.push(
                            "LTspice is installed in a Wine prefix but wine is not on PATH".into(),
                        );
                    }
                },
            }
            if self.headless() {
                if which("xvfb-run").is_some() {
                    d.notes.push("no display: runs under xvfb-run".into());
                } else {
                    d.found = false;
                    d.notes
                        .push("no display and no xvfb-run: Wine cannot run LTspice".into());
                }
            }
        }
        if install.platform == Platform::MacOs {
            d.notes.push(
                "macOS LTspice runs netlists only; schematics are netlisted by aispice".into(),
            );
        }
        for lib in self.lib_dirs() {
            d.notes.push(format!("library: {}", lib.display()));
        }
        d
    }

    async fn run(&self, job: &SimJob, cancel: &CancellationToken) -> Result<SimOutput, SimError> {
        job.ngspice_only(SimId::Ltspice)?;
        let install = self
            .find()
            .ok_or_else(|| SimError::NotFound("LTspice".into()))?;
        let stem = job.stem();
        let ws = Workspace::new()?;
        // LTspice reads netlists as 8-bit text; the micro and section signs
        // must arrive as single Latin-1 bytes.
        let deck = aispice_core::encoding::encode(
            &job.netlist_text,
            aispice_core::encoding::Encoding::Latin1,
        );
        let net = format!("{stem}.net");
        ws.write(&net, &deck)?;
        if install.edition == Edition::Modern {
            ws.write("aispice.ini", b"")?;
        }
        let spec = self.command(
            &install,
            ws.path(),
            &Self::batch_args(install.edition, &net),
        )?;
        let names = [
            format!("{stem}.raw"),
            format!("{stem}.op.raw"),
            format!("{stem}.log"),
            format!("{stem}.log.raw"),
        ];
        let started = std::time::Instant::now();
        let mut out = process::run(&spec, job.timeout, cancel).await?;
        let wrote_nothing = |ws: &Workspace| !names.iter().any(|n| ws.file(n).exists());
        if wrote_nothing(&ws) && install.platform == Platform::Wine && self.headless() {
            // All Wine processes in a prefix share one session; a run that
            // joins a session started without a display (by another
            // program) fails silently. That session ends within moments,
            // so one retry is enough.
            tokio::time::sleep(Duration::from_secs(2)).await;
            let left = job.timeout.saturating_sub(started.elapsed());
            out = process::run(&spec, left, cancel).await?;
        }
        if wrote_nothing(&ws) {
            return Err(SimError::NoOutput(no_output_message(
                install.platform,
                &out.combined(),
            )));
        }
        let (log, report) = match read_log_limited(&ws.file(&names[2])) {
            Some(bytes) => parse_ltspice_log_bytes(&bytes),
            None => {
                let text = filter_wine(&out.combined());
                let r = crate::log::parse_ltspice_log(&text);
                (text, r)
            }
        };
        let mut datasets = Vec::new();
        for n in &names[..2] {
            let p = ws.file(n);
            if p.exists() {
                datasets.extend(read_raw_limited(&p)?);
            }
        }
        for ds in datasets.iter_mut() {
            apply_step_labels(ds, &report.step_labels);
        }
        crate::noise::mark_input_quantity(&mut datasets, &job.netlist_text);
        let mut keep = vec![net.clone()];
        keep.extend(names.iter().cloned());
        let files = ws.keep(&job.run_dir, &keep)?;
        let raw_path = files
            .iter()
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy() == names[0])
            })
            .or_else(|| {
                files
                    .iter()
                    .find(|p| p.to_string_lossy().ends_with(".op.raw"))
            })
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
        finish(result, out.success(), "LTspice wrote no results")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editions_and_batch_args() {
        assert_eq!(
            Ltspice::edition_of(Path::new("/x/LTC/LTspiceXVII/XVIIx64.exe")),
            Edition::Xvii
        );
        assert_eq!(
            Ltspice::edition_of(Path::new("C:/Program Files/ADI/LTspice/LTspice.exe")),
            Edition::Modern
        );
        assert_eq!(
            Ltspice::batch_args(Edition::Xvii, "d.net"),
            vec!["-b", "d.net"]
        );
        assert_eq!(
            Ltspice::batch_args(Edition::Modern, "d.net"),
            vec!["-Run", "-b", "d.net", "-ini", "aispice.ini"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wine_command_uses_xvfb_when_headless() {
        if which("xvfb-run").is_none() {
            return;
        }
        let lt = Ltspice {
            wine: Some(PathBuf::from("/usr/bin/wine")),
            headless: Some(true),
            ..Default::default()
        };
        let install = Install {
            exe: PathBuf::from("/p/XVIIx64.exe"),
            platform: Platform::Wine,
            edition: Edition::Xvii,
        };
        let spec = lt
            .command(
                &install,
                Path::new("/tmp"),
                &Ltspice::batch_args(Edition::Xvii, "d.net"),
            )
            .unwrap();
        let shown = spec.display();
        assert!(
            shown.ends_with("xvfb-run -a /usr/bin/wine /p/XVIIx64.exe -b d.net"),
            "{shown}"
        );
        let windowed = Ltspice {
            headless: Some(false),
            ..lt
        };
        let spec = windowed
            .command(&install, Path::new("/tmp"), &["-b".into(), "d.net".into()])
            .unwrap();
        assert_eq!(spec.display(), "/usr/bin/wine /p/XVIIx64.exe -b d.net");
    }

    #[test]
    fn wine_noise_is_filtered() {
        let out = "wine: Read access denied for device L\"\\\\??\\\\Z:\\\\\", FS volume label and serial are not available.\nX connection to :99 broken (explicit kill or server shutdown).\nreal message\n";
        assert_eq!(filter_wine(out), "real message");
    }

    #[test]
    fn references_are_copied_from_the_schematic_folder_only() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("MyPart.asy"), "SymbolType CELL\n").unwrap();
        std::fs::write(src.path().join("models.lib"), ".model X D\n").unwrap();
        std::fs::write(src.path().join("unrelated.txt"), "x").unwrap();
        let asc = b"Version 4\nSYMBOL mypart 0 0 R0\nSYMBOL res 0 0 R0\nTEXT 0 0 Left 2 !.lib models.lib\\n.tran 1m\nTEXT 0 0 Left 2 !.inc C:\\elsewhere\\other.lib\n";
        let ws = Workspace::new().unwrap();
        copy_references(asc, src.path(), &ws, 0).unwrap();
        assert!(ws.file("MyPart.asy").exists());
        assert!(ws.file("models.lib").exists());
        assert!(!ws.file("unrelated.txt").exists());
        assert!(!ws.file("other.lib").exists());
    }
}

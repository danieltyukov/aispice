//! Cadence Spectre on a remote server over SSH.
//!
//! Spectre is licensed and usually lives on a department or company server,
//! so aispice copies the deck there with `scp`, runs Spectre with `ssh`, and
//! copies the nutascii raw output and log back. The host, the remote folder
//! and the remote command (typically a login shell that sources the Cadence
//! setup first) come only from the user's configuration, never from a model
//! or a netlist. Locally every step is an argument vector; the one string a
//! shell sees is the remote command, built from the configured template with
//! values aispice generated and checked to be plain path characters.

use super::process::{self, ProcessSpec};
use super::{
    Detection, SimError, SimId, SimJob, SimOutput, Simulator, Workspace, finish, read_log_limited,
    read_raw_limited,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// The remote command when none is configured. Placeholders: `{dir}` the
/// run folder, `{deck}` the netlist, `{raw}` the raw output, `{log}` the log.
pub const DEFAULT_COMMAND: &str =
    "cd {dir} && spectre {deck} -format nutascii -raw {raw} =log {log}";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpectreConfig {
    /// The SSH destination: `user@host` or a `Host` alias from
    /// `~/.ssh/config`.
    pub host: String,
    /// Folder on the server where run folders are created, absolute or
    /// relative to the remote home.
    pub remote_dir: String,
    /// Remote command template, for example
    /// `tcsh -f -c 'source /opt/cadence/setup.csh; cd {dir} && spectre {deck} -format nutascii -raw {raw} =log {log}'`.
    #[serde(default = "default_command")]
    pub command: String,
    /// Extra `ssh`/`scp` options, such as `-o BatchMode=yes` or `-p 2222`
    /// (scp needs `-P`; use `-o Port=2222` to cover both).
    #[serde(default)]
    pub ssh_options: Vec<String>,
    #[serde(default)]
    pub ssh: Option<PathBuf>,
    #[serde(default)]
    pub scp: Option<PathBuf>,
}

fn default_command() -> String {
    DEFAULT_COMMAND.into()
}

impl SpectreConfig {
    pub fn new(host: impl Into<String>, remote_dir: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            remote_dir: remote_dir.into(),
            command: default_command(),
            ssh_options: vec!["-o".into(), "BatchMode=yes".into()],
            ssh: None,
            scp: None,
        }
    }

    /// Reject configurations that could be read as options or reach outside
    /// the run folder.
    pub fn validate(&self) -> Result<(), SimError> {
        let host_ok = !self.host.is_empty()
            && !self.host.starts_with('-')
            && self
                .host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-@".contains(c));
        if !host_ok {
            return Err(SimError::Config(format!(
                "Spectre host `{}` must be a plain host name or user@host",
                self.host
            )));
        }
        if !is_plain_path(&self.remote_dir) || self.remote_dir.starts_with('-') {
            return Err(SimError::Config(format!(
                "Spectre remote folder `{}` may only use letters, digits and ._/- (no ~, spaces or ..)",
                self.remote_dir
            )));
        }
        if !self.command.contains("{deck}") {
            return Err(SimError::Config(
                "the Spectre command template must contain {deck}".into(),
            ));
        }
        Ok(())
    }
}

fn is_plain_path(s: &str) -> bool {
    !s.is_empty()
        && !s.split('/').any(|part| part == "..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
}

#[derive(Debug, Clone, Default)]
pub struct Spectre {
    pub config: Option<SpectreConfig>,
}

/// The commands one remote run executes, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub remote_dir: String,
    pub mkdir: Vec<OsString>,
    pub upload: Vec<OsString>,
    pub run: Vec<OsString>,
    pub download: Vec<OsString>,
    pub cleanup: Vec<OsString>,
}

impl Spectre {
    pub fn new(config: SpectreConfig) -> Self {
        Self {
            config: Some(config),
        }
    }

    fn config(&self) -> Result<&SpectreConfig, SimError> {
        let c = self.config.as_ref().ok_or_else(|| {
            SimError::Config("Spectre needs an SSH host, a remote folder and a command".into())
        })?;
        c.validate()?;
        Ok(c)
    }

    /// Build the commands for a deck named `stem` in a run folder named with
    /// `nonce`, downloading into `local_dir`.
    pub fn plan(&self, stem: &str, nonce: &str, local_dir: &Path) -> Result<Plan, SimError> {
        let c = self.config()?;
        let stem = super::sanitize_name(stem);
        let nonce = super::sanitize_name(nonce);
        let dir = format!(
            "{}/aispice-{stem}-{nonce}",
            c.remote_dir.trim_end_matches('/')
        );
        if !is_plain_path(&dir) {
            return Err(SimError::Config(format!("bad remote folder {dir}")));
        }
        let deck = format!("{stem}.scs");
        let raw = format!("{stem}.raw");
        let log = format!("{stem}.log");
        let measure = format!("{stem}.measure");
        let ssh = c.ssh.clone().unwrap_or_else(|| PathBuf::from("ssh"));
        let scp = c.scp.clone().unwrap_or_else(|| PathBuf::from("scp"));
        let ssh_cmd = |remote: String| -> Vec<OsString> {
            let mut v: Vec<OsString> = vec![ssh.clone().into()];
            v.extend(c.ssh_options.iter().map(OsString::from));
            v.push("--".into());
            v.push(c.host.clone().into());
            v.push(remote.into());
            v
        };
        let scp_cmd = |recursive: bool, args: Vec<OsString>| -> Vec<OsString> {
            let mut v: Vec<OsString> = vec![scp.clone().into(), "-q".into()];
            if recursive {
                v.push("-r".into());
            }
            v.extend(c.ssh_options.iter().map(OsString::from));
            v.push("--".into());
            v.extend(args);
            v
        };
        let remote_command = c
            .command
            .replace("{dir}", &dir)
            .replace("{deck}", &deck)
            .replace("{raw}", &raw)
            .replace("{log}", &log);
        let mut download_to = local_dir.as_os_str().to_owned();
        download_to.push("/");
        Ok(Plan {
            // The base folder may be shared (a /tmp on a department server).
            // The run folder is created with `mkdir` and no `-p`, so it fails
            // if anything already sits at that path, a planted symlink
            // included, and it is private to this user from the start.
            mkdir: ssh_cmd(format!(
                "umask 077 && mkdir -p {base} && mkdir -m 700 {dir}",
                base = c.remote_dir.trim_end_matches('/')
            )),
            upload: scp_cmd(
                false,
                vec![
                    local_dir.join(&deck).into_os_string(),
                    format!("{}:{dir}/{deck}", c.host).into(),
                ],
            ),
            run: ssh_cmd(remote_command),
            // `.meas` results go to their own file, which only a deck with
            // `.meas` lines makes: last, so a missing one stops nothing.
            download: scp_cmd(
                true,
                vec![
                    format!("{}:{dir}/{raw}", c.host).into(),
                    format!("{}:{dir}/{log}", c.host).into(),
                    format!("{}:{dir}/{measure}", c.host).into(),
                    download_to,
                ],
            ),
            cleanup: ssh_cmd(format!("rm -rf {dir}")),
            remote_dir: dir,
        })
    }
}

fn spec(argv: &[OsString], cwd: &Path) -> ProcessSpec {
    let mut s = ProcessSpec::new(PathBuf::from(&argv[0]), cwd);
    for a in &argv[1..] {
        s = s.arg(a.clone());
    }
    s
}

/// An unpredictable folder suffix. The standard library seeds each
/// `RandomState` from the operating system's random source, so hashing with
/// two fresh ones gives 128 bits nobody on the remote host can guess.
fn nonce() -> String {
    use std::hash::{BuildHasher, Hasher};
    let part = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        // The clock and the process id only make runs distinct; the
        // unpredictability comes from the hasher's random keys.
        if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            h.write_u128(d.as_nanos());
        }
        h.write_u32(std::process::id());
        h.finish()
    };
    format!("{:016x}{:016x}", part(), part())
}

#[async_trait::async_trait]
impl Simulator for Spectre {
    fn id(&self) -> SimId {
        SimId::Spectre
    }

    /// Reachability only: the remote setup that puts `spectre` on PATH is
    /// part of the configured command, so a dry run would need a license.
    async fn detect(&self) -> Detection {
        let c = match self.config() {
            Ok(c) => c,
            Err(e) => {
                return Detection {
                    notes: vec![e.to_string()],
                    ..Default::default()
                };
            }
        };
        let mut argv: Vec<OsString> = vec![c.ssh.clone().unwrap_or_else(|| "ssh".into()).into()];
        argv.extend(c.ssh_options.iter().map(OsString::from));
        argv.extend(["-o", "ConnectTimeout=5", "--"].map(OsString::from));
        argv.push(c.host.clone().into());
        argv.push("true".into());
        let cwd = std::env::temp_dir();
        let reachable = matches!(
            process::run(&spec(&argv, &cwd), Duration::from_secs(15), &CancellationToken::new()).await,
            Ok(out) if out.success()
        );
        Detection {
            found: reachable,
            version: None,
            path: None,
            notes: vec![if reachable {
                format!("{} is reachable over SSH", c.host)
            } else {
                format!("cannot reach {} over SSH", c.host)
            }],
        }
    }

    async fn run(&self, job: &SimJob, cancel: &CancellationToken) -> Result<SimOutput, SimError> {
        let stem = job.stem();
        let ws = Workspace::new()?;
        let plan = self.plan(&stem, &nonce(), ws.path())?;
        ws.write(&format!("{stem}.scs"), job.netlist_text.as_bytes())?;
        let deadline = Instant::now() + job.timeout;
        let left = || deadline.saturating_duration_since(Instant::now());
        let step = |argv: &Vec<OsString>| spec(argv, ws.path());

        let mk = process::run(&step(&plan.mkdir), left(), cancel).await?;
        if !mk.success() {
            return Err(SimError::Failed {
                errors: vec![format!(
                    "could not create {} on the server",
                    plan.remote_dir
                )],
                log: mk.combined(),
            });
        }
        let up = process::run(&step(&plan.upload), left(), cancel).await?;
        if !up.success() {
            return Err(SimError::Failed {
                errors: vec!["could not copy the deck to the server".into()],
                log: up.combined(),
            });
        }
        let run = process::run(&step(&plan.run), left(), cancel).await;
        // Fetch whatever exists even after a failed run: the log says why.
        let down = process::run(
            &step(&plan.download),
            left().max(Duration::from_secs(30)),
            cancel,
        )
        .await;
        let _ = process::run(&step(&plan.cleanup), Duration::from_secs(30), cancel).await;
        let run = run?;
        if let Err(e) = down {
            tracing::debug!("spectre download: {e}");
        }

        let mut log = run.combined();
        if let Some(bytes) = read_log_limited(&ws.file(&format!("{stem}.log"))) {
            log.push('\n');
            log.push_str(&String::from_utf8_lossy(&bytes));
        }
        let mut report = crate::log::parse_spectre_output(&log);
        if let Some(bytes) = read_log_limited(&ws.file(&format!("{stem}.measure"))) {
            report
                .measurements
                .extend(crate::log::parse_spectre_measure(&String::from_utf8_lossy(
                    &bytes,
                )));
        }
        let raw = ws.file(&format!("{stem}.raw"));
        let datasets = if raw.exists() {
            read_raw_limited(&raw)?
        } else {
            Vec::new()
        };
        let files = ws.keep(
            &job.run_dir,
            &[
                format!("{stem}.scs"),
                format!("{stem}.raw"),
                format!("{stem}.log"),
                format!("{stem}.measure"),
            ],
        )?;
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
            duration: run.elapsed,
            raw_path,
            files,
        };
        finish(result, run.success(), "Spectre wrote no results")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(v: &[OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn plan_builds_argument_vectors() {
        let mut cfg = SpectreConfig::new("me@eda.example.edu", "/home/me/aispice-runs");
        cfg.command = "tcsh -f -c 'source /opt/cadence/setup.csh; cd {dir} && spectre {deck} -format nutascii -raw {raw} =log {log}'".into();
        let sp = Spectre::new(cfg);
        let p = sp
            .plan("rc low-pass", "1a2b", Path::new("/tmp/run"))
            .unwrap();
        let dir = "/home/me/aispice-runs/aispice-rc_low-pass-1a2b";
        assert_eq!(p.remote_dir, dir);
        assert_eq!(
            strs(&p.mkdir),
            vec![
                "ssh",
                "-o",
                "BatchMode=yes",
                "--",
                "me@eda.example.edu",
                &format!(
                    "umask 077 && mkdir -p {} && mkdir -m 700 {dir}",
                    dir.rsplit_once('/').unwrap().0
                )
            ]
        );
        assert_eq!(
            strs(&p.upload),
            vec![
                "scp",
                "-q",
                "-o",
                "BatchMode=yes",
                "--",
                // The local side uses the platform's separator.
                &Path::new("/tmp/run")
                    .join("rc_low-pass.scs")
                    .display()
                    .to_string(),
                &format!("me@eda.example.edu:{dir}/rc_low-pass.scs"),
            ]
        );
        assert_eq!(
            strs(&p.run).last().unwrap(),
            &format!(
                "tcsh -f -c 'source /opt/cadence/setup.csh; cd {dir} && spectre rc_low-pass.scs -format nutascii -raw rc_low-pass.raw =log rc_low-pass.log'"
            )
        );
        assert_eq!(strs(&p.run).len(), 6, "one remote command string");
        assert_eq!(
            strs(&p.download),
            vec![
                "scp",
                "-q",
                "-r",
                "-o",
                "BatchMode=yes",
                "--",
                &format!("me@eda.example.edu:{dir}/rc_low-pass.raw"),
                &format!("me@eda.example.edu:{dir}/rc_low-pass.log"),
                &format!("me@eda.example.edu:{dir}/rc_low-pass.measure"),
                "/tmp/run/",
            ]
        );
        assert_eq!(strs(&p.cleanup).last().unwrap(), &format!("rm -rf {dir}"));
    }

    #[test]
    fn default_command_template() {
        let sp = Spectre::new(SpectreConfig::new("host", "runs"));
        let p = sp.plan("d", "n", Path::new("/tmp/x")).unwrap();
        assert_eq!(
            strs(&p.run).last().unwrap(),
            "cd runs/aispice-d-n && spectre d.scs -format nutascii -raw d.raw =log d.log"
        );
    }

    #[test]
    fn hostile_configuration_is_refused() {
        for (host, dir) in [
            ("-oProxyCommand=evil", "runs"),
            ("host; rm -rf /", "runs"),
            ("host", "runs; rm -rf ~"),
            ("host", "../../etc"),
            ("host", "~/runs"),
            ("host", "-rf"),
            ("", "runs"),
        ] {
            let sp = Spectre::new(SpectreConfig::new(host, dir));
            assert!(
                matches!(
                    sp.plan("d", "n", Path::new("/tmp")),
                    Err(SimError::Config(_))
                ),
                "{host} {dir}"
            );
        }
        let mut no_deck = SpectreConfig::new("host", "runs");
        no_deck.command = "spectre".into();
        assert!(
            Spectre::new(no_deck)
                .plan("d", "n", Path::new("/tmp"))
                .is_err()
        );
        assert!(
            Spectre::default()
                .plan("d", "n", Path::new("/tmp"))
                .is_err()
        );
    }

    #[test]
    fn deck_names_cannot_inject() {
        let sp = Spectre::new(SpectreConfig::new("host", "runs"));
        let p = sp
            .plan("x; rm -rf / #", "n'$(id)", Path::new("/tmp"))
            .unwrap();
        let cmd = strs(&p.run).last().unwrap().clone();
        assert!(
            !cmd.contains(';') && !cmd.contains('$') && !cmd.contains('\''),
            "{cmd}"
        );
    }

    #[tokio::test]
    async fn unconfigured_detects_as_missing() {
        let d = Spectre::default().detect().await;
        assert!(!d.found);
        assert!(d.notes[0].contains("SSH host"));
    }
}

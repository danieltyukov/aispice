use super::*;

#[cfg(unix)]
fn plan_in(dir: &Path, program: &Path, profile: Profile) -> Plan {
    Plan {
        program: program.to_path_buf(),
        args: Vec::new(),
        env: vec![("HOME".into(), dir.into())],
        cwd: dir.to_path_buf(),
        read: Vec::new(),
        exec: Vec::new(),
        write: vec![dir.to_path_buf()],
        profile,
        limits: Limits {
            file_size: Some(64 << 20),
            address_space: Some(4 << 30),
            nproc_headroom: Some(128),
        },
    }
}

#[cfg(unix)]
fn strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn landlock_abis_are_floored_and_capped() {
    assert!(landlock_abi_from(0).is_err());
    assert!(landlock_abi_from(3).is_err());
    assert_eq!(landlock_abi_from(4), Ok(4));
    assert_eq!(landlock_abi_from(5), Ok(5));
    assert_eq!(landlock_abi_from(6), Ok(6));
    assert_eq!(landlock_abi_from(8), Ok(6));
}

#[test]
fn bubblewrap_versions_parse() {
    assert_eq!(parse_bwrap_version("bubblewrap 0.9.0\n"), Some((0, 9, 0)));
    assert_eq!(parse_bwrap_version("bubblewrap 0.11.1"), Some((0, 11, 1)));
    assert_eq!(parse_bwrap_version("bubblewrap 0.6"), Some((0, 6, 0)));
    assert_eq!(parse_bwrap_version("nonsense"), None);
    assert!(parse_bwrap_version("bubblewrap 0.8.0").unwrap() >= MIN_BWRAP);
    assert!(parse_bwrap_version("bubblewrap 0.7.0").unwrap() < MIN_BWRAP);
}

/// Under bubblewrap: new namespaces with no user namespace inside, only
/// the plan's environment, the system and the plan's paths read-only at
/// their own places, its folders writable, the seccomp programs as
/// descriptors, the program last.
#[cfg(unix)]
#[test]
fn the_bubblewrap_command_shows_only_the_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let data = dir.join("data");
    let out = dir.join("out");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let mut plan = plan_in(&out, Path::new("/usr/bin/true"), Profile::NativeCode);
    plan.read.push(data.clone());
    plan.args = vec!["--flag".into()];
    let sandbox = Sandbox::bwrap_for_tests(PathBuf::from("/usr/bin/bwrap"));
    let spec = sandbox.command(&plan).unwrap();
    assert_eq!(spec.program, PathBuf::from("/usr/bin/bwrap"));
    assert!(spec.clear_env && spec.env.is_empty());
    let confine = spec.confine.as_ref().unwrap();
    assert!(confine.landlock().is_none());
    assert!(confine.seccomp_as_fds());
    assert_eq!(confine.seccomp().unwrap().profile, Profile::NativeCode);
    assert_eq!(confine.limits(), &plan.limits);
    let args = strings(&spec.args);
    let has = |w: &[&str]| args.windows(w.len()).any(|x| x == w);
    for flag in [
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--assert-userns-disabled",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
    ] {
        assert!(args.contains(&flag.to_string()), "{flag}: {args:?}");
    }
    let s = |p: &Path| p.display().to_string();
    assert!(has(&["--ro-bind", "/usr", "/usr"]));
    assert!(has(&["--ro-bind", &s(&data), &s(&data)]));
    assert!(has(&["--bind", &s(&out), &s(&out)]));
    assert!(has(&["--chdir", &s(&out)]));
    assert!(has(&["--setenv", "HOME", &s(&out)]));
    assert!(has(&["--tmpfs", "/tmp"]));
    // The only writable bind is the plan's folder; the user's home and the
    // working folder are not shown.
    assert_eq!(args.iter().filter(|a| *a == "--bind").count(), 1);
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy().into_owned();
        assert!(!has(&["--ro-bind", &home, &home]));
    }
    let fds: Vec<&String> = args
        .windows(2)
        .filter(|w| w[0] == "--add-seccomp-fd")
        .map(|w| &w[1])
        .collect();
    let want: Vec<String> = (0..policy::PROFILE_PROGRAMS)
        .map(|i| (3 + i).to_string())
        .collect();
    assert_eq!(fds, want.iter().collect::<Vec<_>>());
    let tail = args.iter().position(|a| a == "--").unwrap();
    assert_eq!(args[tail + 1..], ["/usr/bin/true", "--flag"]);
}

/// Under Landlock: the program runs directly with the plan's environment;
/// it may read the system and the plan's paths, execute only itself, its
/// helpers and the loader, write only the plan's folders. At ABI 6 the
/// scopes hold signals and sockets; below, seccomp's signal program does.
#[cfg(unix)]
#[test]
fn the_landlock_command_confines_to_the_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let mut plan = plan_in(&dir, Path::new("/usr/bin/true"), Profile::Denylist);
    plan.exec.push(PathBuf::from("/bin/sh"));
    for (abi, scopes, signals) in [
        (6, Scopes::ALL, Signals::Unrestricted),
        (5, Scopes::default(), Signals::SelfOnly),
        (4, Scopes::default(), Signals::SelfOnly),
    ] {
        let spec = Sandbox::landlock_for_tests(abi).command(&plan).unwrap();
        assert_eq!(spec.program, PathBuf::from("/usr/bin/true"));
        assert!(spec.clear_env);
        assert_eq!(spec.env, plan.env);
        let c = spec.confine.as_ref().unwrap();
        assert!(!c.seccomp_as_fds());
        let ll = c.landlock().unwrap();
        assert_eq!((ll.min_abi, ll.scopes), (abi, scopes));
        assert_eq!(
            c.seccomp(),
            Some(&SeccompPolicy {
                profile: Profile::Denylist,
                signals
            })
        );
        assert!(ll.read.contains(&PathBuf::from("/usr")));
        assert_eq!(ll.write, vec![dir.clone()]);
        assert_eq!(ll.rw, vec![PathBuf::from("/dev/null")]);
        assert!(ll.exec.contains(&PathBuf::from("/usr/bin/true")));
        assert!(
            ll.exec
                .contains(&Path::new("/bin/sh").canonicalize().unwrap())
        );
        assert!(
            !ll.exec
                .iter()
                .any(|p| p == Path::new("/usr/bin") || p == Path::new("/usr"))
        );
    }
}

/// Write `script` as the program in `dir` and run it in `sandbox`, its
/// shell allowed; returns the file `result` it leaves in `dir`, if any.
#[cfg(unix)]
async fn run_script(sandbox: &Sandbox, dir: &Path, script: &str, profile: Profile) -> String {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    let out = dir.join("out");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let program = bin.join("stand-in");
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut plan = plan_in(&out, &program, profile);
    plan.exec.push(PathBuf::from("/bin/sh"));
    let spec = sandbox.command(&plan).unwrap();
    let run = process::run(&spec, Duration::from_secs(30), &CancellationToken::new())
        .await
        .unwrap();
    std::fs::read_to_string(out.join("result"))
        .unwrap_or_else(|_| panic!("{}: no result: {}", sandbox.describe(), run.combined()))
}

/// In every sandbox this machine has, a stand-in writes only its own
/// folder: it cannot create a file beside it, in the user's home or in the
/// real /tmp, or read a file it was not given. Each attempt is benign and
/// records nothing on success, so the result staying exactly `ok` is the
/// proof that none worked.
#[cfg(unix)]
#[tokio::test]
async fn every_sandbox_holds() {
    for sandbox in every_available().await {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let decoy = dir.join("decoy");
        std::fs::write(&decoy, "SECRET\n").unwrap();
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()));
        let tag = format!("aispice-sandbox-{}", std::process::id());
        let marks = [
            dir.join(&tag),
            home.join(&tag),
            PathBuf::from("/tmp").join(&tag),
        ];
        let mut script = String::from("#!/bin/sh\nprintf 'ok\\n' > result\n");
        for m in &marks {
            script.push_str(&format!("touch '{}' 2>/dev/null\n", m.display()));
        }
        script.push_str(&format!(
            "cat '{}' >> result 2>/dev/null\n",
            decoy.display()
        ));
        let text = run_script(&sandbox, &dir, &script, Profile::Denylist).await;
        let escaped: Vec<&PathBuf> = marks.iter().filter(|m| m.exists()).collect();
        for m in &escaped {
            let _ = std::fs::remove_file(m);
        }
        assert!(
            escaped.is_empty(),
            "{}: wrote outside: {escaped:?}",
            sandbox.describe()
        );
        assert_eq!(
            text,
            "ok\n",
            "{}: let something through",
            sandbox.describe()
        );
    }
}

/// Under Landlock (at ABI 6 through its scope, below it through seccomp's
/// signal program), a stand-in cannot execute a binary it was not given or
/// signal a process outside the sandbox. bubblewrap holds these with its
/// namespaces instead, and shows all of /usr, so this is Landlock only.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn landlock_narrows_exec_and_signals() {
    for sandbox in every_available().await {
        if sandbox.is_bwrap() {
            continue;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let script = format!(
            "#!/bin/sh\nprintf 'ok\\n' > result\n\
             /usr/bin/env >> result 2>/dev/null\n\
             kill -0 {} 2>/dev/null && printf 'signalled\\n' >> result\n",
            std::process::id()
        );
        for profile in [Profile::Denylist, Profile::NativeCode] {
            let text = run_script(&sandbox, &dir, &script, profile).await;
            assert_eq!(
                text,
                "ok\n",
                "{} {profile:?}: exec or signal was not denied",
                sandbox.describe()
            );
        }
    }
}

/// A `tmpfile()` that makes its unnamed file in `TMPDIR`. glibc's always
/// uses `/tmp`, which no sandbox grants under Landlock, and ngspice calls it
/// at start in every mode, so a caller has to preload one like this.
#[cfg(unix)]
const TMPFILE_SHIM: &str = r#"#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
FILE *tmpfile(void) {
    const char *dir = getenv("TMPDIR");
    int fd = open(dir ? dir : ".", O_TMPFILE | O_RDWR | O_EXCL | O_CLOEXEC, 0600);
    if (fd < 0) return NULL;
    FILE *f = fdopen(fd, "w+");
    if (!f) close(fd);
    return f;
}
FILE *tmpfile64(void) { return tmpfile(); }
"#;

/// [`TMPFILE_SHIM`] built into `dir` with the system's C compiler, or None
/// when there is none.
#[cfg(unix)]
fn tmpfile_shim(dir: &Path) -> Option<PathBuf> {
    let cc = which("cc")?;
    let src = dir.join("tmpfile.c");
    let lib = dir.join("libtmpfile.so");
    std::fs::write(&src, TMPFILE_SHIM).ok()?;
    let built = std::process::Command::new(cc)
        .args(["-shared", "-fPIC", "-O2", "-o"])
        .arg(&lib)
        .arg(&src)
        .status()
        .ok()?;
    built.success().then_some(lib)
}

/// ngspice, the untrusted-native-code case, runs under the allowlist in
/// every sandbox here: a plain deck, and with `AISPICE_PDK_IHP` an IHP deck
/// that loads the PSP model as OSDI. It gets [`TMPFILE_SHIM`] and no access
/// to `/tmp`. Gated on ngspice; under Landlock also on a C compiler.
#[cfg(unix)]
#[tokio::test]
async fn ngspice_runs_under_the_native_code_profile() {
    let Some(ngspice) = which("ngspice") else {
        eprintln!("skipped: ngspice is not installed");
        return;
    };
    let pdk = std::env::var_os("AISPICE_PDK_IHP")
        .map(PathBuf::from)
        .filter(|p| p.join("libs.tech/ngspice/osdi/psp103.osdi").is_file());
    let shim_dir = tempfile::tempdir().unwrap();
    let shim = tmpfile_shim(&shim_dir.path().canonicalize().unwrap());
    for sandbox in every_available().await {
        if shim.is_none() && !sandbox.is_bwrap() {
            eprintln!(
                "skipped under {}: no C compiler for the tmpfile() shim",
                sandbox.describe()
            );
            continue;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let mut decks = vec![(
            "rc",
            "rc\nV1 in 0 SINE(0 1 1k) AC 1\nR1 in out 1k\nC1 out 0 100n\n.tran 10u 2m\n.ac dec 10 1 1meg\n.end\n"
                .to_string(),
        )];
        let mut read = Vec::new();
        if let Some(pdk) = &pdk {
            let tech = pdk.join("libs.tech/ngspice").canonicalize().unwrap();
            std::fs::write(
                dir.join(".spiceinit"),
                format!("osdi {}\n", tech.join("osdi/psp103.osdi").display()),
            )
            .unwrap();
            decks.push((
                "nmos",
                format!(
                    "nmos\n.lib {} mos_tt\nVg g 0 0.8\nVd d 0 1.2\nXM1 d g 0 0 sg13_lv_nmos w=1u l=0.13u ng=1 m=1\n.op\n.dc Vg 0 1.2 0.05\n.end\n",
                    tech.join("models/cornerMOSlv.lib").display()
                ),
            ));
            read.push(tech);
        }
        for (name, deck) in decks {
            let cir = dir.join(format!("{name}.cir"));
            let raw = dir.join(format!("{name}.raw"));
            std::fs::write(&cir, deck).unwrap();
            let mut plan = plan_in(&dir, &ngspice, Profile::NativeCode);
            plan.args = vec!["-b".into(), "-r".into(), raw.clone().into(), cir.into()];
            plan.read = read.clone();
            plan.env.push(("TMPDIR".into(), dir.clone().into()));
            if let Some(lib) = &shim {
                plan.read.push(lib.clone());
                plan.env.push(("LD_PRELOAD".into(), lib.clone().into()));
            }
            let spec = sandbox.command(&plan).unwrap();
            let out = process::run(&spec, Duration::from_secs(60), &CancellationToken::new())
                .await
                .unwrap();
            assert!(
                out.success() && raw.is_file(),
                "{} {name}: {}",
                sandbox.describe(),
                out.combined()
            );
            assert!(
                !out.combined().to_ascii_lowercase().contains("error"),
                "{} {name}: {}",
                sandbox.describe(),
                out.combined()
            );
        }
    }
}

/// `select` picks a sandbox here when one is available, and it starts.
#[tokio::test]
async fn select_picks_one_that_starts() {
    let cancel = CancellationToken::new();
    match Sandbox::select(&cancel).await {
        Ok(s) => s.probe(&cancel).await.unwrap(),
        Err(e) => eprintln!("note: {e}"),
    }
}

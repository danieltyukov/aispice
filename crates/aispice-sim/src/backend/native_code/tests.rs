use super::*;
#[cfg(target_os = "linux")]
use crate::sandbox::{Scopes, SeccompPolicy, Signals};

#[test]
fn a_refusal_says_what_is_missing_and_what_to_do_instead() {
    let without = "use a deck that does not instantiate the models declared under `veriloga` and `[[cosim]]` in aispice.toml";
    for why in [
        "this system is not Linux",
        "the kernel has no seccomp filters",
        "no sandbox is available here: bubblewrap (bwrap) is not installed; the kernel's Landlock ABI 3 is below 4",
    ] {
        let m = refusal(Some(why), false);
        assert!(
            m.starts_with("the project's Verilog-A and co-simulated models were not loaded: "),
            "{m}"
        );
        assert!(m.contains(why), "{m}");
        assert!(
            m.contains("Linux with seccomp and either bubblewrap 0.8 or later or Landlock ABI 4"),
            "{m}"
        );
        assert!(m.contains(without), "{m}");
        assert!(!m.contains("C compiler"), "{m}");
    }
    let m = refusal(None, true);
    assert!(
        m.contains("there is no C compiler (cc, gcc, clang in /usr/local/bin, /usr/bin, /bin)"),
        "{m}"
    );
    assert!(m.contains("Install a C compiler (gcc or clang), or"), "{m}");
    assert!(m.contains(without), "{m}");
    assert!(!m.contains("sandboxed ngspice, which needs"), "{m}");
    let m = refusal(Some("this system is not Linux"), true);
    assert!(
        m.contains("this system is not Linux") && m.contains("no C compiler"),
        "{m}"
    );
}

fn fake_ngspice(dir: &Path) -> PathBuf {
    let bin = dir.join("opt/ngspice/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(dir.join("opt/ngspice/share/ngspice/scripts")).unwrap();
    std::fs::create_dir_all(dir.join("opt/ngspice/lib/ngspice")).unwrap();
    let exe = bin.join("ngspice");
    std::fs::write(&exe, b"").unwrap();
    exe
}

fn real(p: &Path) -> PathBuf {
    p.canonicalize().unwrap()
}

#[test]
fn ngspice_writes_only_its_run_folder_and_runs_under_the_native_code_profile() {
    let dir = tempfile::tempdir().unwrap();
    let exe = fake_ngspice(dir.path());
    let run = dir.path().join("run");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let gone = dir.path().join("no-such-folder");
    let p = plan(&exe, &run, &[project.clone(), gone], vec!["-b".into()]).unwrap();
    assert_eq!(p.program, real(&exe));
    assert_eq!(p.args, vec![OsString::from("-b")]);
    assert_eq!(p.write, vec![real(&run)]);
    assert_eq!(p.cwd, real(&run));
    // The sandbox adds ngspice itself and the loader; nothing else runs.
    assert!(p.exec.is_empty());
    assert_eq!(p.profile, Profile::NativeCode);
    assert_eq!(p.env, environment(&real(&run)));
    assert!(p.read.contains(&real(&project)));
    // Its install's scripts and code models, never the prefix itself.
    let prefix = real(&dir.path().join("opt/ngspice"));
    assert!(p.read.contains(&prefix.join("share/ngspice")));
    assert!(p.read.contains(&prefix.join("lib/ngspice")));
    assert!(!p.read.contains(&prefix));
    assert!(!p.read.iter().any(|r| r.ends_with("no-such-folder")));
    assert_eq!(p.limits.file_size, Some(MAX_FILE));
    assert_eq!(p.limits.address_space, Some(MAX_MEMORY));
    assert!(p.limits.nproc_headroom.is_some());
}

#[cfg(unix)]
#[test]
fn a_run_folder_ld_preload_would_split_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let exe = fake_ngspice(dir.path());
    for bad in ["with space", "with:colon"] {
        let run = dir.path().join(bad);
        std::fs::create_dir(&run).unwrap();
        let e = plan(&exe, &run, &[], Vec::new()).unwrap_err();
        assert!(e.to_string().contains("space or a colon"), "{e}");
    }
}

#[test]
fn a_sandboxed_run_starts_from_a_fixed_environment() {
    let env = environment(Path::new("/tmp/run"));
    let keys: Vec<_> = env.iter().map(|(k, _)| k.to_str().unwrap()).collect();
    assert_eq!(
        keys,
        ["PATH", "HOME", "PWD", "TMPDIR", "LC_ALL", "LD_PRELOAD"]
    );
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(get("HOME"), Some("/tmp/run".into()));
    assert_eq!(get("PWD"), Some("/tmp/run".into()));
    assert_eq!(get("TMPDIR"), Some("/tmp/run".into()));
    assert_eq!(
        get("LD_PRELOAD"),
        Some("/tmp/run/aispice_tmpfile.so".into())
    );
}

/// Whatever the sandbox, the command starts ngspice from the plan's
/// environment alone, so `LD_PRELOAD` and everything else come from
/// [`environment`] and nothing of aispice's own. Under Landlock the
/// ruleset writes only the run folder at the sandbox's ABI, and seccomp's
/// native-code profile keeps signals to ngspice itself at every ABI.
#[cfg(target_os = "linux")]
#[test]
fn every_sandbox_starts_ngspice_from_the_fixed_environment() {
    let dir = tempfile::tempdir().unwrap();
    let exe = fake_ngspice(dir.path());
    let run = dir.path().join("run");
    std::fs::create_dir(&run).unwrap();
    let p = plan(&exe, &run, &[], vec!["-b".into(), "deck.cir".into()]).unwrap();
    for abi in [4, 5, 6] {
        let spec = Sandbox::landlock_for_tests(abi).command(&p).unwrap();
        assert!(spec.clear_env);
        assert_eq!(spec.env, p.env);
        let c = spec.confine.unwrap();
        let ll = c.landlock().unwrap();
        assert_eq!(ll.write, p.write);
        assert_eq!(ll.rw, vec![PathBuf::from("/dev/null")]);
        assert_eq!(ll.min_abi, abi);
        let scopes = if abi >= 6 {
            Scopes::ALL
        } else {
            Scopes::default()
        };
        assert_eq!(ll.scopes, scopes);
        assert_eq!(
            c.seccomp(),
            Some(&SeccompPolicy {
                profile: Profile::NativeCode,
                signals: Signals::SelfOnly,
            }),
            "ABI {abi}"
        );
        assert_eq!(c.limits(), &p.limits);
    }
    let spec = Sandbox::bwrap_for_tests(PathBuf::from("/usr/bin/bwrap"))
        .command(&p)
        .unwrap();
    assert!(spec.clear_env);
    let args: Vec<String> = spec
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(args.contains(&"--clearenv".to_string()));
    let set: Vec<(String, String)> = args
        .windows(3)
        .filter(|w| w[0] == "--setenv")
        .map(|w| (w[1].clone(), w[2].clone()))
        .collect();
    let want: Vec<(String, String)> = p
        .env
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    assert_eq!(set, want);
}

/// bubblewrap shows the files a deck reads at their resolved paths only,
/// so an include through a link outside a folder it shows whole is refused
/// before ngspice runs, naming the include, the link and the fix. Landlock
/// grants the file and ngspice follows the link to it.
#[cfg(unix)]
#[test]
fn bubblewrap_refuses_an_include_through_a_link_and_names_it() {
    let d = tempfile::tempdir().unwrap();
    let p = real(d.path());
    std::fs::create_dir_all(p.join("models/sub")).unwrap();
    std::fs::create_dir_all(p.join("shared")).unwrap();
    std::fs::write(p.join("shared/vref.inc"), "V2 b 0 2\n").unwrap();
    std::os::unix::fs::symlink("../../shared/vref.inc", p.join("models/sub/link.inc")).unwrap();
    std::os::unix::fs::symlink("../shared", p.join("models/linked")).unwrap();
    let linked = |include: &str, path: &str| LinkedInclude {
        include: include.into(),
        path: p.join(path),
        file: p.join("shared/vref.inc"),
    };
    let file_link = [linked("sub/link.inc", "models/sub/link.inc")];
    let bwrap = Sandbox::bwrap_for_tests(PathBuf::from("/usr/bin/bwrap"));
    let e = check_links(&bwrap, &file_link, &[]).unwrap_err();
    for part in [
        "the include `sub/link.inc`".to_string(),
        format!(
            "through the symbolic link {}",
            p.join("models/sub/link.inc").display()
        ),
        format!(
            "Include {} itself, or replace the link with the file.",
            p.join("shared/vref.inc").display()
        ),
    ] {
        assert!(e.contains(&part), "{e}");
    }
    // A linked folder on the way is the link named.
    let folder_link = [linked("linked/vref.inc", "models/linked/vref.inc")];
    let e = check_links(&bwrap, &folder_link, &[]).unwrap_err();
    assert!(
        e.contains(&format!(
            "through the symbolic link {},",
            p.join("models/linked").display()
        )),
        "{e}"
    );
    // A link inside a folder bubblewrap shows whole is there to follow.
    assert!(check_links(&bwrap, &file_link, &[p.join("models")]).is_ok());
    for abi in [4, 6] {
        let landlock = Sandbox::landlock_for_tests(abi);
        assert!(check_links(&landlock, &file_link, &[]).is_ok());
        assert!(check_links(&landlock, &folder_link, &[]).is_ok());
    }
}

#[test]
fn the_cache_key_follows_the_source_the_compiler_and_its_version() {
    let a = cache_key(Path::new("/nonexistent/a/cc"), "cc (GCC) 13.2.0");
    assert_eq!(
        a,
        cache_key(Path::new("/nonexistent/a/cc"), "cc (GCC) 13.2.0")
    );
    assert_ne!(
        a,
        cache_key(Path::new("/nonexistent/a/cc"), "cc (GCC) 14.1.0")
    );
    assert_ne!(
        a,
        cache_key(Path::new("/nonexistent/b/cc"), "cc (GCC) 13.2.0")
    );
    assert_eq!(a.len(), 32);
}

#[test]
fn the_compiler_comes_from_the_system_folders_with_fixed_flags_and_environment() {
    if let Some(cc) = compiler() {
        let dir = cc.parent().unwrap();
        assert!(TOOL_PATH.iter().any(|d| Path::new(d) == dir), "{cc:?}");
    }
    let spec = compiler_spec(
        Path::new("/usr/bin/cc"),
        Path::new("/tmp/build"),
        COMPILER_ARGS,
    );
    assert!(spec.clear_env);
    assert_eq!(
        spec.args,
        [
            "-O2",
            "-fPIC",
            "-shared",
            "-o",
            "aispice_tmpfile.so",
            "tmpfile.c"
        ]
        .map(OsString::from)
        .to_vec()
    );
    let env: Vec<(String, String)> = spec
        .env
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    assert_eq!(
        env,
        [
            ("PATH", "/usr/local/bin:/usr/bin:/bin"),
            ("TMPDIR", "/tmp/build"),
            ("LC_ALL", "C"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .to_vec()
    );
}

#[cfg(unix)]
#[test]
fn a_placed_library_must_hold_the_bytes_recorded_when_it_was_built() {
    let dir = tempfile::tempdir().unwrap();
    let good = Preload {
        bytes: b"lib".to_vec().into(),
        sha256: sha256_hex(b"lib"),
    };
    let p = place_preload(dir.path(), &good).unwrap();
    assert_eq!(p, dir.path().join(PRELOAD_FILE));
    assert_eq!(std::fs::read(&p).unwrap(), b"lib");
    // Nothing already at the name is written through, a link included.
    assert!(place_preload(dir.path(), &good).is_err());
    let other = tempfile::tempdir().unwrap();
    let bad = Preload {
        bytes: b"lib".to_vec().into(),
        sha256: sha256_hex(b"something else"),
    };
    let e = place_preload(other.path(), &bad).unwrap_err();
    assert!(e.to_string().contains("does not match the SHA-256"), "{e}");
}

/// The cache: a build is stored with its SHA-256; a cached copy is used
/// only while it has that SHA-256 and is a plain file not every user may
/// write, and is rebuilt otherwise.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_preloaded_library_is_cached_with_its_sha256_and_rebuilt_when_it_changes() {
    use std::os::unix::fs::PermissionsExt;
    let Some(cc) = compiler() else {
        eprintln!("skipped: no C compiler");
        return;
    };
    let cache = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let built = load_or_build(Some(cache.path()), &cancel).await.unwrap();
    assert_eq!(built.sha256, sha256_hex(&built.bytes));
    assert!(built.bytes.starts_with(b"\x7fELF"));
    let key = cache_key(&cc, &compiler_version(&cc, &cancel).await.unwrap());
    let so = cache.path().join(format!("{key}.so"));
    let record = cache.path().join(format!("{key}.sha256"));
    assert_eq!(std::fs::read(&so).unwrap(), &built.bytes[..]);
    assert_eq!(std::fs::read_to_string(&record).unwrap(), built.sha256);

    // A copy that matches its record is what is used.
    std::fs::write(&so, b"stand-in").unwrap();
    std::fs::write(&record, sha256_hex(b"stand-in")).unwrap();
    let cached = load_or_build(Some(cache.path()), &cancel).await.unwrap();
    assert_eq!(&cached.bytes[..], b"stand-in");

    // One that does not, is a link, or others may write is rebuilt, and
    // the cache holds the new build.
    let decoy = cache.path().join("decoy");
    std::fs::write(&decoy, b"stand-in").unwrap();
    let tampered: [&dyn Fn(); 3] = [
        &|| std::fs::write(&so, b"changed").unwrap(),
        &|| {
            std::fs::remove_file(&so).unwrap();
            std::os::unix::fs::symlink(&decoy, &so).unwrap();
        },
        &|| {
            std::fs::remove_file(&so).unwrap();
            std::fs::write(&so, b"stand-in").unwrap();
            std::fs::set_permissions(&so, std::fs::Permissions::from_mode(0o666)).unwrap();
        },
    ];
    for (i, tamper) in tampered.iter().enumerate() {
        std::fs::write(&record, sha256_hex(b"stand-in")).unwrap();
        tamper();
        let p = load_or_build(Some(cache.path()), &cancel).await.unwrap();
        assert!(p.bytes.starts_with(b"\x7fELF"), "case {i}");
        assert_eq!(p.sha256, sha256_hex(&p.bytes), "case {i}");
        assert_eq!(
            std::fs::read_to_string(&record).unwrap(),
            p.sha256,
            "case {i}"
        );
        assert!(
            !std::fs::symlink_metadata(&so).unwrap().is_symlink(),
            "case {i}"
        );
    }
    assert_eq!(std::fs::read(&decoy).unwrap(), b"stand-in");
}

/// Calls `tmpfile()` and prints where the file is (`file <path>`) or
/// `null <errno>`. With `NO_O_TMPFILE` set its `open`, which the preloaded
/// library calls and glibc's own functions do not, refuses `O_TMPFILE` as a
/// filesystem without it does.
#[cfg(target_os = "linux")]
const HARNESS: &str = r#"#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

static int open_any(const char *path, int flags, va_list ap)
{
    mode_t mode = 0;
    if ((flags & O_CREAT) || (flags & O_TMPFILE) == O_TMPFILE)
        mode = va_arg(ap, mode_t);
    if ((flags & O_TMPFILE) == O_TMPFILE && getenv("NO_O_TMPFILE")) {
        errno = EOPNOTSUPP;
        return -1;
    }
    return syscall(SYS_openat, AT_FDCWD, path, flags, mode);
}

int open(const char *path, int flags, ...)
{
    va_list ap;
    va_start(ap, flags);
    int r = open_any(path, flags, ap);
    va_end(ap);
    return r;
}

int open64(const char *path, int flags, ...)
{
    va_list ap;
    va_start(ap, flags);
    int r = open_any(path, flags, ap);
    va_end(ap);
    return r;
}

int main(void)
{
    FILE *f = tmpfile();
    if (!f) {
        printf("null %d\n", errno);
        return 0;
    }
    char link[64], target[4096];
    snprintf(link, sizeof link, "/proc/self/fd/%d", fileno(f));
    ssize_t n = readlink(link, target, sizeof target - 1);
    if (n < 0) {
        printf("unknown\n");
        return 0;
    }
    target[n] = '\0';
    printf("file %s\n", target);
    return 0;
}
"#;

/// The preloaded `tmpfile()` makes its file in `TMPDIR` (unnamed, or named
/// and removed at once where `O_TMPFILE` is missing) and fails without an
/// absolute `TMPDIR` it can write, where glibc's own would use `/tmp`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_preloaded_tmpfile_uses_tmpdir_and_never_falls_back_to_tmp() {
    let Some(cc) = compiler() else {
        eprintln!("skipped: no C compiler");
        return;
    };
    let work = tempfile::tempdir().unwrap();
    let work = real(work.path());
    let preload = load_or_build(None, &CancellationToken::new())
        .await
        .unwrap();
    let shim = work.join(PRELOAD_FILE);
    std::fs::write(&shim, &preload.bytes).unwrap();
    std::fs::write(work.join("harness.c"), HARNESS).unwrap();
    let built = std::process::Command::new(&cc)
        .args([
            "-O0",
            "-U_FORTIFY_SOURCE",
            "-rdynamic",
            "-o",
            "harness",
            "harness.c",
        ])
        .current_dir(&work)
        .status()
        .unwrap();
    assert!(built.success(), "the harness did not compile");
    let run = |tmpdir: Option<&Path>, preload: bool, no_o_tmpfile: bool| {
        let mut cmd = std::process::Command::new(work.join("harness"));
        cmd.env_clear();
        if let Some(t) = tmpdir {
            cmd.env("TMPDIR", t);
        }
        if preload {
            cmd.env("LD_PRELOAD", &shim);
        }
        if no_o_tmpfile {
            cmd.env("NO_O_TMPFILE", "1");
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let dir = work.join("tmpdir");
    std::fs::create_dir(&dir).unwrap();
    let in_dir = |line: &str| {
        line.strip_prefix("file ")
            .is_some_and(|p| Path::new(p).parent() == Some(dir.as_path()))
    };

    // glibc's own ignores TMPDIR: this is what the library is for.
    let glibc = run(Some(&dir), false, false);
    assert!(
        glibc.starts_with("file /tmp/") && !in_dir(&glibc),
        "{glibc}"
    );

    let unnamed = run(Some(&dir), true, false);
    assert!(in_dir(&unnamed), "{unnamed}");
    let named = run(Some(&dir), true, true);
    assert!(
        in_dir(&named) && named.contains("/tmpf") && named.ends_with("(deleted)"),
        "{named}"
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

    // No TMPDIR, a relative one or one that does not exist: no file at all.
    let enoent = format!("null {}", libc::ENOENT);
    assert_eq!(run(None, true, false), enoent);
    assert_eq!(run(Some(Path::new("tmpdir")), true, false), enoent);
    let missing = work.join("missing");
    assert_eq!(run(Some(&missing), true, false), enoent);
    assert_eq!(run(Some(&missing), true, true), enoent);
}

#[cfg(target_os = "linux")]
mod sandboxed;

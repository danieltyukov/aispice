//! The test-only Landlock ABI override (feature `testing`). After
//! `Sandbox::use_landlock_abi_for_tests(4)`, `Sandbox::select` gives Landlock
//! at ABI 4 even where bubblewrap starts, and what runs there is under a
//! real ABI-4 ruleset with seccomp's signal program: a device ioctl gets
//! past Landlock (ABI 5 added that right) and a signal to every process is
//! refused. The override is process-wide, which is why this test has a
//! binary of its own.

#![cfg(target_os = "linux")]

use aispice_sim::backend::{process, which};
use aispice_sim::sandbox::{Limits, Plan, Profile, Sandbox};
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn select_gives_a_real_abi_4_ruleset() {
    for abi in [0, 3, 7] {
        assert!(
            Sandbox::use_landlock_abi_for_tests(abi).is_err(),
            "ABI {abi} accepted"
        );
    }
    let kernel = aispice_sim::backend::confine::abi();
    if kernel < 4 {
        assert!(Sandbox::use_landlock_abi_for_tests(4).is_err());
        eprintln!("skipped: the kernel's Landlock ABI is {kernel}");
        return;
    }
    let Some(stty) = which("stty") else {
        eprintln!("skipped: no stty");
        return;
    };
    Sandbox::use_landlock_abi_for_tests(4).unwrap();
    assert_eq!(aispice_sim::backend::confine::abi(), 4);
    let cancel = CancellationToken::new();
    let sandbox = Sandbox::select(&cancel).await.unwrap();
    assert!(!sandbox.is_bwrap(), "{}", sandbox.describe());
    assert_eq!(sandbox.landlock_abi(), Some(4));

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let program = dir.join("stand-in");
    std::fs::write(
        &program,
        "#!/bin/sh\nprintf 'ok\\n' > result\n\
         stty -F /dev/null 2>> result\n\
         kill -0 -1 2>/dev/null && printf 'broadcast\\n' >> result\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let plan = Plan {
        program: program.clone(),
        args: Vec::new(),
        env: vec![("HOME".into(), out.clone().into())],
        cwd: out.clone(),
        read: Vec::new(),
        exec: vec![PathBuf::from("/bin/sh"), stty],
        write: vec![out.clone()],
        profile: Profile::NativeCode,
        limits: Limits::default(),
    };
    let spec = sandbox.command(&plan).unwrap();
    let run = process::run(&spec, Duration::from_secs(30), &cancel)
        .await
        .unwrap();
    let text = std::fs::read_to_string(out.join("result"))
        .unwrap_or_else(|_| panic!("no result: {}", run.combined()));
    assert!(text.starts_with("ok\n"), "{text}");
    assert!(text.contains("Inappropriate ioctl"), "{text}");
    assert!(!text.contains("broadcast"), "{text}");
}

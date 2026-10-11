//! Confining a child process with Landlock before it execs (Linux only).
//!
//! aispice runs xschem inside bubblewrap where that can start. Where it
//! cannot (an unprivileged-user-namespace restriction without a bwrap
//! AppArmor profile, the default on Ubuntu 24.04), it falls back to
//! Landlock. Landlock (Linux 5.13 and later, ABI 4 and later for the TCP
//! rules used here) lets a process give up access it will not need before
//! it execs the tool: the tool may read and execute the system and xschem's
//! install, read the PDK and the input, write only a private output
//! directory with a private HOME and TMPDIR, and neither bind nor connect a
//! TCP socket. Everything else, the rest of the user's home included, is
//! denied, so nothing xschem's Tcl might do can touch it.
//!
//! The ruleset is built here, in the parent, where allocation is safe. The
//! child's `pre_exec` hook does nothing but a few syscalls: it sets the
//! resource limits, asks to die with its parent, turns on `no_new_privs`,
//! and enforces the ruleset the parent already built. A post-fork hook that
//! runs beside the async runtime's threads must not allocate or take a lock,
//! so the hook allocates nothing and the ruleset's file descriptor is the
//! only state it carries in.

#![allow(unsafe_code)]

use super::process::{Confinement, Landlock};
use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
    RulesetAttr, RulesetCreatedAttr,
};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};

/// The lowest Landlock ABI aispice will use. ABI 4 (Linux 6.7) is the first
/// with the TCP network rules, which the sandbox relies on to forbid every
/// bind and connect.
pub const MIN_ABI: i32 = 4;

/// The ABI the ruleset is written against. The crate downgrades it on an
/// older kernel (best effort); a newer kernel's extra access rights are not
/// handled, so they stay allowed, which only matters above what this sandbox
/// cares about (files and TCP).
const ABI_TARGET: ABI = ABI::V4;

/// Landlock's ABI version on this machine, or 0 when the kernel has no
/// Landlock. Read straight from the kernel with the version probe of
/// `landlock_create_ruleset`, since the crate does not expose a runtime
/// value on purpose.
pub fn abi() -> i32 {
    // landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION).
    const VERSION: u32 = 1;
    let r = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            VERSION,
        )
    };
    if r < 0 { 0 } else { r as i32 }
}

/// Build the ruleset for `ll` in this (parent) process and return its file
/// descriptor, without enforcing it here. Missing paths are skipped: a file
/// that is not there is nothing to allow.
fn build(ll: &Landlock) -> io::Result<OwnedFd> {
    let read_exec = AccessFs::from_read(ABI_TARGET) | AccessFs::Execute;
    let read = AccessFs::from_read(ABI_TARGET);
    let write = AccessFs::from_all(ABI_TARGET);
    let to_io = |e: landlock::RulesetError| io::Error::other(e.to_string());
    // Handle every file access and both TCP accesses, so that anything not
    // granted by a rule below is denied. No rule grants TCP, so no bind or
    // connect is possible. Best effort: on an older kernel the unsupported
    // bits are dropped rather than failing the run.
    let mut created = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(ABI_TARGET))
        .map_err(to_io)?
        .handle_access(AccessNet::from_all(ABI_TARGET))
        .map_err(to_io)?
        .create()
        .map_err(to_io)?;
    for (paths, rights) in [
        (&ll.read_exec, read_exec),
        (&ll.read, read),
        (&ll.write, write),
    ] {
        for p in paths {
            // A path gone since the plan was made is skipped, not an error.
            let Ok(fd) = PathFd::new(p) else { continue };
            created = created
                .add_rule(PathBeneath::new(fd, rights))
                .map_err(to_io)?;
        }
    }
    Option::<OwnedFd>::from(created)
        .ok_or_else(|| io::Error::other("this kernel has no usable Landlock support"))
}

/// Set one resource limit. A syscall only: safe to call in `pre_exec`.
unsafe fn setrlimit(resource: libc::__rlimit_resource_t, value: u64) -> io::Result<()> {
    let lim = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    if unsafe { libc::setrlimit(resource, &lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Confine `cmd`'s child: build the Landlock ruleset now, and install a
/// `pre_exec` hook that applies the limits and enforces the ruleset. The
/// hook makes no allocation and holds no lock, so it is safe to run after a
/// fork beside the runtime's threads.
pub fn apply(cmd: &mut tokio::process::Command, c: &Confinement) -> io::Result<()> {
    let ruleset = match &c.landlock {
        Some(ll) => Some(build(ll)?),
        None => None,
    };
    let fsize = c.rlimit_fsize;
    let as_limit = c.rlimit_as;
    // SAFETY: the hook runs in the forked child before exec. It only makes
    // syscalls (setrlimit, prctl, landlock_restrict_self) and reads the
    // captured ruleset fd and the two limit values; it allocates nothing and
    // takes no lock, so it cannot deadlock against a thread that held one at
    // fork. The ruleset fd was created in the parent and is valid in the
    // child; after exec it is closed on its own (close-on-exec).
    unsafe {
        cmd.pre_exec(move || {
            if let Some(v) = fsize {
                setrlimit(libc::RLIMIT_FSIZE, v)?;
            }
            if let Some(v) = as_limit {
                setrlimit(libc::RLIMIT_AS, v)?;
            }
            // Die if aispice dies, so no confined xschem is left behind.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(fd) = &ruleset {
                // no_new_privs first: Landlock requires it, and it stops the
                // child regaining privileges through a setuid program.
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::syscall(libc::SYS_landlock_restrict_self, fd.as_raw_fd(), 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

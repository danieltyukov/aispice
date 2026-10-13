//! Enforcing a [`Confinement`] on a child process before it execs (Linux
//! only): Landlock, seccomp, descriptors and resource limits.
//!
//! - **Landlock**: the ruleset is written against the policy's ABI (4 to
//!   6) at a hard requirement, so a kernel that cannot enforce every access
//!   handled is refused rather than used with a weaker sandbox. Everything
//!   the ABI knows is handled and only what the policy lists is granted:
//!   reading, executing, writing plain files and folders (no symlink,
//!   socket, fifo or device), no TCP, and from ABI 6 the scopes (no
//!   abstract UNIX socket or signal outside the domain). Every rule is
//!   built from a descriptor opened `O_PATH|O_NOFOLLOW`.
//! - **seccomp**: the profile's program ([`Profile::Denylist`] refuses the
//!   calls an escape would use with `EACCES`; [`Profile::NativeCode`]
//!   allows only what a computational program needs and answers the rest
//!   `ENOSYS`), a program answering `clone3` with `ENOSYS` (its flags sit
//!   in memory a filter cannot read, so the C library falls back to
//!   `clone`, whose flags the profile checks), and on x86_64 a program
//!   refusing every x32 call, which carries the x86_64 architecture and so
//!   matches no rule written for the 64-bit numbers. With
//!   [`Signals::SelfOnly`] a fourth program keeps signals to the process
//!   and its group; the process's pid is written into it in `pre_exec`.
//!   The filters are inherited across fork and exec.
//! - **Descriptors**: every descriptor above stderr is marked close-on-exec
//!   before the program starts, so nothing aispice holds open reaches it;
//!   Landlock does not restrict descriptors opened before it applies.
//!   Under bubblewrap the seccomp programs are handed over as descriptors
//!   3.. and only those survive.
//! - **Resource limits** cap the size of each file written, the address
//!   space and the number of processes (a margin over what the user runs
//!   now), and the process is asked to die with its parent. Nothing caps
//!   the total bytes written across many files in a writable folder.
//!
//! The ruleset and the programs are built here, in the parent, where
//! allocation is safe. The child's `pre_exec` hook does nothing but a few
//! syscalls over already-built state (and writes its pid into the signal
//! program in place), so it allocates nothing and is safe after a fork
//! beside the async runtime's threads. Landlock is applied before seccomp,
//! since the allowlist does not include Landlock's own calls.

#![allow(unsafe_code)]

use crate::sandbox::policy::{
    Confinement, LandlockPolicy, MAX_PROGRAMS, PROFILE_PROGRAMS, Profile, SeccompPolicy, Signals,
};
use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, PathBeneath, Ruleset,
    RulesetAttr, RulesetCreatedAttr, Scope,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, sock_filter,
};
use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Bit 30 of the system call number marks an x32 call on x86_64.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// The audit architecture the seccomp programs are written for.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 62 | 0x8000_0000 | 0x4000_0000;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 183 | 0x8000_0000 | 0x4000_0000;

// Classic BPF opcodes (linux/filter.h) and the offsets in struct
// seccomp_data: `nr` at 0, `arch` at 4, `args[i]` at 16 + 8 i (its low
// 32 bits first on the little-endian targets aispice builds for).
const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
const RET_K: u16 = 0x06; // BPF_RET | BPF_K
const NR: u32 = 0;
const ARCH: u32 = 4;
const fn arg_low(i: u32) -> u32 {
    16 + 8 * i
}
const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
const DENY: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;

// fcntl commands libc does not name (asm-generic/fcntl.h, the same on
// x86_64 and aarch64): have the kernel signal another process later.
const F_SETSIG: u32 = 10;
const F_SETOWN_EX: u32 = 15;

/// The pid of the program bubblewrap runs, in its PID namespace.
const BWRAP_PROGRAM_PID: libc::pid_t = 2;

/// The namespace flags `clone` must not carry.
const CLONE_NEW: [libc::c_int; 8] = [
    libc::CLONE_NEWNS,
    libc::CLONE_NEWUTS,
    libc::CLONE_NEWIPC,
    libc::CLONE_NEWUSER,
    libc::CLONE_NEWPID,
    libc::CLONE_NEWNET,
    libc::CLONE_NEWCGROUP,
    libc::CLONE_NEWTIME,
];

/// Landlock's ABI version on this machine, or 0 when the kernel has no
/// Landlock. A test build may lower it ([`override_abi_for_tests`]).
pub fn abi() -> i32 {
    let kernel = kernel_abi();
    #[cfg(feature = "testing")]
    if let Some(abi) = abi_override_for_tests() {
        return kernel.min(i32::from(abi));
    }
    kernel
}

/// Tests only (feature `testing`): the Landlock ABI [`abi`] reports in
/// place of the kernel's, or 0 for none.
#[cfg(feature = "testing")]
static ABI_FOR_TESTS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Tests only (feature `testing`, which only dev-dependencies enable): make
/// [`abi`] report `abi` for the rest of this process, so a test on a newer
/// kernel gets Landlock at that ABI and a real ruleset written against it.
/// Refused outside the ABIs aispice writes rulesets for or above the
/// kernel's own. No product build has this.
#[cfg(feature = "testing")]
pub(crate) fn override_abi_for_tests(abi: u8) -> Result<(), String> {
    use crate::sandbox::policy::{MAX_LANDLOCK_ABI, MIN_LANDLOCK_ABI};
    if !(MIN_LANDLOCK_ABI..=MAX_LANDLOCK_ABI).contains(&abi) {
        return Err(format!(
            "aispice writes Landlock rulesets for ABI {MIN_LANDLOCK_ABI} to {MAX_LANDLOCK_ABI}, not {abi}"
        ));
    }
    let kernel = kernel_abi();
    if kernel < i32::from(abi) {
        return Err(format!(
            "the kernel's Landlock ABI is {kernel}, below {abi}"
        ));
    }
    ABI_FOR_TESTS.store(abi, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// The ABI a test set with [`override_abi_for_tests`], if any.
#[cfg(feature = "testing")]
pub(crate) fn abi_override_for_tests() -> Option<u8> {
    match ABI_FOR_TESTS.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        abi => Some(abi),
    }
}

/// The kernel's own Landlock ABI, from the version probe of
/// `landlock_create_ruleset`, since the crate does not expose a runtime
/// value on purpose.
fn kernel_abi() -> i32 {
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

/// The crate's ABI for a policy's `min_abi`. The builder has already kept
/// it within the ABIs aispice writes rulesets for.
fn ruleset_abi(min_abi: u8) -> io::Result<ABI> {
    match min_abi {
        4 => Ok(ABI::V4),
        5 => Ok(ABI::V5),
        6 => Ok(ABI::V6),
        a => Err(io::Error::other(format!("no Landlock ruleset for ABI {a}"))),
    }
}

/// Open `p` as an `O_PATH|O_NOFOLLOW` descriptor and report whether it is a
/// directory. A symlink at the final component is refused: the caller gives
/// real paths, so a symlink here is something swapped in.
fn open_pinned(p: &Path) -> io::Result<(OwnedFd, bool)> {
    use std::os::fd::FromRawFd;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("a path holds a NUL"))?;
    // SAFETY: a valid C string and flags; the returned fd is owned below.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a valid fd and a stat buffer we own.
    if unsafe { libc::fstat(owned.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let kind = st.st_mode & libc::S_IFMT;
    if kind == libc::S_IFLNK {
        return Err(io::Error::other(format!(
            "{} is a symlink, which the sandbox will not open",
            p.display()
        )));
    }
    Ok((owned, kind == libc::S_IFDIR))
}

/// Add one path to the ruleset with `rights`, opened and pinned by
/// descriptor. Rights that do not apply to a plain file are dropped, and a
/// path that cannot be opened is skipped (nothing to allow).
fn add(
    created: landlock::RulesetCreated,
    p: &Path,
    rights: BitFlags<AccessFs>,
) -> io::Result<landlock::RulesetCreated> {
    let (fd, is_dir) = match open_pinned(p) {
        Ok(v) => v,
        // A root that is not installed (e.g. an optional /etc file) is not
        // an error; a symlink swap is, and open_pinned already refused it.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(created),
        Err(e) => return Err(e),
    };
    let file_rights =
        AccessFs::Execute | AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Truncate;
    let rights = if is_dir { rights } else { rights & file_rights };
    if rights.is_empty() {
        return Ok(created);
    }
    created
        .add_rule(PathBeneath::new(fd, rights))
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Build the Landlock ruleset for `ll` in this (parent) process and return
/// its descriptor, without enforcing it here.
fn build_landlock(ll: &LandlockPolicy) -> io::Result<OwnedFd> {
    let to_io = |e: landlock::RulesetError| io::Error::other(e.to_string());
    let abi = ruleset_abi(ll.min_abi)?;
    // Everything the ABI knows is handled, at a hard requirement: anything
    // not granted below is denied, and an older kernel fails here rather
    // than running weakly. No rule grants TCP, so no bind or connect.
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .map_err(to_io)?
        .handle_access(AccessNet::from_all(abi))
        .map_err(to_io)?;
    let mut scopes = BitFlags::<Scope>::empty();
    if ll.scopes.abstract_unix_socket {
        scopes |= Scope::AbstractUnixSocket;
    }
    if ll.scopes.signal {
        scopes |= Scope::Signal;
    }
    if !scopes.is_empty() {
        ruleset = ruleset.scope(scopes).map_err(to_io)?;
    }
    let mut created = ruleset.create().map_err(to_io)?;
    // Rights per category; `add` drops the directory-only bits for a plain
    // file, so one read set serves both files and directories.
    let read = AccessFs::ReadFile | AccessFs::ReadDir;
    let exec = AccessFs::Execute | AccessFs::ReadFile;
    // Write: plain files and folders only. No symlink, socket, fifo or
    // device may be made in the writable folders, so nothing there can
    // point a later reader elsewhere, and nothing written can be executed.
    let write = AccessFs::ReadFile
        | AccessFs::ReadDir
        | AccessFs::WriteFile
        | AccessFs::Truncate
        | AccessFs::MakeReg
        | AccessFs::MakeDir
        | AccessFs::RemoveFile
        | AccessFs::RemoveDir
        | AccessFs::Refer;
    let rw = AccessFs::ReadFile | AccessFs::WriteFile;
    for p in &ll.read {
        created = add(created, p, read)?;
    }
    for p in &ll.exec {
        created = add(created, p, exec)?;
    }
    for p in &ll.write {
        created = add(created, p, write)?;
    }
    for p in &ll.rw {
        created = add(created, p, rw)?;
    }
    Option::<OwnedFd>::from(created)
        .ok_or_else(|| io::Error::other("this kernel has no usable Landlock support"))
}

/// The syscalls [`Profile::Denylist`] refuses: sockets of every family and
/// anything done on one, the asynchronous interfaces that can open or
/// connect a socket without the socket syscall (io_uring), tracing or
/// reading another process, taking its descriptors, the keyring, BPF,
/// perf, userfaultfd, System V and POSIX inter-process communication
/// (another program of the same user, such as an X server's shared memory,
/// is reachable through it without an IPC namespace), namespaces and
/// mounts, and leaving the process group, which is how aispice finds every
/// process the run started.
fn denied_syscalls() -> Vec<libc::c_long> {
    vec![
        libc::SYS_socket,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_sendto,
        libc::SYS_sendmsg,
        libc::SYS_sendmmsg,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_process_madvise,
        libc::SYS_pidfd_getfd,
        libc::SYS_kcmp,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_shmget,
        libc::SYS_shmat,
        libc::SYS_shmctl,
        libc::SYS_shmdt,
        libc::SYS_msgget,
        libc::SYS_msgsnd,
        libc::SYS_msgrcv,
        libc::SYS_msgctl,
        libc::SYS_semget,
        libc::SYS_semop,
        libc::SYS_semtimedop,
        libc::SYS_semctl,
        libc::SYS_mq_open,
        libc::SYS_mq_unlink,
        libc::SYS_mq_timedsend,
        libc::SYS_mq_timedreceive,
        libc::SYS_mq_notify,
        libc::SYS_mq_getsetattr,
        libc::SYS_setsid,
        libc::SYS_setpgid,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_open_tree,
        libc::SYS_move_mount,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_fspick,
        libc::SYS_mount_setattr,
        libc::SYS_open_by_handle_at,
    ]
}

/// The syscalls [`Profile::NativeCode`] allows without looking at their
/// arguments: files (Landlock decides which), memory, threads, time,
/// signals (the signal program decides to whom), waiting for and starting
/// programs (Landlock decides which), and process queries. Taken from
/// tracing ngspice 42 with OSDI models, plus what the C library and thread
/// runtimes use on their common paths.
fn native_code_syscalls() -> Vec<libc::c_long> {
    let mut v = vec![
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_preadv,
        libc::SYS_pwritev,
        libc::SYS_preadv2,
        libc::SYS_pwritev2,
        libc::SYS_lseek,
        libc::SYS_close,
        libc::SYS_close_range,
        libc::SYS_openat,
        libc::SYS_openat2,
        libc::SYS_fstat,
        libc::SYS_newfstatat,
        libc::SYS_statx,
        libc::SYS_statfs,
        libc::SYS_fstatfs,
        libc::SYS_getdents64,
        libc::SYS_readlinkat,
        libc::SYS_faccessat,
        libc::SYS_faccessat2,
        libc::SYS_getcwd,
        libc::SYS_chdir,
        libc::SYS_fchdir,
        libc::SYS_mkdirat,
        libc::SYS_unlinkat,
        libc::SYS_renameat,
        libc::SYS_renameat2,
        libc::SYS_ftruncate,
        libc::SYS_fsync,
        libc::SYS_fdatasync,
        libc::SYS_flock,
        libc::SYS_umask,
        libc::SYS_dup,
        libc::SYS_dup3,
        libc::SYS_pipe2,
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_brk,
        libc::SYS_msync,
        libc::SYS_membarrier,
        libc::SYS_get_mempolicy,
        libc::SYS_futex,
        libc::SYS_set_robust_list,
        libc::SYS_get_robust_list,
        libc::SYS_set_tid_address,
        libc::SYS_rseq,
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_wait4,
        libc::SYS_waitid,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_kill,
        libc::SYS_tkill,
        libc::SYS_tgkill,
        libc::SYS_rt_sigqueueinfo,
        libc::SYS_rt_tgsigqueueinfo,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_rt_sigsuspend,
        libc::SYS_rt_sigtimedwait,
        libc::SYS_rt_sigpending,
        libc::SYS_sigaltstack,
        libc::SYS_restart_syscall,
        libc::SYS_setitimer,
        libc::SYS_getitimer,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_getppid,
        libc::SYS_getpgid,
        libc::SYS_getsid,
        libc::SYS_getuid,
        libc::SYS_geteuid,
        libc::SYS_getgid,
        libc::SYS_getegid,
        libc::SYS_getgroups,
        libc::SYS_getresuid,
        libc::SYS_getresgid,
        // Without CAP_SETUID these only report the current value.
        libc::SYS_setfsuid,
        libc::SYS_setfsgid,
        libc::SYS_uname,
        libc::SYS_sysinfo,
        libc::SYS_getrusage,
        libc::SYS_times,
        libc::SYS_getrlimit,
        libc::SYS_sched_getaffinity,
        libc::SYS_sched_yield,
        libc::SYS_getcpu,
        libc::SYS_getrandom,
        libc::SYS_clock_gettime,
        libc::SYS_clock_getres,
        libc::SYS_clock_nanosleep,
        libc::SYS_nanosleep,
        libc::SYS_gettimeofday,
        libc::SYS_ppoll,
        libc::SYS_pselect6,
    ];
    // The older calls x86_64 still has and its C library still makes.
    // `fork` and `vfork` take no flags, so they start nothing a `clone`
    // without namespace flags could not; shells such as dash use `vfork`.
    #[cfg(target_arch = "x86_64")]
    v.extend([
        libc::SYS_fork,
        libc::SYS_vfork,
        libc::SYS_open,
        libc::SYS_stat,
        libc::SYS_lstat,
        libc::SYS_access,
        libc::SYS_readlink,
        libc::SYS_getdents,
        libc::SYS_mkdir,
        libc::SYS_rmdir,
        libc::SYS_unlink,
        libc::SYS_rename,
        libc::SYS_dup2,
        libc::SYS_pipe,
        libc::SYS_poll,
        libc::SYS_select,
        libc::SYS_arch_prctl,
        libc::SYS_getpgrp,
        libc::SYS_time,
        libc::SYS_alarm,
    ]);
    v
}

/// `fcntl` commands [`Profile::NativeCode`] allows: descriptor flags,
/// status flags and locks. Not `F_SETOWN`, `F_SETOWN_EX` or `F_SETSIG`,
/// which have the kernel signal another process.
const FCNTL_ALLOWED: [libc::c_int; 13] = [
    libc::F_DUPFD,
    libc::F_GETFD,
    libc::F_SETFD,
    libc::F_GETFL,
    libc::F_SETFL,
    libc::F_GETLK,
    libc::F_SETLK,
    libc::F_SETLKW,
    libc::F_OFD_GETLK,
    libc::F_OFD_SETLK,
    libc::F_OFD_SETLKW,
    libc::F_DUPFD_CLOEXEC,
    libc::F_GETPIPE_SZ,
];

/// `ioctl` requests [`Profile::NativeCode`] allows (the generic numbers,
/// the same on x86_64 and aarch64): asking whether a descriptor is a
/// terminal and its size, bytes readable, non-blocking and close-on-exec.
const IOCTL_ALLOWED: [u64; 6] = [
    0x5401, // TCGETS
    0x5413, // TIOCGWINSZ
    0x541B, // FIONREAD
    0x5421, // FIONBIO
    0x5450, // FIONCLEX
    0x5451, // FIOCLEX
];

/// `prctl` options [`Profile::NativeCode`] allows: naming the thread and
/// anonymous memory, and reading whether the process is dumpable.
const PRCTL_ALLOWED: [u64; 4] = [
    libc::PR_SET_NAME as u64,
    libc::PR_GET_NAME as u64,
    libc::PR_GET_DUMPABLE as u64,
    0x5356_4d41, // PR_SET_VMA
];

fn compile(
    rules: BTreeMap<i64, Vec<SeccompRule>>,
    mismatch: SeccompAction,
    matched: SeccompAction,
) -> io::Result<BpfProgram> {
    let arch = std::env::consts::ARCH
        .try_into()
        .map_err(|_| io::Error::other("seccomp has no rules for this architecture"))?;
    let filter = SeccompFilter::new(rules, mismatch, matched, arch)
        .map_err(|e| io::Error::other(e.to_string()))?;
    BpfProgram::try_from(filter).map_err(|e| io::Error::other(e.to_string()))
}

fn to_io(e: seccompiler::BackendError) -> io::Error {
    io::Error::other(e.to_string())
}

/// One rule per value: argument `arg` (its low 32 bits) equals it.
fn any_of(arg: u8, values: impl IntoIterator<Item = u64>) -> io::Result<Vec<SeccompRule>> {
    values
        .into_iter()
        .map(|v| {
            let c = SeccompCondition::new(arg, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, v)
                .map_err(to_io)?;
            SeccompRule::new(vec![c]).map_err(to_io)
        })
        .collect()
}

/// The mask of every namespace flag `clone` could carry.
fn clone_ns_mask() -> u64 {
    CLONE_NEW.iter().fold(0u64, |m, f| m | *f as u64)
}

/// [`Profile::Denylist`]: [`denied_syscalls`] and any `clone` that asks for
/// a new namespace get `EACCES`; everything else is allowed.
fn denylist_program() -> io::Result<BpfProgram> {
    // c_long is i64 on the 64-bit targets aispice builds for, but the
    // conversion keeps this correct on a 32-bit libc where it is i32.
    #[allow(clippy::useless_conversion)]
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = denied_syscalls()
        .into_iter()
        .map(|n| (i64::from(n), Vec::new()))
        .collect();
    let mut clone = Vec::new();
    for flag in CLONE_NEW {
        let bit = flag as u64;
        let c = SeccompCondition::new(0, SeccompCmpArgLen::Qword, SeccompCmpOp::MaskedEq(bit), bit)
            .map_err(to_io)?;
        clone.push(SeccompRule::new(vec![c]).map_err(to_io)?);
    }
    #[allow(clippy::useless_conversion)]
    rules.insert(i64::from(libc::SYS_clone), clone);
    compile(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EACCES as u32),
    )
}

/// [`Profile::NativeCode`]: [`native_code_syscalls`] are allowed, `clone`
/// without a namespace flag, `fcntl`, `ioctl` and `prctl` with the listed
/// commands, `prlimit64` on the process itself only; everything else gets
/// `ENOSYS`, so the C library falls back where it can.
fn native_code_program() -> io::Result<BpfProgram> {
    #[allow(clippy::useless_conversion)]
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = native_code_syscalls()
        .into_iter()
        .map(|n| (i64::from(n), Vec::new()))
        .collect();
    let no_ns = SeccompCondition::new(
        0,
        SeccompCmpArgLen::Qword,
        SeccompCmpOp::MaskedEq(clone_ns_mask()),
        0,
    )
    .map_err(to_io)?;
    let own = |arg| any_of(arg, [0]);
    #[allow(clippy::useless_conversion)]
    {
        rules.insert(
            i64::from(libc::SYS_clone),
            vec![SeccompRule::new(vec![no_ns]).map_err(to_io)?],
        );
        rules.insert(
            i64::from(libc::SYS_fcntl),
            any_of(1, FCNTL_ALLOWED.iter().map(|c| *c as u64))?,
        );
        rules.insert(i64::from(libc::SYS_ioctl), any_of(1, IOCTL_ALLOWED)?);
        rules.insert(i64::from(libc::SYS_prctl), any_of(0, PRCTL_ALLOWED)?);
        rules.insert(i64::from(libc::SYS_prlimit64), own(0)?);
    }
    compile(
        rules,
        SeccompAction::Errno(libc::ENOSYS as u32),
        SeccompAction::Allow,
    )
}

/// `clone3` gets `ENOSYS`, so the C library falls back to `clone`.
fn clone3_program() -> io::Result<BpfProgram> {
    #[allow(clippy::useless_conversion)]
    let rules: BTreeMap<i64, Vec<SeccompRule>> = [(i64::from(libc::SYS_clone3), Vec::new())]
        .into_iter()
        .collect();
    compile(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
    )
}

fn op(code: u16, jt: u8, jf: u8, k: u32) -> sock_filter {
    sock_filter { code, jt, jf, k }
}

/// x86_64 only: refuse every x32 system call. An x32 call carries the
/// x86_64 audit architecture with bit 30 set in its number, so it passes
/// seccompiler's architecture check and then matches none of the rules,
/// which name only the 64-bit numbers: `0x40000000 | SYS_socket` would make
/// a socket. The programs aispice confines are 64-bit and never make one.
/// seccompiler generates no such check, so this program is written out.
#[cfg(target_arch = "x86_64")]
fn x32_program() -> BpfProgram {
    vec![
        op(LD_W_ABS, 0, 0, ARCH),
        // Another architecture: the profile's program kills it, allow here.
        op(JEQ_K, 0, 3, AUDIT_ARCH),
        op(LD_W_ABS, 0, 0, NR),
        op(JGE_K, 0, 1, X32_SYSCALL_BIT),
        op(RET_K, 0, 0, libc::SECCOMP_RET_ERRNO | libc::EACCES as u32),
        op(RET_K, 0, 0, ALLOW),
    ]
}

/// The programs a [`Profile`] compiles to, in the order they are
/// installed: `clone3`, on x86_64 x32, and the profile last. The order does
/// not change what they enforce (every filter runs on every call), but the
/// allowlist refuses `seccomp` and most of `prctl`, so nothing could be
/// installed after it, by aispice or by bubblewrap.
fn profile_programs(profile: Profile) -> io::Result<Vec<BpfProgram>> {
    #[allow(unused_mut)]
    let mut v = vec![clone3_program()?];
    #[cfg(target_arch = "x86_64")]
    v.push(x32_program());
    v.push(match profile {
        Profile::Denylist => denylist_program()?,
        Profile::NativeCode => native_code_program()?,
    });
    debug_assert_eq!(v.len(), PROFILE_PROGRAMS);
    Ok(v)
}

/// [`Signals::SelfOnly`]: a program that lets a signal reach only the
/// process it is applied to (whose pid is written in with [`Self::patch`])
/// and that process's group, which it leads. Every check reads the low 32
/// bits of the argument, as the kernel reads a `pid_t` or an `int` command.
struct SignalProgram {
    bpf: BpfProgram,
    /// Instructions that compare with the pid.
    pid_at: Vec<usize>,
    /// Instructions that compare with minus the pid (its process group).
    group_at: Vec<usize>,
}

impl SignalProgram {
    fn new() -> Self {
        let mut p = SignalProgram {
            bpf: vec![
                op(LD_W_ABS, 0, 0, ARCH),
                op(JEQ_K, 1, 0, AUDIT_ARCH),
                // Another architecture: the profile's program kills it.
                op(RET_K, 0, 0, ALLOW),
                op(LD_W_ABS, 0, 0, NR),
            ],
            pid_at: Vec::new(),
            group_at: Vec::new(),
        };
        // kill: the process, its group (0, or minus its pid); not -1 or
        // anyone else.
        p.allow_if(libc::SYS_kill, 0, &[Value::Pid, Value::Zero, Value::Group]);
        // The rest name a thread group or thread: the process's own.
        for nr in [
            libc::SYS_tkill,
            libc::SYS_tgkill,
            libc::SYS_rt_sigqueueinfo,
            libc::SYS_rt_tgsigqueueinfo,
        ] {
            p.allow_if(nr, 0, &[Value::Pid]);
        }
        p.deny(libc::SYS_pidfd_send_signal);
        // Ways to have the kernel signal another process later.
        p.deny_if(
            libc::SYS_fcntl,
            1,
            &[libc::F_SETOWN as u32, F_SETSIG, F_SETOWN_EX],
        );
        p.deny_if(
            libc::SYS_ioctl,
            1,
            &[
                0x8901, // FIOSETOWN
                0x8902, // SIOCSPGRP
            ],
        );
        p.bpf.push(op(RET_K, 0, 0, ALLOW));
        p
    }

    /// If the call is `nr`, return: allowed when argument `arg` is one of
    /// `values`, refused otherwise. A different call skips the block.
    fn allow_if(&mut self, nr: libc::c_long, arg: u32, values: &[Value]) {
        let n = values.len() as u8;
        self.bpf.push(op(JEQ_K, 0, n + 3, nr as u32));
        self.bpf.push(op(LD_W_ABS, 0, 0, arg_low(arg)));
        for (j, v) in values.iter().enumerate() {
            let at = self.bpf.len();
            match v {
                Value::Pid => self.pid_at.push(at),
                Value::Group => self.group_at.push(at),
                Value::Zero => {}
            }
            // Equal: jump over the remaining compares and the refusal.
            self.bpf.push(op(JEQ_K, n - j as u8, 0, 0));
        }
        self.bpf.push(op(RET_K, 0, 0, DENY));
        self.bpf.push(op(RET_K, 0, 0, ALLOW));
    }

    /// If the call is `nr`, refuse it when argument `arg` is one of
    /// `values` and allow it otherwise.
    fn deny_if(&mut self, nr: libc::c_long, arg: u32, values: &[u32]) {
        let n = values.len() as u8;
        self.bpf.push(op(JEQ_K, 0, n + 3, nr as u32));
        self.bpf.push(op(LD_W_ABS, 0, 0, arg_low(arg)));
        for (j, v) in values.iter().enumerate() {
            // Equal: jump over the remaining compares and the allow.
            self.bpf.push(op(JEQ_K, n - j as u8, 0, *v));
        }
        self.bpf.push(op(RET_K, 0, 0, ALLOW));
        self.bpf.push(op(RET_K, 0, 0, DENY));
    }

    /// Refuse the call `nr` outright.
    fn deny(&mut self, nr: libc::c_long) {
        self.bpf.push(op(JEQ_K, 0, 1, nr as u32));
        self.bpf.push(op(RET_K, 0, 0, DENY));
    }

    /// Write `pid` into the program. Allocation-free, for `pre_exec`.
    fn patch(&mut self, pid: libc::pid_t) {
        for &i in &self.pid_at {
            self.bpf[i].k = pid as u32;
        }
        for &i in &self.group_at {
            self.bpf[i].k = pid.wrapping_neg() as u32;
        }
    }
}

/// A value a signal check compares with.
enum Value {
    Pid,
    Group,
    Zero,
}

/// The bytes bubblewrap reads a program from: the `sock_filter` array as
/// the kernel takes it.
fn bpf_bytes(prog: &[sock_filter]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prog.len() * 8);
    for f in prog {
        out.extend_from_slice(&f.code.to_ne_bytes());
        out.push(f.jt);
        out.push(f.jf);
        out.extend_from_slice(&f.k.to_ne_bytes());
    }
    out
}

/// A sealed-off memory file holding `bytes`, read from the start.
fn memfd_with(bytes: &[u8]) -> io::Result<OwnedFd> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::FromRawFd;
    // SAFETY: a valid name and flags; the returned fd is owned below.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"aispice-seccomp".as_ptr(),
            libc::MFD_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) });
    file.write_all(bytes)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(OwnedFd::from(file))
}

/// Tasks (processes and threads) whose real user is `uid` right now, which
/// is what `RLIMIT_NPROC` counts. Read from `/proc`, in the parent.
fn tasks_of(uid: libc::uid_t) -> u64 {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut n = 0u64;
    for e in dir.flatten() {
        if !e.file_name().as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(e.path().join("status")) else {
            continue;
        };
        let mut real = None;
        let mut threads = 1;
        for line in status.lines() {
            if let Some(v) = line.strip_prefix("Uid:") {
                real = v.split_whitespace().next().and_then(|s| s.parse().ok());
            } else if let Some(v) = line.strip_prefix("Threads:") {
                threads = v.trim().parse().unwrap_or(1);
            }
        }
        if real == Some(uid) {
            n += threads;
        }
    }
    n
}

/// The `RLIMIT_NPROC` value for a child that may start `headroom` more
/// tasks than its user has now, never above the current hard limit. A
/// count of zero (no `/proc`) leaves only the headroom, which fails closed.
fn nproc_limit(headroom: u64) -> io::Result<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a valid resource id and a stack rlimit to fill.
    if unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let want = tasks_of(uid).saturating_add(headroom);
    Ok(want.min(lim.rlim_max))
}

/// Mark every descriptor from `first` up close-on-exec. One `close_range`
/// call on Linux 5.11 and later; on an older kernel a loop up to the
/// descriptor limit. Syscalls only, safe in `pre_exec`.
unsafe fn cloexec_from(first: libc::c_int) -> io::Result<()> {
    // SAFETY: close_range with CLOSE_RANGE_CLOEXEC only sets flags.
    let r = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first as u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if r == 0 {
        return Ok(());
    }
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a valid resource id and a stack rlimit to fill.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let top = lim.rlim_cur.min(1 << 20) as libc::c_int;
    for fd in first..top {
        // SAFETY: fcntl on any number is harmless; a closed one is EBADF.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        }
    }
    Ok(())
}

/// Put `fds` at descriptors 3, 4, ... without close-on-exec, so the program
/// inherits exactly those. They are first copied above the target range,
/// so moving one never overwrites another. Syscalls only, safe in
/// `pre_exec`.
unsafe fn pass_fds(fds: &[OwnedFd]) -> io::Result<()> {
    let mut high = [0 as libc::c_int; MAX_PROGRAMS];
    if fds.len() > high.len() {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let n = fds.len() as libc::c_int;
    for (i, fd) in fds.iter().enumerate() {
        // SAFETY: F_DUPFD_CLOEXEC on a valid fd, lowest free at or above 3 + n.
        let h = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3 + n) };
        if h < 0 {
            return Err(io::Error::last_os_error());
        }
        high[i] = h;
    }
    for (i, h) in high.iter().take(fds.len()).enumerate() {
        // SAFETY: dup2 onto a fixed number; the new descriptor has no CLOEXEC.
        if unsafe { libc::dup2(*h, 3 + i as libc::c_int) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Set one resource limit. A syscall only, safe in `pre_exec`.
unsafe fn setrlimit(resource: libc::__rlimit_resource_t, value: u64) -> io::Result<()> {
    let lim = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    // SAFETY: a valid resource id and a stack rlimit.
    if unsafe { libc::setrlimit(resource, &lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// What `pre_exec` does with seccomp, built in the parent.
enum Seccomp {
    None,
    /// Install these, and the signal program with the pid written in.
    Apply {
        programs: Vec<BpfProgram>,
        signals: Option<SignalProgram>,
    },
    /// Hand these to the program as descriptors 3.. (bubblewrap).
    Pass(Vec<OwnedFd>),
}

fn plan_seccomp(c: &Confinement) -> io::Result<Seccomp> {
    let Some(SeccompPolicy { profile, signals }) = c.seccomp().copied() else {
        return Ok(Seccomp::None);
    };
    let programs = profile_programs(profile)?;
    if c.seccomp_as_fds() {
        // bubblewrap runs the program as pid 2 of its PID namespace, under
        // its reaper at pid 1, and makes it lead its own session and group
        // (`--new-session`), so the signal program is written with that pid
        // here and goes first, as below.
        let mut all = Vec::new();
        if signals == Signals::SelfOnly {
            let mut s = SignalProgram::new();
            s.patch(BWRAP_PROGRAM_PID);
            all.push(s.bpf);
        }
        all.extend(programs);
        let fds = all
            .iter()
            .map(|p| memfd_with(&bpf_bytes(p)))
            .collect::<io::Result<Vec<_>>>()?;
        return Ok(Seccomp::Pass(fds));
    }
    Ok(Seccomp::Apply {
        programs,
        signals: (signals == Signals::SelfOnly).then(SignalProgram::new),
    })
}

/// Confine `cmd`'s child: build the Landlock ruleset and the seccomp
/// programs now, and install a `pre_exec` hook that applies them. The hook
/// makes no allocation and holds no lock.
pub fn apply(cmd: &mut tokio::process::Command, c: &Confinement) -> io::Result<()> {
    let ruleset = match c.landlock() {
        Some(ll) => Some(build_landlock(ll)?),
        None => None,
    };
    let mut seccomp = plan_seccomp(c)?;
    let limits = *c.limits();
    let nproc = match limits.nproc_headroom {
        Some(h) => Some(nproc_limit(h)?),
        None => None,
    };
    // SAFETY: the hook runs in the forked child before exec. It only makes
    // syscalls over state built in the parent (the limit values, the
    // seccomp programs and descriptors, the ruleset descriptor) and writes
    // the pid into the signal program in place; it allocates nothing and
    // takes no lock, so it cannot deadlock against a thread that held one at
    // fork. The ruleset fd is valid in the child and closes on exec.
    unsafe {
        cmd.pre_exec(move || {
            match &seccomp {
                Seccomp::Pass(fds) => {
                    pass_fds(fds)?;
                    cloexec_from(3 + fds.len() as libc::c_int)?;
                }
                _ => cloexec_from(3)?,
            }
            if let Some(v) = nproc {
                setrlimit(libc::RLIMIT_NPROC, v)?;
            }
            if let Some(v) = limits.file_size {
                setrlimit(libc::RLIMIT_FSIZE, v)?;
            }
            if let Some(v) = limits.address_space {
                setrlimit(libc::RLIMIT_AS, v)?;
            }
            // Die if aispice dies. This is not inherited across fork, so
            // the run's process group is also killed when the run ends.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            // Landlock first: the allowlist profile does not let its calls
            // through.
            if let Some(fd) = &ruleset {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::syscall(libc::SYS_landlock_restrict_self, fd.as_raw_fd(), 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            // seccompiler sets no_new_privs and installs each filter over a
            // borrowed slice, with no allocation. The signal program goes
            // first and the profile last (see `profile_programs`).
            if let Seccomp::Apply { programs, signals } = &mut seccomp {
                if let Some(s) = signals {
                    s.patch(libc::getpid());
                    seccompiler::apply_filter(&s.bpf)
                        .map_err(|_| io::Error::from_raw_os_error(libc::EPERM))?;
                }
                for bpf in programs.iter() {
                    seccompiler::apply_filter(bpf)
                        .map_err(|_| io::Error::from_raw_os_error(libc::EPERM))?;
                }
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f` in a forked child and return its result as the exit code.
    /// `f` must only make syscalls: the child allocates nothing.
    fn in_child(f: impl FnOnce() -> i32) -> Option<i32> {
        // SAFETY: the child only runs `f`, which makes syscalls, and _exit.
        match unsafe { libc::fork() } {
            -1 => panic!("fork failed"),
            0 => unsafe { libc::_exit(f()) },
            pid => {
                let mut status = 0;
                unsafe { libc::waitpid(pid, &mut status, 0) };
                libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status))
            }
        }
    }

    /// The first check that failed, numbered from 1, or 0.
    fn first_failed(checks: &[bool]) -> i32 {
        checks.iter().position(|ok| !ok).map_or(0, |i| i as i32 + 1)
    }

    fn fails_with(r: libc::c_long, errno: i32) -> bool {
        r < 0 && io::Error::last_os_error().raw_os_error() == Some(errno)
    }

    fn apply_all(programs: &[BpfProgram]) -> bool {
        programs
            .iter()
            .all(|bpf| seccompiler::apply_filter(bpf).is_ok())
    }

    /// The denylist refuses creating any socket (so no TCP, UDP,
    /// abstract-UNIX or pathname-UNIX connection), also through the x32
    /// call numbers, io_uring, System V and POSIX inter-process
    /// communication, a new session or process group, a new namespace
    /// through `unshare` or `clone`, and answers `clone3` with `ENOSYS`,
    /// while a local `socketpair` and a plain `fork` still work.
    #[test]
    fn the_denylist_refuses_sockets_ipc_sessions_namespaces_and_io_uring() {
        let programs = profile_programs(Profile::Denylist).unwrap();
        let code = in_child(|| {
            let applied = apply_all(&programs);
            let sock = |domain, kind| unsafe { libc::socket(domain, kind, 0) } as libc::c_long;
            let inet = fails_with(sock(libc::AF_INET, libc::SOCK_STREAM), libc::EACCES);
            let unix = fails_with(sock(libc::AF_UNIX, libc::SOCK_STREAM), libc::EACCES);
            let udp = fails_with(sock(libc::AF_INET, libc::SOCK_DGRAM), libc::EACCES);
            let ring = fails_with(
                unsafe {
                    libc::syscall(libc::SYS_io_uring_setup, 1u32, std::ptr::null_mut::<u8>())
                },
                libc::EACCES,
            );
            let userns = fails_with(
                unsafe { libc::unshare(libc::CLONE_NEWUSER) } as libc::c_long,
                libc::EACCES,
            );
            let clone_ns = fails_with(
                unsafe {
                    libc::syscall(
                        libc::SYS_clone,
                        (libc::CLONE_NEWNET | libc::SIGCHLD) as libc::c_ulong,
                        0usize,
                        0usize,
                        0usize,
                        0usize,
                    )
                },
                libc::EACCES,
            );
            let clone3 = fails_with(
                unsafe { libc::syscall(libc::SYS_clone3, std::ptr::null_mut::<u8>(), 0usize) },
                libc::ENOSYS,
            );
            let x32 = x32_socket_refused();
            let shm = fails_with(
                unsafe { libc::shmget(libc::IPC_PRIVATE, 4096, libc::IPC_CREAT | 0o600) }
                    as libc::c_long,
                libc::EACCES,
            );
            let msg = fails_with(
                unsafe { libc::msgget(libc::IPC_PRIVATE, libc::IPC_CREAT | 0o600) } as libc::c_long,
                libc::EACCES,
            );
            let sem = fails_with(
                unsafe { libc::semget(libc::IPC_PRIVATE, 1, libc::IPC_CREAT | 0o600) }
                    as libc::c_long,
                libc::EACCES,
            );
            let mq = fails_with(
                unsafe {
                    libc::syscall(
                        libc::SYS_mq_open,
                        c"aispice-confine-test".as_ptr(),
                        libc::O_RDWR | libc::O_CREAT,
                        0o600,
                        std::ptr::null::<u8>(),
                    )
                },
                libc::EACCES,
            );
            let session = fails_with(unsafe { libc::setsid() } as libc::c_long, libc::EACCES);
            let group = fails_with(unsafe { libc::setpgid(0, 0) } as libc::c_long, libc::EACCES);
            let mut sv = [0i32; 2];
            let pair_ok =
                unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) }
                    == 0;
            let fork_ok = fork_works();
            first_failed(&[
                applied, inet, unix, udp, ring, userns, clone_ns, clone3, x32, shm, msg, sem, mq,
                session, group, pair_ok, fork_ok,
            ])
        });
        // 1 applied, 2 inet, 3 unix, 4 udp, 5 io_uring, 6 unshare, 7 clone,
        // 8 clone3, 9 x32, 10 shm, 11 msg, 12 sem, 13 mq, 14 setsid,
        // 15 setpgid, 16 socketpair, 17 fork.
        assert_eq!(code, Some(0), "denylist check {code:?} did not hold");
    }

    /// An x32 socket call is refused with `EACCES` (x86_64; elsewhere there
    /// are no x32 calls). Without the x32 program it would be `ENOSYS` on a
    /// kernel without x32 and a socket with it.
    fn x32_socket_refused() -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            fails_with(
                unsafe {
                    libc::syscall(
                        libc::c_long::from(X32_SYSCALL_BIT) | libc::SYS_socket,
                        libc::AF_UNIX,
                        libc::SOCK_STREAM,
                        0,
                    )
                },
                libc::EACCES,
            )
        }
        #[cfg(not(target_arch = "x86_64"))]
        true
    }

    fn fork_works() -> bool {
        match unsafe { libc::fork() } {
            0 => unsafe { libc::_exit(0) },
            pid if pid > 0 => {
                let mut st = 0;
                unsafe { libc::waitpid(pid, &mut st, 0) };
                libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0
            }
            _ => false,
        }
    }

    /// The allowlist answers anything off its list `ENOSYS` (sockets,
    /// socketpair, io_uring, ptrace, IPC, unshare, a `clone` with a
    /// namespace flag, `prlimit64` on another process, `fcntl` `F_SETOWN`,
    /// `ioctl` `FIOSETOWN`, `sched_setaffinity`), while files, memory,
    /// threads, `fork` and its own limits still work.
    #[test]
    fn the_native_code_allowlist_lets_only_computation_through() {
        let programs = profile_programs(Profile::NativeCode).unwrap();
        let mut p = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
        let code = in_child(|| {
            let applied = apply_all(&programs);
            let enosys = |r: libc::c_long| fails_with(r, libc::ENOSYS);
            let sock = enosys(
                unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) } as libc::c_long
            );
            let mut sv = [0i32; 2];
            let pair = enosys(unsafe {
                libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr())
            } as libc::c_long);
            let ring = enosys(unsafe {
                libc::syscall(libc::SYS_io_uring_setup, 1u32, std::ptr::null_mut::<u8>())
            });
            let trace =
                enosys(unsafe { libc::syscall(libc::SYS_ptrace, libc::PTRACE_TRACEME, 0, 0, 0) });
            let shm =
                enosys(
                    unsafe { libc::shmget(libc::IPC_PRIVATE, 4096, libc::IPC_CREAT | 0o600) }
                        as libc::c_long,
                );
            let userns = enosys(unsafe { libc::unshare(libc::CLONE_NEWUSER) } as libc::c_long);
            let clone_ns = enosys(unsafe {
                libc::syscall(
                    libc::SYS_clone,
                    (libc::CLONE_NEWNET | libc::SIGCHLD) as libc::c_ulong,
                    0usize,
                    0usize,
                    0usize,
                    0usize,
                )
            });
            let parent = unsafe { libc::getppid() };
            let mut lim = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            let other_limits = enosys(unsafe {
                libc::syscall(
                    libc::SYS_prlimit64,
                    parent,
                    libc::RLIMIT_NOFILE,
                    std::ptr::null::<libc::rlimit>(),
                    &mut lim as *mut libc::rlimit,
                )
            });
            let own_limits = unsafe {
                libc::syscall(
                    libc::SYS_prlimit64,
                    0,
                    libc::RLIMIT_NOFILE,
                    std::ptr::null::<libc::rlimit>(),
                    &mut lim as *mut libc::rlimit,
                )
            } == 0;
            let setown =
                enosys(unsafe { libc::fcntl(p[0], libc::F_SETOWN, parent) } as libc::c_long);
            let getfd = unsafe { libc::fcntl(p[0], libc::F_GETFD) } >= 0;
            let fiosetown =
                enosys(
                    unsafe { libc::ioctl(p[0], 0x8901, &parent as *const libc::pid_t) }
                        as libc::c_long,
                );
            let affinity = enosys(unsafe {
                libc::syscall(
                    libc::SYS_sched_setaffinity,
                    0,
                    0usize,
                    std::ptr::null::<u8>(),
                )
            });
            let wrote = unsafe { libc::write(p[1], b"x".as_ptr().cast(), 1) } == 1;
            let fork_ok = fork_works();
            first_failed(&[
                applied,
                sock,
                pair,
                ring,
                trace,
                shm,
                userns,
                clone_ns,
                other_limits,
                own_limits,
                setown,
                getfd,
                fiosetown,
                affinity,
                wrote,
                fork_ok,
                x32_socket_refused_enosys_or_eacces(),
            ])
        });
        unsafe {
            libc::close(p[0]);
            libc::close(p[1]);
        }
        // 1 applied, 2 socket, 3 socketpair, 4 io_uring, 5 ptrace, 6 shm,
        // 7 unshare, 8 clone, 9 prlimit other, 10 prlimit own, 11 F_SETOWN,
        // 12 F_GETFD, 13 FIOSETOWN, 14 sched_setaffinity, 15 write, 16 fork,
        // 17 x32.
        assert_eq!(code, Some(0), "allowlist check {code:?} did not hold");
    }

    /// Under the allowlist an x32 call is refused one way or the other.
    fn x32_socket_refused_enosys_or_eacces() -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            let r = unsafe {
                libc::syscall(
                    libc::c_long::from(X32_SYSCALL_BIT) | libc::SYS_socket,
                    libc::AF_UNIX,
                    libc::SOCK_STREAM,
                    0,
                )
            };
            r < 0
        }
        #[cfg(not(target_arch = "x86_64"))]
        true
    }

    /// With the pid written in, signals reach the process and its group
    /// only: `kill` of itself, of 0 and of minus its pid, and `tgkill` of
    /// its own thread, work (or fail for another reason than the filter),
    /// while `kill` of its parent, of 1 and of -1, `tgkill` and
    /// `rt_sigqueueinfo` of the parent, `pidfd_send_signal`, `F_SETOWN` and
    /// `FIOSETOWN` are refused with `EPERM`. A child it forks may still
    /// signal it.
    #[test]
    fn the_signal_program_keeps_signals_to_the_process_and_its_group() {
        let mut prog = SignalProgram::new();
        let mut p = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
        let code = in_child(|| {
            // Lead a group of its own, as a confined run does.
            let led = unsafe { libc::setpgid(0, 0) } == 0;
            let me = unsafe { libc::getpid() };
            prog.patch(me);
            let applied = seccompiler::apply_filter(&prog.bpf).is_ok();
            let parent = unsafe { libc::getppid() };
            let eperm = |r: libc::c_long| fails_with(r, libc::EPERM);
            let ok = |r: libc::c_long| r == 0;
            let kill = |pid: libc::pid_t| unsafe { libc::kill(pid, 0) } as libc::c_long;
            let tgkill = |tgid: libc::pid_t, tid: libc::pid_t| unsafe {
                libc::syscall(libc::SYS_tgkill, tgid, tid, 0)
            };
            let tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::pid_t;
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let queue = eperm(unsafe {
                libc::syscall(
                    libc::SYS_rt_sigqueueinfo,
                    parent,
                    libc::SIGUSR1,
                    &mut info as *mut libc::siginfo_t,
                )
            });
            let pidfd = eperm(unsafe {
                libc::syscall(libc::SYS_pidfd_send_signal, 0, 0, std::ptr::null::<u8>(), 0)
            });
            let setown =
                eperm(unsafe { libc::fcntl(p[0], libc::F_SETOWN, parent) } as libc::c_long);
            let fiosetown =
                eperm(
                    unsafe { libc::ioctl(p[0], 0x8901, &parent as *const libc::pid_t) }
                        as libc::c_long,
                );
            // A grandchild may signal the process it was forked from.
            let child_signals_root = match unsafe { libc::fork() } {
                0 => unsafe { libc::_exit(if kill(me) == 0 { 0 } else { 1 }) },
                pid if pid > 0 => {
                    let mut st = 0;
                    unsafe { libc::waitpid(pid, &mut st, 0) };
                    libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0
                }
                _ => false,
            };
            first_failed(&[
                led,
                applied,
                ok(kill(me)),
                ok(kill(0)),
                ok(kill(-me)),
                ok(tgkill(me, tid)),
                eperm(kill(parent)),
                eperm(kill(1)),
                eperm(kill(-1)),
                eperm(tgkill(parent, parent)),
                queue,
                pidfd,
                setown,
                fiosetown,
                child_signals_root,
            ])
        });
        unsafe {
            libc::close(p[0]);
            libc::close(p[1]);
        }
        // 1 setpgid, 2 applied, 3 kill self, 4 kill 0, 5 kill -self,
        // 6 tgkill self, 7 kill parent, 8 kill 1, 9 kill -1, 10 tgkill parent,
        // 11 rt_sigqueueinfo, 12 pidfd_send_signal, 13 F_SETOWN,
        // 14 FIOSETOWN, 15 grandchild to root.
        assert_eq!(code, Some(0), "signal check {code:?} did not hold");
    }

    /// The bytes handed to bubblewrap are the programs as the kernel takes
    /// them, read back from the memory file from the start.
    #[test]
    fn programs_handed_over_read_back_whole() {
        use std::io::Read;
        for profile in [Profile::Denylist, Profile::NativeCode] {
            let programs = profile_programs(profile).unwrap();
            assert_eq!(programs.len(), PROFILE_PROGRAMS);
            for prog in &programs {
                let bytes = bpf_bytes(prog);
                assert_eq!(bytes.len(), prog.len() * 8);
                let mut back = Vec::new();
                std::fs::File::from(memfd_with(&bytes).unwrap())
                    .read_to_end(&mut back)
                    .unwrap();
                assert_eq!(back, bytes);
            }
        }
        // Native code under bubblewrap: the signal program first, written
        // with the program's pid in bubblewrap's namespace.
        let c = Confinement::for_bwrap(Default::default(), Profile::NativeCode);
        let Seccomp::Pass(fds) = plan_seccomp(&c).unwrap() else {
            panic!("bubblewrap's programs are not handed over");
        };
        assert_eq!(fds.len(), PROFILE_PROGRAMS + 1);
        let mut want = SignalProgram::new();
        want.patch(BWRAP_PROGRAM_PID);
        let mut back = Vec::new();
        std::fs::File::from(fds.into_iter().next().unwrap())
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, bpf_bytes(&want.bpf));
    }

    /// The process count covers at least this test's own threads, and the
    /// limit is that count plus the headroom, capped by the hard limit.
    #[test]
    fn the_process_limit_is_relative_to_what_the_user_runs() {
        let uid = unsafe { libc::getuid() };
        let now = tasks_of(uid);
        assert!(now >= 1, "{now}");
        let limit = nproc_limit(1000).unwrap();
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut lim) }, 0);
        assert!(limit <= lim.rlim_max);
        if lim.rlim_max > now + 2000 {
            assert!(limit >= 1000 && limit < now + 2000, "{limit} vs {now}");
        }
    }

    /// In a forked child, a descriptor without close-on-exec gets it, and
    /// stdin, stdout and stderr keep theirs; descriptors passed on land at
    /// 3.. without it.
    #[test]
    fn only_passed_descriptors_survive_exec() {
        let mut p = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
        let flag = |fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC;
        assert_eq!(flag(p[0]), 0, "pipe() makes descriptors without CLOEXEC");
        // As many as any policy hands bubblewrap: the profile's programs
        // and the signal program.
        let passed: Vec<OwnedFd> = (0..MAX_PROGRAMS)
            .map(|i| memfd_with(&[i as u8]).unwrap())
            .collect();
        let top = 3 + passed.len() as libc::c_int;
        let code = in_child(|| {
            let moved = unsafe { pass_fds(&passed) }.is_ok();
            let swept = unsafe { cloexec_from(top) }.is_ok();
            let mut checks = vec![moved, swept];
            checks.extend((3..top).map(|fd| flag(fd) == 0));
            checks.extend([
                flag(p[0]) != 0 || p[0] < top,
                flag(p[1]) != 0 || p[1] < top,
                flag(2) == 0,
            ]);
            first_failed(&checks)
        });
        unsafe {
            libc::close(p[0]);
            libc::close(p[1]);
        }
        assert_eq!(code, Some(0), "descriptor check {code:?} did not hold");
    }

    #[test]
    fn rulesets_exist_for_abi_4_to_6_only() {
        for a in [4, 5, 6] {
            assert!(ruleset_abi(a).is_ok());
        }
        for a in [0, 3, 7] {
            assert!(ruleset_abi(a).is_err());
        }
    }
}

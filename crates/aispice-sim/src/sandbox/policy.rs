//! What a confinement is made of, checked as a whole when it is built.
//!
//! A [`Confinement`] is only made by [`ConfinementBuilder::build`], which
//! refuses a combination that leaves a gap: a Landlock ruleset older than
//! ABI 4, Landlock scopes the ABI cannot enforce, or a ruleset below ABI 6
//! (which cannot scope signals and abstract UNIX sockets) without a seccomp
//! policy that does both instead. The types are plain data on every
//! platform; only Linux enforces them (`crate::backend::confine`), and
//! running a confined process anywhere else is an error, never a silent
//! unconfined run.

use std::path::PathBuf;

/// The oldest Landlock ABI a ruleset may be written against. ABI 3 is the
/// first to restrict truncation and ABI 4 the first with network rules.
pub const MIN_LANDLOCK_ABI: u8 = 4;
/// The first Landlock ABI that scopes signals and abstract UNIX sockets.
pub const SCOPED_LANDLOCK_ABI: u8 = 6;
/// The newest ABI aispice knows how to write a ruleset for.
pub const MAX_LANDLOCK_ABI: u8 = 6;

/// How many seccomp programs a [`Profile`] compiles to: the `clone3`
/// program, on x86_64 the x32 program, and the profile. bubblewrap gets one
/// descriptor for each, numbered from 3, and installs them in that order.
pub(crate) const PROFILE_PROGRAMS: usize = if cfg!(target_arch = "x86_64") { 3 } else { 2 };

/// How a child process is confined. Made by [`Confinement::builder`].
#[derive(Debug, Clone)]
pub struct Confinement {
    landlock: Option<LandlockPolicy>,
    seccomp: Option<SeccompPolicy>,
    limits: Limits,
    /// Hand the seccomp profile's programs to the program as descriptors
    /// 3.. instead of applying them: bubblewrap installs them inside its
    /// sandbox (`--add-seccomp-fd`). Only [`crate::sandbox::Sandbox`] sets it.
    /// Read only where confinement is enforced.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    seccomp_as_fds: bool,
}

impl Confinement {
    pub fn builder() -> ConfinementBuilder {
        ConfinementBuilder::default()
    }

    /// Resource limits only: no Landlock, no seccomp. For a process that is
    /// already isolated otherwise (inside bubblewrap's namespaces).
    pub fn limits_only(limits: Limits) -> Self {
        Self {
            landlock: None,
            seccomp: None,
            limits,
            seccomp_as_fds: false,
        }
    }

    /// The limits, with `profile`'s programs handed to the program (which
    /// is bubblewrap) as descriptors 3.. for it to install.
    pub(crate) fn for_bwrap(limits: Limits, profile: Profile) -> Self {
        Self {
            landlock: None,
            seccomp: Some(SeccompPolicy {
                profile,
                signals: Signals::Unrestricted,
            }),
            limits,
            seccomp_as_fds: true,
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn seccomp_as_fds(&self) -> bool {
        self.seccomp_as_fds
    }

    pub fn landlock(&self) -> Option<&LandlockPolicy> {
        self.landlock.as_ref()
    }

    pub fn seccomp(&self) -> Option<&SeccompPolicy> {
        self.seccomp.as_ref()
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }
}

/// A Landlock ruleset: every filesystem and network access the ABI knows is
/// handled, at a hard requirement (a kernel below `min_abi` is refused), and
/// only what is listed here is granted. Each path is opened once with
/// `O_PATH|O_NOFOLLOW` and the rule built from that descriptor.
#[derive(Debug, Clone, Default)]
pub struct LandlockPolicy {
    /// Files and folders the process may read; a folder is also listable.
    pub read: Vec<PathBuf>,
    /// The only files the process may execute.
    pub exec: Vec<PathBuf>,
    /// Folders the process may write under, plain files and folders only
    /// (no symlink, socket, fifo or device).
    pub write: Vec<PathBuf>,
    /// Single files the process may read and write (`/dev/null`).
    pub rw: Vec<PathBuf>,
    /// The ABI the ruleset is written against, at least
    /// [`MIN_LANDLOCK_ABI`] and at most [`MAX_LANDLOCK_ABI`].
    pub min_abi: u8,
    /// What the ruleset scopes off (ABI 6 and later).
    pub scopes: Scopes,
}

/// Landlock scopes: IPC the domain may not reach outside itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scopes {
    /// No connection to an abstract UNIX socket created outside the domain.
    pub abstract_unix_socket: bool,
    /// No signal to a process outside the domain.
    pub signal: bool,
}

impl Scopes {
    pub const ALL: Scopes = Scopes {
        abstract_unix_socket: true,
        signal: true,
    };

    fn any(self) -> bool {
        self.abstract_unix_socket || self.signal
    }
}

/// A seccomp policy: which system calls the process may make, and to whom it
/// may send signals. Every profile refuses creating a socket (only
/// `socketpair` may stay), so neither TCP nor any UNIX socket, abstract or
/// pathname, is reachable; on x86_64 every x32 call is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeccompPolicy {
    pub profile: Profile,
    pub signals: Signals,
}

/// Which system calls a profile lets through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Everything except the calls an escape would use: sockets and
    /// anything done on one, io_uring, ptrace and reading or writing
    /// another process, taking its descriptors, the keyring, BPF, perf,
    /// userfaultfd, System V and POSIX IPC, mounts, new namespaces, and
    /// leaving the process group. For a large program whose calls are not
    /// known in advance (xschem, with Tcl and Tk).
    Denylist,
    /// Only what a computational program needs: files (as Landlock allows),
    /// memory, threads, time, its own signals, and starting a program (as
    /// Landlock allows). Everything else gets `ENOSYS`. For untrusted native
    /// code, such as ngspice loading a user's OSDI or `d_cosim` library.
    NativeCode,
}

/// To whom the process may send signals, enforced by seccomp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signals {
    /// seccomp does not look at signals: a Landlock signal scope (ABI 6)
    /// or a PID namespace keeps them inside.
    Unrestricted,
    /// Only to the confined process itself and its own process group:
    /// `kill`, `tkill`, `tgkill`, `rt_sigqueueinfo` and `rt_tgsigqueueinfo`
    /// are checked against the process's pid, which is written into the
    /// filter just before it starts; `kill(-1)` and `pidfd_send_signal` are
    /// refused, and so are the ways to have the kernel signal someone else
    /// (`fcntl` `F_SETOWN`, `F_SETOWN_EX`, `F_SETSIG`; `ioctl` `FIOSETOWN`,
    /// `SIOCSPGRP`). A process the confined one starts may signal it and the
    /// group, but not itself by pid.
    SelfOnly,
}

/// Resource limits, set before the process starts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    /// `RLIMIT_FSIZE`: the largest file the process may write.
    pub file_size: Option<u64>,
    /// `RLIMIT_AS`: its address space.
    pub address_space: Option<u64>,
    /// `RLIMIT_NPROC` as a margin over what its user runs when it starts.
    pub nproc_headroom: Option<u64>,
}

/// Builds a [`Confinement`], refusing one that leaves a gap.
#[derive(Debug, Clone, Default)]
pub struct ConfinementBuilder {
    landlock: Option<LandlockPolicy>,
    seccomp: Option<SeccompPolicy>,
    limits: Limits,
}

impl ConfinementBuilder {
    pub fn landlock(mut self, policy: LandlockPolicy) -> Self {
        self.landlock = Some(policy);
        self
    }

    pub fn seccomp(mut self, policy: SeccompPolicy) -> Self {
        self.seccomp = Some(policy);
        self
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// The confinement, or why it would leave a gap.
    pub fn build(self) -> Result<Confinement, PolicyError> {
        if let Some(ll) = &self.landlock {
            let abi = ll.min_abi;
            if abi < MIN_LANDLOCK_ABI {
                return Err(PolicyError::AbiTooOld { abi });
            }
            if abi > MAX_LANDLOCK_ABI {
                return Err(PolicyError::AbiUnknown { abi });
            }
            if abi < SCOPED_LANDLOCK_ABI && ll.scopes.any() {
                return Err(PolicyError::ScopesNeedAbi6 { abi });
            }
            let signals_held =
                ll.scopes.signal || self.seccomp.is_some_and(|s| s.signals == Signals::SelfOnly);
            if !signals_held {
                return Err(PolicyError::SignalsUnconfined { abi });
            }
            // Every seccomp profile refuses socket creation, which covers
            // abstract UNIX sockets when the ruleset cannot scope them.
            if !ll.scopes.abstract_unix_socket && self.seccomp.is_none() {
                return Err(PolicyError::SocketsUnconfined { abi });
            }
        }
        Ok(Confinement {
            landlock: self.landlock,
            seccomp: self.seccomp,
            limits: self.limits,
            seccomp_as_fds: false,
        })
    }
}

/// Why a confinement was not built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error(
        "Landlock ABI {abi} is below {MIN_LANDLOCK_ABI}, which is needed to restrict truncation and network access"
    )]
    AbiTooOld { abi: u8 },
    #[error("Landlock ABI {abi} is newer than aispice writes rulesets for ({MAX_LANDLOCK_ABI})")]
    AbiUnknown { abi: u8 },
    #[error("Landlock ABI {abi} cannot scope signals or abstract sockets (ABI 6 and later can)")]
    ScopesNeedAbi6 { abi: u8 },
    #[error(
        "with Landlock ABI {abi} and no signal scope, signals need a seccomp policy with Signals::SelfOnly"
    )]
    SignalsUnconfined { abi: u8 },
    #[error(
        "with Landlock ABI {abi} and no abstract-socket scope, sockets need a seccomp policy, which refuses them"
    )]
    SocketsUnconfined { abi: u8 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ll(min_abi: u8, scopes: Scopes) -> LandlockPolicy {
        LandlockPolicy {
            min_abi,
            scopes,
            ..LandlockPolicy::default()
        }
    }

    const SELF_ONLY: SeccompPolicy = SeccompPolicy {
        profile: Profile::NativeCode,
        signals: Signals::SelfOnly,
    };

    #[test]
    fn abi_6_with_scopes_needs_no_seccomp() {
        assert!(
            Confinement::builder()
                .landlock(ll(6, Scopes::ALL))
                .build()
                .is_ok()
        );
    }

    #[test]
    fn below_abi_6_signals_and_sockets_need_seccomp() {
        for abi in [4, 5] {
            assert_eq!(
                Confinement::builder()
                    .landlock(ll(abi, Scopes::default()))
                    .build()
                    .unwrap_err(),
                PolicyError::SignalsUnconfined { abi }
            );
            let unrestricted = SeccompPolicy {
                profile: Profile::Denylist,
                signals: Signals::Unrestricted,
            };
            assert_eq!(
                Confinement::builder()
                    .landlock(ll(abi, Scopes::default()))
                    .seccomp(unrestricted)
                    .build()
                    .unwrap_err(),
                PolicyError::SignalsUnconfined { abi }
            );
            assert!(
                Confinement::builder()
                    .landlock(ll(abi, Scopes::default()))
                    .seccomp(SELF_ONLY)
                    .build()
                    .is_ok()
            );
            assert_eq!(
                Confinement::builder()
                    .landlock(ll(abi, Scopes::ALL))
                    .seccomp(SELF_ONLY)
                    .build()
                    .unwrap_err(),
                PolicyError::ScopesNeedAbi6 { abi }
            );
        }
    }

    #[test]
    fn abi_6_scoping_only_signals_still_needs_seccomp_for_sockets() {
        let signal_only = Scopes {
            abstract_unix_socket: false,
            signal: true,
        };
        assert_eq!(
            Confinement::builder()
                .landlock(ll(6, signal_only))
                .build()
                .unwrap_err(),
            PolicyError::SocketsUnconfined { abi: 6 }
        );
    }

    #[test]
    fn rulesets_outside_the_known_abis_are_refused() {
        for abi in [0, 1, 2, 3] {
            assert_eq!(
                Confinement::builder()
                    .landlock(ll(abi, Scopes::default()))
                    .seccomp(SELF_ONLY)
                    .build()
                    .unwrap_err(),
                PolicyError::AbiTooOld { abi }
            );
        }
        assert_eq!(
            Confinement::builder()
                .landlock(ll(7, Scopes::ALL))
                .build()
                .unwrap_err(),
            PolicyError::AbiUnknown { abi: 7 }
        );
    }

    #[test]
    fn without_landlock_anything_goes() {
        // The bubblewrap path: namespaces isolate, seccomp is optional.
        assert!(Confinement::builder().build().is_ok());
        assert!(Confinement::builder().seccomp(SELF_ONLY).build().is_ok());
    }
}

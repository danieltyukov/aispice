# Running programs aispice does not trust

Some programs aispice starts may run code it did not write: xschem evaluates Tcl from the files it opens, and ngspice loads compiled Verilog-A (OSDI) and co-simulation libraries built from a user's sources. aispice runs those in a sandbox (`aispice_sim::sandbox`) on Linux, and refuses to run a confined program on any other system rather than run it unconfined.

KLayout and Magic run no code of a project's, only aispice's scripts and the PDK's decks, but they parse layouts aispice did not write. They run in the same sandbox under `Denylist`, as defense in depth: each run reads the system, the program's install, the PDK and its inputs, and writes only its private folder and the folders its results go to. Where the machine has no sandbox (any system but Linux, or a Linux without bubblewrap or Landlock ABI 4) they run as before, unconfined in their private folder, and the layout tools say so in their output; `aispice doctor` shows which sandbox they get. IHP's pcell library asks psutil for the names of every process up its chain to decide whether it runs inside KLayout, and fails when `/proc` of those processes is not readable, as under Landlock; it also accepts a `KLAYOUT` variable, so aispice sets that and gives KLayout's Python (through `KLAYOUT_PYTHONPATH`) a `psutil` that cannot be imported.

A caller describes the run as a `Plan`: the program and its arguments, its whole environment (nothing of aispice's is inherited), the working folder, the files and folders it may read, the other programs it may execute, the folders it may write, a seccomp profile and resource limits. `Sandbox::select` picks the sandbox this machine gives, and `Sandbox::command` turns the plan into the command to run. A `Sandbox` can only come from `select`.

## The two sandboxes

`select` tries them in this order:

1. **bubblewrap** 0.8 or later, not setuid, when it can make its namespaces here (it runs `true` under `Denylist` and a shell that signals itself under `NativeCode` once to find out, with the same flags and seccomp programs a real run uses). The program gets new user, PID, IPC, UTS and network namespaces (no network at all) and cannot make another user namespace inside (`--disable-userns`, checked with `--assert-userns-disabled`). It sees the system folders, its own program, helpers and `read` paths read-only, its `write` folders read-write, a private `/proc` and `/dev`, and a private 64 MiB `/tmp`. Every path appears at its own place, so arguments and environment are the same as under Landlock. bubblewrap installs the plan's seccomp profile itself (`--add-seccomp-fd`); its PID namespace keeps signals in.
2. **Landlock**, at the kernel's ABI from 4 (Linux 6.7) up to 6 (Linux 6.12), as a hard requirement: a kernel that cannot enforce the ruleset is refused, never used with a weaker one. Everything the ABI knows is handled and only what the plan lists is granted: reading the system, a few system files (`/dev/urandom`, `/dev/zero`, `/proc/meminfo`, the online CPU list), the plan's `read` paths; executing the program, its helpers and the loader; writing plain files and folders (no symlink, socket, fifo or device) in the `write` folders; `/dev/null`; no TCP. At ABI 6 Landlock also scopes signals and abstract UNIX sockets to the sandbox. At ABI 4 and 5 it cannot, so seccomp's signal program does that job (below), and every seccomp profile refuses creating sockets. This is the path on a kernel that restricts unprivileged user namespaces without an AppArmor profile for bubblewrap, which is Ubuntu 24.04's default.
3. Neither: nothing runs, and `select` says why.

Under either sandbox the program:

- keeps no descriptor of aispice's but stdin, stdout and stderr: every other one is marked close-on-exec before it starts, since Landlock does not restrict descriptors opened before it applies (under bubblewrap the seccomp programs are moved to descriptors 3 and up first, and only those survive);
- may write no single file larger than its limit, use no more address space than its limit, and start no more processes and threads than its margin over what its user runs when it starts;
- dies with aispice, leads its own process group, cannot leave it (`setsid` and `setpgid` are refused), and the group is killed with `killpg` when the program exits, times out or is cancelled.

## seccomp

Each profile compiles to programs installed in this order: one answering `clone3` with `ENOSYS` (its flags sit in memory a filter cannot read, so the C library falls back to `clone`, whose flags the profile checks), on x86_64 one refusing every x32 system call (those carry the x86_64 audit architecture, so they would match no rule written for the 64-bit numbers), and the profile itself last, since the allowlist refuses the calls that install filters. A call from another architecture is killed.

- **`Denylist`**, for a large program whose calls are not known in advance (xschem, with Tcl and Tk): everything is allowed except, with `EACCES`, creating or using a socket of any kind (only `socketpair` remains), io_uring, `ptrace` and reading or writing another process, taking its descriptors, the keyring, BPF, perf, userfaultfd, System V shared memory, message queues and semaphores and POSIX message queues (another program of yours, such as an X server's shared memory, is reachable through them without an IPC namespace), mounts, new namespaces through `unshare`, `setns` or `clone`, `setsid` and `setpgid`, changing any file's mode, owner, extended attributes or times (`chmod`, `chown`, `setxattr`, `removexattr`, `utimensat` and their other forms; Landlock up to ABI 6 does not mediate these, so they would reach any file you own), and watching files (`inotify_add_watch`, `fanotify_mark`). Limits, scheduling and priority may change only for the process itself: `prlimit64` and the `sched_set` calls with pid 0, `setpriority` and `ioprio_set` for the calling process only (not its group or your user), since on another process of yours they could slow it, kill it or show which pids exist.
- **`NativeCode`**, for untrusted native code (ngspice loading a user's OSDI or co-simulation library): only what a computational program needs. That means files (Landlock decides which), memory, threads, time, signals (to whom is decided below), waiting for and starting programs (Landlock decides which) and process queries. `fcntl` is limited to descriptor flags, status flags and locks, `ioctl` to asking about a terminal, bytes readable, non-blocking and close-on-exec, `prctl` to naming threads and memory, `prlimit64` to the process itself, and `clone` to calls without a namespace flag. Everything else gets `ENOSYS`, so the C library falls back where it can. The list comes from tracing ngspice 42 on plain decks and on IHP decks that load the PSP model as OSDI, plus the C library's and thread runtimes' common paths. A test runs ngspice that way in every sandbox the machine has.

With **`Signals::SelfOnly`**, a further program lets a signal reach only the process itself and its own group. `NativeCode` always runs with it, in either sandbox and at every Landlock ABI: Landlock's signal scope and bubblewrap's PID namespace keep a signal from arriving, but `kill(-1, 0)` still reports success under the scope, and only seccomp makes it fail. `Denylist` runs with it under Landlock below ABI 6, where nothing else holds signals. `kill`, `tkill`, `tgkill`, `rt_sigqueueinfo` and `rt_tgsigqueueinfo` are checked against the process's pid, which under Landlock is written into the program in `pre_exec`, just before it starts, and under bubblewrap is 2, the program's pid in bubblewrap's PID namespace (below its reaper at 1). There the program does not lead its own group; it reaches its group by 0. The builder refuses `NativeCode` with any other signal policy. `kill(-1)` and `pidfd_send_signal` are refused, and so are the ways to have the kernel signal someone else later (`fcntl` `F_SETOWN`, `F_SETOWN_EX` and `F_SETSIG`; `ioctl` `FIOSETOWN` and `SIOCSPGRP`). A process the program starts may signal the program and the group, but not itself by pid.

## What is not covered

- Nothing caps the total bytes written across many files in a writable folder; the limit is per file.
- Under Landlock a program can still see that files outside its folders exist and read their metadata (`stat`, `access`: size, owner, mode, times), though not their contents. bubblewrap shows it only the folders it binds.
- Executing is limited to the plan's programs and the loader. That keeps a program from starting others by path, but it is not a boundary on its own, since the loader can run any readable binary. seccomp and Landlock's file, network and scope rules are what hold.
- Nothing is granted in the real `/tmp` under Landlock. glibc's `tmpfile()` always makes its file there, whatever `TMPDIR` says, and ngspice calls it at start in every mode. bubblewrap gives each run a private `/tmp`, so it works there; under Landlock the caller has to give the program a `tmpfile()` of its own, such as a preloaded one that makes the file in `TMPDIR` (one of the `write` folders). The sandbox tests run ngspice that way.

## Testing at an older Landlock ABI

A kernel at ABI 6 or later can still enforce a ruleset written against ABI 4 or 5. aispice-sim's own tests run their Landlock checks at ABI 4 as well as at the kernel's. Tests in other crates can turn on the `testing` feature of `aispice-sim` in their dev-dependencies. A test that hands a sandbox to the code under test takes `Sandbox::landlock_at_abi_for_tests(4)`, which changes nothing else. Where `Sandbox::select` is called deep inside the code under test, `Sandbox::use_landlock_abi_for_tests(4)` makes it skip bubblewrap and give Landlock at ABI 4 for the rest of that test process; since that is process-wide, such tests belong in a test binary of their own. Both are refused above the kernel's ABI. No product build has the feature, and CI checks that no normal dependency path of the `aispice` or desktop binary turns it on.

## Using bubblewrap on Ubuntu 24.04

The default kernel refuses bubblewrap its user namespace (`kernel.apparmor_restrict_unprivileged_userns=1`) unless bwrap has an AppArmor profile, so aispice uses Landlock there. To use bubblewrap instead, add a profile that allows the namespace (this needs root), for example in `/etc/apparmor.d/bwrap`:

```
abi <abi/4.0>,
include <tunables/global>
profile bwrap /usr/bin/bwrap flags=(unconfined) {
  userns,
  include if exists <local/bwrap>
}
```

then `sudo apparmor_parser -r /etc/apparmor.d/bwrap`. aispice does not change system configuration; this is yours to decide. CI lifts the restriction on its throwaway runners so both sandboxes are tested there.

//! Kernel sandbox for the `bash` tool (Linux Landlock LSM).
//!
//! Disabled by default for development. Set `IRIS_SECURITY_OPT_IN=1` to confine
//! shell commands to a data-driven filesystem/network policy enforced by the
//! kernel, not by string inspection. That policy grants write access only to the
//! workspace (plus temp dirs and `/dev/null`) and denies all TCP networking;
//! reads and program execution are left unrestricted so the shell and its tools
//! still run.
//!
//! Enforcement happens in the child between `fork` and `exec` via
//! [`CommandExt::pre_exec`]. To stay safe in a multi-threaded parent (a
//! post-`fork` child may only touch async-signal-safe state), the Landlock
//! ruleset is *built in the parent* and the child performs only raw syscalls
//! (`prctl` + `landlock_restrict_self`) with no heap allocation.
//!
//! Scope of the network restriction: Landlock confines TCP `bind`/`connect`
//! only. UDP, raw sockets, and `connect` to already-bound UNIX domain sockets
//! are not covered by the LSM, so this is TCP deny-by-default, not a full
//! network jail.
//! ponytail: TCP-only network confinement; a network namespace (unshare) is the
//! upgrade path if UDP/UNIX-socket egress must also be blocked.
//!
//! Fallback is explicit, never silent: when the kernel lacks Landlock (or it is
//! disabled, or the platform is not Linux) the command runs unconfined and the
//! [`SandboxStatus`] carries a notice that the bash tool surfaces in its output
//! and logs via `tracing`. Delegated workers are stricter: they grant no shared
//! temp directories and refuse to spawn when filesystem confinement is unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "linux")]
use anyhow::Context;

/// Landlock ABI version that first supports TCP network restrictions
/// (`AccessNet::{BindTcp, ConnectTcp}`), added in Linux 6.7.
const NET_ABI: u32 = 4;

/// Data-driven filesystem + network policy for a sandboxed command.
///
/// Reads and execution are intentionally unrestricted; only writes and network
/// access are confined, which is what the bash sandbox needs to enforce while
/// keeping the shell and its tools usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SandboxPolicy {
    /// Paths granted write/create/remove access. Everything else is read-only.
    pub(crate) writable: Vec<PathBuf>,
    /// Outbound TCP ports allowed to `connect`. Empty means deny all.
    pub(crate) allow_tcp_connect: Vec<u16>,
    /// TCP ports allowed to `bind`. Empty means deny all.
    pub(crate) allow_tcp_bind: Vec<u16>,
}

impl SandboxPolicy {
    /// Default bash policy: write access to the workspace and the system temp
    /// directories (so here-docs, `mktemp`, `sed -i`, compilers, etc. keep
    /// working), plus `/dev/null` for redirects. No inbound or outbound TCP.
    /// Reads and execution everywhere are unrestricted.
    pub(crate) fn for_workspace(root: &Path) -> Self {
        Self {
            writable: vec![
                root.to_path_buf(),
                PathBuf::from("/dev/null"),
                // `temp_dir()` honors `$TMPDIR`; `/var/tmp` is the persistent
                // temp dir. Absent paths are skipped when the ruleset is built.
                std::env::temp_dir(),
                PathBuf::from("/var/tmp"),
            ],
            allow_tcp_connect: Vec::new(),
            allow_tcp_bind: Vec::new(),
        }
    }

    fn for_strict_workspace(root: &Path) -> Self {
        Self {
            writable: vec![root.to_path_buf(), PathBuf::from("/dev/null")],
            allow_tcp_connect: Vec::new(),
            allow_tcp_bind: Vec::new(),
        }
    }
}

/// What the kernel can actually enforce for a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SandboxStatus {
    /// Sandbox intentionally disabled.
    Disabled,
    /// Filesystem and network policy both enforced.
    Enforced,
    /// Filesystem enforced, but the kernel's Landlock ABI is too old to confine
    /// the network, so TCP is not restricted.
    FilesystemOnly,
    /// Nothing enforced; the command ran unconfined. `reason` says why.
    Unavailable { reason: String },
}

impl SandboxStatus {
    pub(crate) fn filesystem_enforced(&self) -> bool {
        matches!(self, Self::Enforced | Self::FilesystemOnly)
    }

    /// A one-line notice to surface when the sandbox is not fully enforced.
    /// `None` on the fully-[`Enforced`](SandboxStatus::Enforced) happy path so
    /// normal output carries no noise.
    pub(crate) fn notice(&self) -> Option<String> {
        match self {
            Self::Disabled | Self::Enforced => None,
            Self::FilesystemOnly => Some(
                "sandbox: filesystem confined, but this kernel's Landlock ABI is \
                 too old to restrict TCP network access"
                    .to_string(),
            ),
            Self::Unavailable { reason } => Some(format!(
                "sandbox: not enforced ({reason}); command ran unconfined"
            )),
        }
    }
}

/// Map a detected Landlock ABI version to the enforcement we can guarantee.
/// `None` means Landlock is unsupported on this kernel/platform.
fn decide(abi: Option<u32>) -> SandboxStatus {
    match abi {
        None => SandboxStatus::Unavailable {
            reason: "Landlock LSM unavailable on this kernel/platform".to_string(),
        },
        Some(v) if v >= NET_ABI => SandboxStatus::Enforced,
        Some(_) => SandboxStatus::FilesystemOnly,
    }
}

/// Whether this platform ships a kernel sandbox backend for the shell at all.
///
/// Only Linux has one (Landlock). On macOS and Windows the shell always runs
/// unconfined regardless of opt-in, so the approval UI surfaces that posture
/// instead of implying confinement that does not exist. This reports platform
/// capability, not whether confinement is currently opted in or enforced.
pub(crate) fn platform_can_sandbox() -> bool {
    cfg!(target_os = "linux")
}

pub(crate) fn policy_for_current_agent(root: &Path) -> SandboxPolicy {
    if crate::tools::path::workspace_confinement_required() {
        SandboxPolicy::for_strict_workspace(root)
    } else {
        SandboxPolicy::for_workspace(root)
    }
}

pub(crate) fn require_for_current_agent(status: &SandboxStatus) -> anyhow::Result<()> {
    if crate::tools::path::workspace_confinement_required() && !status.filesystem_enforced() {
        anyhow::bail!("delegated shell requires kernel-enforced workspace confinement")
    }
    Ok(())
}

/// Detect + apply: confine `command` to `policy` using the running kernel's
/// Landlock support, returning what was actually enforced.
pub(crate) fn confine(command: &mut Command, policy: &SandboxPolicy) -> SandboxStatus {
    if !crate::tools::path::restrictions_enabled() {
        return SandboxStatus::Disabled;
    }
    apply(command, policy, detect_abi())
}

/// Apply `policy` to `command` for a given detected Landlock `abi`.
///
/// `abi` is injected (rather than always detected) so tests can simulate an
/// unsupported kernel. When `abi` is `None` the command is left unconfined
/// (graceful fallback) and the returned status records why. If the ruleset
/// cannot be built on a supposedly-supported kernel, the command is left
/// unconfined and the status downgrades to [`Unavailable`](SandboxStatus::Unavailable)
/// so the failure is surfaced rather than silently dropped.
pub(crate) fn apply(
    command: &mut Command,
    policy: &SandboxPolicy,
    abi: Option<u32>,
) -> SandboxStatus {
    let status = decide(abi);
    if abi.is_none() {
        return status;
    }
    #[cfg(target_os = "linux")]
    match install(command, policy) {
        Ok(()) => status,
        Err(error) => SandboxStatus::Unavailable {
            reason: format!("failed to build Landlock ruleset: {error:#}"),
        },
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (command, policy);
        status
    }
}

/// Build the Landlock ruleset in the parent and arrange for the child to
/// enforce it between `fork` and `exec`.
#[cfg(target_os = "linux")]
fn install(command: &mut Command, policy: &SandboxPolicy) -> anyhow::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    // Take ownership of the ruleset's fd directly (it is close-on-exec, which is
    // correct: the Landlock domain persists across `exec` and the fd is only
    // needed before it). A `None` here means the kernel produced no enforceable
    // domain, which should not happen once Landlock support is detected.
    let fd: OwnedFd = Option::<OwnedFd>::from(build_ruleset(policy)?)
        .context("Landlock ruleset has no enforceable domain")?;

    // Defensive: if the parent had closed its own std streams, this fd could be
    // 0/1/2. The child's stdio setup (dup2 for Stdio::null/piped) runs before
    // pre_exec and would clobber that fd, making `landlock_restrict_self` fail
    // with EBADF. Relocate it above the std range (keeping CLOEXEC) so only the
    // fd number changes; the pre_exec contract stays allocation-free and
    // async-signal-safe. No unit test: it requires the parent to close fds 0-2,
    // which would break the test harness itself.
    let fd: OwnedFd = if fd.as_raw_fd() <= 2 {
        let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to relocate Landlock ruleset fd above std range");
        }
        // SAFETY: fcntl(F_DUPFD_CLOEXEC) returned a fresh owned fd >= 3; the
        // original `fd` is dropped (closed) at the end of this block.
        unsafe { OwnedFd::from_raw_fd(raw) }
    } else {
        fd
    };

    // SAFETY: the closure runs in the forked child before `exec` and performs
    // only async-signal-safe work: two syscalls and no heap allocation, locks,
    // or non-reentrant calls. Building the ruleset (which does allocate) already
    // happened in the parent above.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let rc = libc::syscall(
                libc::SYS_landlock_restrict_self,
                fd.as_raw_fd() as libc::c_long,
                0 as libc::c_long,
            );
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// Construct a Landlock ruleset from `policy` using best-effort compatibility so
/// it degrades cleanly on older kernels (the [`SandboxStatus`] reports what was
/// actually enforceable). Handles only write and TCP access; reads and program
/// execution stay unrestricted so the shell and its tools run.
#[cfg(target_os = "linux")]
fn build_ruleset(policy: &SandboxPolicy) -> anyhow::Result<landlock::RulesetCreated> {
    use landlock::{
        ABI, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, PathBeneath, PathFd, Ruleset,
        RulesetAttr, RulesetCreatedAttr,
    };

    let abi = ABI::V4;
    let fs_write = AccessFs::from_write(abi);
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(fs_write)?
        .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)?
        .create()?;

    for path in &policy.writable {
        // A path that does not exist cannot be granted write access; skip it
        // rather than fail the whole ruleset.
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset.add_rule(PathBeneath::new(fd, fs_write))?;
        }
    }
    for &port in &policy.allow_tcp_connect {
        ruleset = ruleset.add_rule(NetPort::new(port, AccessNet::ConnectTcp))?;
    }
    for &port in &policy.allow_tcp_bind {
        ruleset = ruleset.add_rule(NetPort::new(port, AccessNet::BindTcp))?;
    }
    Ok(ruleset)
}

/// Detect the kernel's supported Landlock ABI version, or `None` if Landlock is
/// unavailable (old kernel, disabled, or non-Linux).
#[cfg(target_os = "linux")]
fn detect_abi() -> Option<u32> {
    // `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` returns
    // the supported ABI version (>= 1) without creating anything, or -1 on an
    // unsupported/disabled kernel.
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    // SAFETY: FFI to a side-effect-free version query; null attr pointer and
    // zero size are the documented form of the version request.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    (rc > 0).then_some(rc as u32)
}

#[cfg(not(target_os = "linux"))]
fn detect_abi() -> Option<u32> {
    None
}

/// Test-only access to kernel Landlock detection so end-to-end bash tests can
/// skip cleanly on kernels without Landlock.
#[cfg(test)]
pub(crate) fn detect_abi_for_test() -> Option<u32> {
    detect_abi()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_outside_path(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("iris-sandbox-{tag}-{nanos}-{seq}"))
    }

    /// A path the default policy never grants write access to (the temp dirs
    /// *are* writable now, so escapes must target `$HOME`). `None` if `$HOME`
    /// is unset or itself sits under a writable temp dir.
    fn forbidden_path(tag: &str) -> Option<PathBuf> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let home = PathBuf::from(std::env::var_os("HOME")?);
        if home.starts_with(std::env::temp_dir()) {
            return None;
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        Some(home.join(format!(".iris-sandbox-{tag}-{nanos}-{seq}")))
    }

    fn workspace() -> PathBuf {
        let dir = unique_outside_path("ws");
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    /// Run `bash -c script` with `policy` applied for `abi`, returning
    /// (combined output, exit-success).
    fn run(script: &str, policy: &SandboxPolicy, abi: Option<u32>) -> (String, bool) {
        let mut cmd = Command::new("/bin/bash");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply(&mut cmd, policy, abi);
        let mut child = cmd.spawn().expect("spawn bash");
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        let mut err = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        let status = child.wait().unwrap();
        (format!("{out}{err}"), status.success())
    }

    #[test]
    fn delegated_policy_excludes_shared_temp_and_requires_filesystem_enforcement() {
        let ws = workspace();
        crate::tools::path::with_restrictions(Some(true), || {
            let policy = policy_for_current_agent(&ws);
            assert_eq!(
                policy.writable,
                vec![ws.clone(), PathBuf::from("/dev/null")]
            );
            assert!(
                require_for_current_agent(&SandboxStatus::Unavailable {
                    reason: "unsupported".to_string(),
                })
                .is_err()
            );
            require_for_current_agent(&SandboxStatus::FilesystemOnly).unwrap();
        });
        std::fs::remove_dir_all(ws).ok();
    }

    #[test]
    fn decide_maps_abi_to_status() {
        assert!(matches!(decide(None), SandboxStatus::Unavailable { .. }));
        assert_eq!(decide(Some(1)), SandboxStatus::FilesystemOnly);
        assert_eq!(decide(Some(3)), SandboxStatus::FilesystemOnly);
        assert_eq!(decide(Some(4)), SandboxStatus::Enforced);
        assert_eq!(decide(Some(7)), SandboxStatus::Enforced);
    }

    #[test]
    fn notice_only_when_not_fully_enforced() {
        assert!(SandboxStatus::Enforced.notice().is_none());
        assert!(
            SandboxStatus::FilesystemOnly
                .notice()
                .unwrap()
                .contains("network")
        );
        assert!(
            SandboxStatus::Unavailable { reason: "x".into() }
                .notice()
                .unwrap()
                .contains("unconfined")
        );
    }

    #[test]
    fn blocks_write_outside_workspace_at_kernel_level() {
        let abi = detect_abi();
        assert!(abi.is_some(), "test kernel must support Landlock");
        let ws = workspace();
        let policy = SandboxPolicy::for_workspace(&ws);
        let Some(outside) = forbidden_path("escape") else {
            std::fs::remove_dir_all(&ws).ok();
            return;
        };
        let script = format!("echo bad > {}", outside.display());

        let (_out, ok) = run(&script, &policy, abi);

        assert!(!outside.exists(), "write outside workspace was not blocked");
        assert!(!ok, "blocked write should make the command fail");
        std::fs::remove_dir_all(&ws).ok();
        std::fs::remove_file(&outside).ok();
    }

    #[test]
    fn allows_read_write_inside_workspace() {
        let abi = detect_abi();
        let ws = workspace();
        let policy = SandboxPolicy::for_workspace(&ws);
        let inside = ws.join("inside.txt");
        let script = format!("echo ok > {p} && cat {p}", p = inside.display());

        let (out, ok) = run(&script, &policy, abi);

        assert!(ok, "workspace write/read should succeed: {out}");
        assert!(out.contains("ok"));
        assert!(inside.exists());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn allows_tempdir_writes_so_shell_stays_usable() {
        // Regression: blocking the temp dirs breaks here-docs, `mktemp`, and
        // tools that write scratch files. Those must still work confined.
        let abi = detect_abi();
        let ws = workspace();
        let policy = SandboxPolicy::for_workspace(&ws);
        let script = "f=$(mktemp) && echo scratch > \"$f\" && cat \"$f\" && \
                      cat <<EOF\nheredoc-ok\nEOF";

        let (out, ok) = run(script, &policy, abi);

        assert!(ok, "temp writes/here-docs should work under sandbox: {out}");
        assert!(out.contains("scratch"));
        assert!(out.contains("heredoc-ok"));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn denies_tcp_connect_by_default() {
        let abi = detect_abi();
        assert!(abi.is_some(), "test kernel must support Landlock");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let ws = workspace();
        let policy = SandboxPolicy::for_workspace(&ws);
        let script = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo connected || echo refused");

        // Unconfined first: proves /dev/tcp + listener actually work here.
        let (base_out, _) = run(&script, &policy, None);
        assert!(
            base_out.contains("connected"),
            "baseline TCP connect did not work in this env: {base_out}"
        );

        // Confined: the kernel must block the connect.
        let (out, _ok) = run(&script, &policy, abi);
        assert!(
            !out.contains("connected"),
            "sandboxed TCP connect was not blocked: {out}"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn unavailable_abi_runs_command_unconfined() {
        // Graceful fallback: when Landlock is unavailable the command still runs
        // (here a write outside the workspace succeeds) and the status says so.
        let ws = workspace();
        let policy = SandboxPolicy::for_workspace(&ws);
        let Some(outside) = forbidden_path("fallback") else {
            std::fs::remove_dir_all(&ws).ok();
            return;
        };
        let script = format!("echo through > {}", outside.display());

        let mut cmd = Command::new("/bin/bash");
        cmd.arg("-c")
            .arg(&script)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let status = apply(&mut cmd, &policy, None);
        let ok = cmd.status().unwrap().success();

        assert!(ok, "fallback should run the command");
        assert!(outside.exists(), "fallback runs unconfined");
        assert!(matches!(status, SandboxStatus::Unavailable { .. }));
        assert!(status.notice().is_some());
        std::fs::remove_dir_all(&ws).ok();
        std::fs::remove_file(&outside).ok();
    }
}

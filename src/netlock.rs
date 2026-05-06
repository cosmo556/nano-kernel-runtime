// =============================================================================
// NKR netlock — Inter-process serialization of netlink/iptables operations
// =============================================================================
//
// Creating TAPs, joining them to bridges and applying iptables/ebtables/tc
// rules in parallel (N `nkr run` processes spawned by `nkr compose up`)
// produces races in rtnetlink and the kernel's xt_* tables. Symptoms include:
//
//   - "RTNETLINK answers: File exists" when creating an already-pending tap/bridge
//   - Duplicated iptables rules when two simultaneous `-C` don't find the
//     rule and both do `-A`
//   - Classic ebtables that doesn't support xtables --wait and fails under load
//
// A std::sync::Mutex isn't enough because each VM runs in a different process
// (spawned by `nkr compose up`). We use flock(2) on a shared file.
//
// Usage:
//
//   let _guard = NetLock::acquire("tap-create");
//   // ... create tap, join to bridge, set up L2 isolation ...
//   // guard is released on scope exit
//
// The lock is automatically released on drop (RAII). If the process dies, the
// kernel releases the flock.
// =============================================================================

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;

const LOCK_PATH: &str = "/tmp/nkr-netlink.lock";

pub struct NetLock {
    file: Option<File>,
}

impl NetLock {
    /// Acquires the exclusive lock. Blocks until available.
    /// If the lock file can't be opened, degrades silently
    /// (no-op guard) — an occasional race is preferable to a boot crash.
    pub fn acquire(scope: &'static str) -> Self {
        let file = match OpenOptions::new()
            .create(true).read(true).write(true).truncate(false)
            .open(LOCK_PATH)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[NKR-NETLOCK] WARN: no se pudo abrir {}: {} (scope={}, sin serialización)",
                    LOCK_PATH, e, scope);
                return NetLock { file: None };
            }
        };
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            eprintln!("[NKR-NETLOCK] WARN: flock(LOCK_EX) falló en scope={}: {} — continuando sin lock",
                scope, std::io::Error::last_os_error());
            return NetLock { file: None };
        }
        NetLock { file: Some(file) }
    }
}

impl Drop for NetLock {
    fn drop(&mut self) {
        if let Some(f) = &self.file {
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN); }
        }
    }
}

/// Returns a `Command` for `iptables` with `-w 5` pre-loaded.
///
/// `-w N` waits up to N seconds for the kernel's `xtables` lock (different
/// from NKR's inter-process netlock). Without `-w`, if another host process
/// (admin, fail2ban, docker, etc.) holds the xtables lock, `iptables -A/-C`
/// exits 4 and we lose the rule. With `-w 5` it retries for 5s.
///
/// Supported since iptables 1.4.20 (Ubuntu 16.04+). The flag must come BEFORE
/// the table/chain arguments, so we inject it here and the call sites pass
/// the rest via normal `.args(...)`.
pub fn iptables() -> std::process::Command {
    let mut cmd = std::process::Command::new("iptables");
    cmd.args(["-w", "5"]);
    cmd
}


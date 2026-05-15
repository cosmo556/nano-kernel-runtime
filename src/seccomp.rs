// =============================================================================
// NKR Seccomp Jailer — Restrictive BPF filter for the NKR process
// =============================================================================
//
// Installs a SECCOMP_MODE_FILTER filter using prctl() directly (no external
// deps). The filter allows only the syscalls needed during the vCPU loop,
// reducing the hypervisor's attack surface.
//
// Silent degradation: if prctl() fails (old kernel, missing permissions),
// a warning is emitted and NKR continues without the filter.
//
// IMPORTANT: Install AFTER thread::spawn (RX thread from net.rs), but
// BEFORE vcpu.run(). Existing threads inherit the filter.
// =============================================================================

use libc::c_int;

const SECCOMP_MODE_FILTER: c_int = 2;
const PR_SET_NO_NEW_PRIVS: c_int = 38;
const PR_SET_SECCOMP: c_int = 22;

// Result: allow or kill the process
const SECCOMP_RET_ALLOW: u32 = 0x7FFF_0000;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;

// BPF opcodes
const BPF_LD:  u16 = 0x00;
const BPF_W:   u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_K:   u16 = 0x00;
const BPF_RET: u16 = 0x06;

#[repr(C)]
struct SockFilter {
    code: u16,
    jt:   u8,
    jf:   u8,
    k:    u32,
}

#[repr(C)]
struct SockFprog {
    len:    u16,
    _pad:   u16,
    filter: *const SockFilter,
}

/// Syscalls allowed on x86_64 during NKR's vCPU loop.
/// Includes io_uring syscalls (Features B) even if inactive —
/// having them on the allowlist is harmless if unused.
/// 57=fork, 58=vfork, 59=execve, 322=execveat: needed for Command::new()
/// in post-shutdown cleanup (iptables, ip, ebtables, cgroup rmdir).
///
/// **Modern glibc additions (v1.6.7, fix 2026-05-15)**:
///   435=clone3       — glibc 2.34+ uses clone3 internally for thread spawn.
///                      Bug observado: el VMM PID 812313 (intech-devp) murió por
///                      SECCOMP_RET_KILL_PROCESS al hacer clone3 → la VM se cayó
///                      silenciosamente y arrastró el tap nkr-c2-tap4. Sin
///                      whitelist no hay manera segura de spawnear threads en
///                      sistemas modernos. Confirmado por audit log:
///                      `audit: pid=812313 comm="nkr" syscall=435 sig=31 code=0x80000000`
///   437=openat2       — glibc 2.33+ open() puede usar openat2 (RESOLVE_BENEATH etc.)
///   439=faccessat2    — glibc usa faccessat2 cuando está disponible
///   441=epoll_pwait2  — alternativa moderna a epoll_pwait
///   447=memfd_secret  — eventual; no se usa hoy pero futuro-proof
///   452=fchmodat2     — glibc nueva
const ALLOWED_SYSCALLS: &[u32] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 186, 202, 217, 218, 228, 231, 232, 233, 257, 258, 263, 289, 290, 291, 293, 302, 318, 322, 332, 334, 425, 426, 427, 435, 437, 439, 441, 452];

/// Builds and installs the seccomp filter using raw BPF.
/// Returns Ok(()) if installed, Err if not possible (silent degradation).
pub fn install_seccomp_filter() -> Result<(), Box<dyn std::error::Error>> {
    // PR_SET_NO_NEW_PRIVS: required before installing a seccomp filter without CAP_SYS_ADMIN
    let ret = unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1usize, 0usize, 0usize, 0usize) };
    if ret != 0 {
        return Err(format!(
            "prctl PR_SET_NO_NEW_PRIVS falló: {}",
            std::io::Error::last_os_error()
        ).into());
    }

    // Build the BPF program:
    // 1. Load syscall number (u32 at offset 0 of seccomp_data)
    // 2. For each allowed syscall: JEQ → ALLOW (jump to RET_ALLOW at the end)
    // 3. At end of list: RET KILL_PROCESS
    //
    // Program structure:
    //   [LD syscall_nr]
    //   [JEQ allowed[0], jt=offset_to_allow, jf=0]
    //   [JEQ allowed[1], jt=offset_to_allow, jf=0]
    //   ...
    //   [RET KILL_PROCESS]
    //   [RET ALLOW]          ← target of all JEQs
    let n = ALLOWED_SYSCALLS.len();
    let mut prog: Vec<SockFilter> = Vec::with_capacity(n + 3);

    // Instruction 0: load syscall number from seccomp_data.nr (offset 0)
    prog.push(SockFilter { code: BPF_LD | BPF_W | BPF_ABS, jt: 0, jf: 0, k: 0 });

    // Instructions 1..n: JEQ for each allowed syscall
    // jt = forward jump to RET_ALLOW
    // jf = 0 = fall through to next JEQ
    // RET_ALLOW is at position (1 + n + 1) = n + 2 from program start
    // From instruction i (1-based), the distance to RET_ALLOW is: (n - i + 1)
    for (i, &nr) in ALLOWED_SYSCALLS.iter().enumerate() {
        let remaining = (n - i - 1) as u8; // remaining JEQ instructions after this one
        // jt: jump 'remaining + 1' instructions forward to reach RET_ALLOW
        // (remaining JEQs + 1 RET_KILL = remaining+1 instructions to skip)
        prog.push(SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: remaining + 1, // if equal → jump to RET_ALLOW
            jf: 0,             // if not → next instruction
            k: nr,
        });
    }

    // Instruction n+1: RET KILL_PROCESS (syscall not allowed)
    prog.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: SECCOMP_RET_KILL_PROCESS });

    // Instruction n+2: RET ALLOW (target of all successful JEQs)
    prog.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: SECCOMP_RET_ALLOW });

    let fprog = SockFprog {
        len:    prog.len() as u16,
        _pad:   0,
        filter: prog.as_ptr(),
    };

    let ret = unsafe {
        libc::prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER as usize,
            &fprog as *const SockFprog as usize,
            0usize,
            0usize,
        )
    };

    if ret != 0 {
        return Err(format!(
            "prctl PR_SET_SECCOMP falló: {}",
            std::io::Error::last_os_error()
        ).into());
    }

    eprintln!("[NKR] Seccomp: filtro instalado ({} syscalls permitidas)", n);
    Ok(())
}

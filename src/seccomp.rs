// =============================================================================
// NKR Seccomp Jailer — Filtro BPF restrictivo para el proceso NKR
// =============================================================================
//
// Instala un filtro SECCOMP_MODE_FILTER usando prctl() directamente (sin deps
// externos). El filtro permite únicamente las syscalls necesarias durante el
// bucle del vCPU, reduciendo la superficie de ataque del hipervisor.
//
// Degradación silenciosa: si prctl() falla (kernel antiguo, falta de permisos)
// se emite un warning y NKR continúa sin el filtro.
//
// IMPORTANTE: Instalar DESPUÉS de thread::spawn (hilo RX de net.rs), pero
// ANTES de vcpu.run(). Los hilos existentes heredan el filtro.
// =============================================================================

use libc::c_int;

const SECCOMP_MODE_FILTER: c_int = 2;
const PR_SET_NO_NEW_PRIVS: c_int = 38;
const PR_SET_SECCOMP: c_int = 22;

// Resultado: permitir o matar el proceso
const SECCOMP_RET_ALLOW: u32 = 0x7FFF_0000;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;

// Opcodes BPF
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

/// Syscalls permitidas en x86_64 durante el bucle del vCPU NKR.
/// Incluye las syscalls de io_uring (Features B) aunque no estén activas —
/// tenerlas en la allowlist es inocuo si no se usan.
/// 57=fork, 58=vfork, 59=execve, 322=execveat: necesarios para Command::new()
/// en el cleanup post-shutdown (iptables, ip, ebtables, rmdir de cgroups).
const ALLOWED_SYSCALLS: &[u32] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 186, 202, 217, 218, 228, 231, 232, 233, 257, 258, 263, 289, 290, 291, 293, 302, 318, 322, 332, 334, 425, 426, 427];

/// Construye e instala el filtro seccomp usando BPF raw.
/// Devuelve Ok(()) si se instaló, Err si no fue posible (degradación silenciosa).
pub fn install_seccomp_filter() -> Result<(), Box<dyn std::error::Error>> {
    // PR_SET_NO_NEW_PRIVS: requerido antes de instalar un filtro seccomp sin CAP_SYS_ADMIN
    let ret = unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1usize, 0usize, 0usize, 0usize) };
    if ret != 0 {
        return Err(format!(
            "prctl PR_SET_NO_NEW_PRIVS falló: {}",
            std::io::Error::last_os_error()
        ).into());
    }

    // Construir el programa BPF:
    // 1. Cargar el número de syscall (u32 en offset 0 de seccomp_data)
    // 2. Para cada syscall permitida: JEQ → ALLOW (saltar al RET_ALLOW al final)
    // 3. Al final del listado: RET KILL_PROCESS
    //
    // Estructura del programa:
    //   [LD syscall_nr]
    //   [JEQ allowed[0], jt=offset_to_allow, jf=0]
    //   [JEQ allowed[1], jt=offset_to_allow, jf=0]
    //   ...
    //   [RET KILL_PROCESS]
    //   [RET ALLOW]          ← destino de todos los JEQ
    let n = ALLOWED_SYSCALLS.len();
    let mut prog: Vec<SockFilter> = Vec::with_capacity(n + 3);

    // Instrucción 0: cargar syscall number desde seccomp_data.nr (offset 0)
    prog.push(SockFilter { code: BPF_LD | BPF_W | BPF_ABS, jt: 0, jf: 0, k: 0 });

    // Instrucciones 1..n: JEQ para cada syscall permitida
    // jt = salto hacia adelante hasta RET_ALLOW
    // jf = 0 = caer al siguiente JEQ
    // El RET_ALLOW está en posición (1 + n + 1) = n + 2 desde el inicio del programa
    // Desde la instrucción i (base 1), la distancia a RET_ALLOW es: (n - i + 1)
    for (i, &nr) in ALLOWED_SYSCALLS.iter().enumerate() {
        let remaining = (n - i - 1) as u8; // instrucciones JEQ restantes después de esta
        // jt: saltar 'remaining + 1' instrucciones adelante para llegar a RET_ALLOW
        // (remaining JEQs + 1 RET_KILL = remaining+1 instrucciones que saltar)
        prog.push(SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: remaining + 1, // si iguales → salta a RET_ALLOW
            jf: 0,             // si distintos → siguiente instrucción
            k: nr,
        });
    }

    // Instrucción n+1: RET KILL_PROCESS (syscall no permitida)
    prog.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: SECCOMP_RET_KILL_PROCESS });

    // Instrucción n+2: RET ALLOW (destino de todos los JEQ exitosos)
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

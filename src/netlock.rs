// =============================================================================
// NKR netlock — Serialización inter-proceso de operaciones netlink/iptables
// =============================================================================
//
// Crear TAPs, unirlos a bridges y aplicar reglas iptables/ebtables/tc en
// paralelo (N procesos `nkr run` spawneados por `nkr compose up`) produce
// carreras en rtnetlink y tablas xt_* del kernel. Los síntomas son:
//
//   - "RTNETLINK answers: File exists" al crear un tap/bridge ya pendiente
//   - Reglas iptables duplicadas cuando dos `-C` simultáneos no encuentran la
//     regla y ambos hacen `-A`
//   - ebtables clásico que no soporta xtables --wait y falla bajo carga
//
// Un std::sync::Mutex no sirve porque cada VM corre en un proceso distinto
// (spawneado por `nkr compose up`). Usamos flock(2) sobre un archivo común.
//
// Uso:
//
//   let _guard = NetLock::acquire("tap-create");
//   // ... crear tap, unir al bridge, setup aislamiento L2 ...
//   // guard se libera al salir del scope
//
// El lock se libera automáticamente al drop (RAII). Si el proceso muere, el
// kernel libera el flock.
// =============================================================================

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;

const LOCK_PATH: &str = "/tmp/nkr-netlink.lock";

pub struct NetLock {
    file: Option<File>,
}

impl NetLock {
    /// Adquiere el lock exclusivo. Bloquea hasta que esté disponible.
    /// Si no se puede abrir el archivo de lock, degrada silenciosamente
    /// (guard no-op) — preferible race ocasional antes que crash del boot.
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

/// Retorna un `Command` para `iptables` con `-w 5` pre-cargado.
///
/// `-w N` espera hasta N segundos por el `xtables` lock del kernel (diferente
/// del netlock inter-proceso de NKR). Sin `-w`, si otro proceso del host
/// (admin, fail2ban, docker, etc.) tiene el xtables lock, `iptables -A/-C`
/// sale con exit 4 y nos quedamos sin regla. Con `-w 5` reintenta durante 5s.
///
/// Soportado desde iptables 1.4.20 (Ubuntu 16.04+). El flag debe ir ANTES de
/// los argumentos de tabla/chain, por eso lo inyectamos acá y los call sites
/// pasan el resto con `.args(...)` normal.
pub fn iptables() -> std::process::Command {
    let mut cmd = std::process::Command::new("iptables");
    cmd.args(["-w", "5"]);
    cmd
}


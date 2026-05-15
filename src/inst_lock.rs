// =============================================================================
// NKR InstanceLock — flock per-(cell, nkr_name)
// =============================================================================
//
// Serializa operaciones que mutan el estado de UNA instancia (start, stop,
// restart, delete, reload, balloon-active) entre TODOS los actores: el daemon
// vía IPC (panel), el CLI `nkr stop`/`nkr restart` operando directo en state,
// y el watchdog interno.
//
// Patrón: flock(LOCK_EX) sobre `/run/nkr/instances/<cell>_<name>.lock`.
// Survive daemon restart (kernel libera el flock cuando el proceso muere).
//
// Closes las siguientes races identificadas en el audit 2026-05-15:
//   C6 — Watchdog dispara restart de VM que está siendo nkr stop-eada manual.
//   I6 — delete_instance libera registry ID antes de remove_dir_all → un
//        create con el mismo nombre puede colarse en el gap.
//   Race start-vs-stop concurrente (panel + CLI operator).
//
// Diseño:
//   acquire()       — bloquea hasta obtener el lock (operaciones del operador)
//   try_acquire()   — no-blocking, devuelve None si ya tomado (watchdog,
//                     reload — preferimos skip a esperar)
//
// El RAII lock cierra el file en Drop → flock(2) auto-release. No hay manual
// unlock; el scope decide la duración.
// =============================================================================

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

const LOCK_DIR: &str = "/run/nkr/instances";

fn lock_path(cell: &str, nkr_name: &str) -> PathBuf {
    PathBuf::from(LOCK_DIR).join(format!("{}_{}.lock", cell, nkr_name))
}

fn ensure_lock_dir() -> std::io::Result<()> {
    fs::create_dir_all(LOCK_DIR)?;
    // Permisos 0755 — el daemon corre como root, los CLI commands también
    // suelen correr como root. No es secret data, solo coordinación.
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(LOCK_DIR, fs::Permissions::from_mode(0o755));
    Ok(())
}

/// RAII guard. Mientras está vivo, el flock está tomado. Drop = close = unlock.
pub struct InstanceLock {
    _file: fs::File,
    cell: String,
    name: String,
}

impl InstanceLock {
    /// Adquiere flock EXCLUSIVO bloqueando hasta obtenerlo. Usar para
    /// operaciones del operador/panel donde es legítimo esperar.
    pub fn acquire(cell: &str, nkr_name: &str) -> Result<Self, Box<dyn std::error::Error>> {
        ensure_lock_dir()?;
        let path = lock_path(cell, nkr_name);
        let file = fs::OpenOptions::new()
            .create(true).read(true).write(true).truncate(false)
            .open(&path)?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(format!("flock(LOCK_EX) {} falló: {}",
                path.display(), std::io::Error::last_os_error()).into());
        }
        eprintln!("[NKR-INSTLOCK] acquired {} (cell={}, name={})",
            path.display(), cell, nkr_name);
        Ok(Self { _file: file, cell: cell.to_string(), name: nkr_name.to_string() })
    }

    /// No-blocking. Retorna `Ok(None)` si el lock ya está tomado por otro
    /// proceso/thread → caller debe abortar limpio (no esperar). Usar para
    /// el watchdog y para reload/balloon (preferimos skip a competir).
    pub fn try_acquire(
        cell: &str,
        nkr_name: &str,
    ) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        ensure_lock_dir()?;
        let path = lock_path(cell, nkr_name);
        let file = fs::OpenOptions::new()
            .create(true).read(true).write(true).truncate(false)
            .open(&path)?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                // Ya está tomado por otro — limpio.
                return Ok(None);
            }
            return Err(format!("flock(LOCK_EX|NB) {} falló: {}", path.display(), err).into());
        }
        eprintln!("[NKR-INSTLOCK] try_acquired {} (cell={}, name={})",
            path.display(), cell, nkr_name);
        Ok(Some(Self { _file: file, cell: cell.to_string(), name: nkr_name.to_string() }))
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // close(_file) llama flock(LOCK_UN) implícitamente — auto-release.
        eprintln!("[NKR-INSTLOCK] released (cell={}, name={})", self.cell, self.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_acquire_returns_none_when_held() {
        // Crear directorio en tmpfs no-canonical para no chocar con producción.
        // Usamos un nombre random per test.
        let cell = "test_lock_cell";
        let name = format!("test_lock_{}", std::process::id());
        let lock1 = InstanceLock::acquire(cell, &name).expect("primer lock");
        let lock2 = InstanceLock::try_acquire(cell, &name).expect("ok call");
        assert!(lock2.is_none(), "segundo try_acquire debería retornar None");
        drop(lock1);
        let lock3 = InstanceLock::try_acquire(cell, &name).expect("ok call 2");
        assert!(lock3.is_some(), "tras drop del primero, try_acquire debe lograrlo");
        drop(lock3);
        // Cleanup: borrar el lock file
        let _ = fs::remove_file(lock_path(cell, &name));
    }
}

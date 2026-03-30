// =============================================================================
// NKR State — Tracking de micro-VMs activas
// =============================================================================
//
// Cada `nkr run` registra un archivo JSON en /tmp/nkr-vms/{vm_id}.json
// con metadata de la VM (PID, config, timestamps). `nkr ps` lee estos
// archivos y `nkr stop` envía SIGTERM al PID registrado.
// =============================================================================

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Directorio donde se almacenan los archivos de estado
const STATE_DIR: &str = "/tmp/nkr-vms";

/// Metadata de una micro-VM activa
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VmState {
    pub vm_id: u8,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub name: String,
    pub pid: u32,
    pub ram_mb: u32,
    pub chrs: u32,
    pub disks: Vec<String>,
    pub guest_ip: String,
    pub ports: Vec<String>,
    pub tap_name: String,
    pub started_at: u64, // Unix timestamp
}

/// Registra una VM activa escribiendo su estado a disco
pub fn register_vm(state: &VmState) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(STATE_DIR)?;
    let path = state_path(state.vm_id);
    let json = serde_json::to_string_pretty(state)?;
    let mut file = fs::File::create(&path)?;
    file.write_all(json.as_bytes())?;
    eprintln!("[NKR] VM {} registrada en {}", state.vm_id, path.display());
    Ok(())
}

/// Elimina el registro de una VM (llamado al salir)
pub fn unregister_vm(vm_id: u8) {
    let path = state_path(vm_id);
    if path.exists() {
        let _ = fs::remove_file(&path);
        eprintln!("[NKR] VM {} desregistrada", vm_id);
    }
}

/// Lista todas las VMs registradas (filtra las que ya no están vivas)
pub fn list_vms() -> Vec<VmState> {
    let dir = Path::new(STATE_DIR);
    if !dir.exists() {
        return Vec::new();
    }

    let mut vms = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map_or(false, |e| e == "json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(state) = serde_json::from_str::<VmState>(&content) {
                        // Verificar si el proceso sigue vivo
                        if is_pid_alive(state.pid) {
                            vms.push(state);
                        } else {
                            // Limpiar estado huérfano
                            let _ = fs::remove_file(&path);
                        }
                    }
                }
            }
        }
    }

    vms.sort_by_key(|v| v.vm_id);
    vms
}

/// Busca una VM por ID
pub fn find_vm(vm_id: u8) -> Option<VmState> {
    let path = state_path(vm_id);
    if !path.exists() {
        return None;
    }
    let content = fs::read_to_string(&path).ok()?;
    let state: VmState = serde_json::from_str(&content).ok()?;
    if is_pid_alive(state.pid) {
        Some(state)
    } else {
        let _ = fs::remove_file(&path);
        None
    }
}

/// Busca una VM por ID numérico, hash corto, o nombre
pub fn find_vm_by_id_str(id_str: &str) -> Option<VmState> {
    // Intentar parsear como ID numérico legacy
    if let Ok(num) = id_str.parse::<u8>() {
        if let Some(state) = find_vm(num) {
            return Some(state);
        }
    }
    
    // Buscar por hash exacto o nombre exacto
    for state in list_vms() {
        if state.hash == id_str || state.name == id_str {
            return Some(state);
        }
    }
    None
}

/// Detiene una VM enviando SIGTERM a su PID
pub fn stop_vm(vm_id: u8) -> Result<(), Box<dyn std::error::Error>> {
    let state = find_vm(vm_id)
        .ok_or_else(|| format!("VM {} no encontrada o ya detenida", vm_id))?;

    eprintln!("[NKR] Deteniendo VM {} (PID {})...", vm_id, state.pid);

    // Enviar SIGTERM
    let ret = unsafe { libc::kill(state.pid as i32, libc::SIGTERM) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        // Si el proceso ya no existe, limpiar y reportar
        if err.raw_os_error() == Some(libc::ESRCH) {
            unregister_vm(vm_id);
            return Err(format!("VM {} ya no existe (PID {} muerto)", vm_id, state.pid).into());
        }
        return Err(format!("No se pudo enviar SIGTERM a PID {}: {}", state.pid, err).into());
    }

    // Esperar brevemente a que termine
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !is_pid_alive(state.pid) {
            unregister_vm(vm_id);
            eprintln!("[NKR] VM {} detenida exitosamente", vm_id);
            return Ok(());
        }
    }

    // Si no terminó con SIGTERM, enviar SIGKILL
    eprintln!("[NKR] VM {} no respondió a SIGTERM, enviando SIGKILL...", vm_id);
    unsafe { libc::kill(state.pid as i32, libc::SIGKILL); }
    std::thread::sleep(std::time::Duration::from_millis(200));
    unregister_vm(vm_id);
    eprintln!("[NKR] VM {} forzada a detenerse", vm_id);
    Ok(())
}

/// Imprime una tabla con las VMs activas
pub fn print_vm_table() {
    let vms = list_vms();

    if vms.is_empty() {
        eprintln!("[NKR] No hay micro-VMs activas");
        return;
    }

    eprintln!("╔═════╦══════════════╦════════════════════╦════════╦═══════╦══════╦════════════════╦═══════════════════╦═══════════╦══════════╗");
    eprintln!("║ ID  ║     HASH     ║       NOMBRE       ║  PID   ║  RAM  ║ CHRs ║   Guest IP     ║      Discos       ║  Puertos  ║  Uptime  ║");
    eprintln!("╠═════╬══════════════╬════════════════════╬════════╬═══════╬══════╬════════════════╬═══════════════════╬═══════════╬══════════╣");

    for vm in &vms {
        let get_basename = |p: &str| -> String {
            Path::new(p).file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| p.to_string())
        };

        let disks_str = if vm.disks.is_empty() {
            "—".to_string()
        } else if vm.disks.len() == 1 {
            truncate_str(&get_basename(&vm.disks[0]), 17)
        } else {
            format!("{}+{}", truncate_str(&get_basename(&vm.disks[0]), 13), vm.disks.len() - 1)
        };

        let ports_str = if vm.ports.is_empty() {
            "—".to_string()
        } else if vm.ports.len() == 1 {
            vm.ports[0].clone()
        } else {
            format!("{}+{}", vm.ports[0], vm.ports.len() - 1)
        };

        let uptime_secs = current_timestamp().saturating_sub(vm.started_at);
        let uptime_str = if uptime_secs < 60 {
            format!("{}s", uptime_secs)
        } else if uptime_secs < 3600 {
            format!("{}m", uptime_secs / 60)
        } else {
            format!("{}h {}m", uptime_secs / 3600, (uptime_secs % 3600) / 60)
        };

        let hash_disp = if vm.hash.is_empty() { "—" } else { &vm.hash };
        let name_disp = if vm.name.is_empty() { "—" } else { &vm.name };
        let ram_disp = format!("{}M", vm.ram_mb);

        eprintln!("║ {:<3} ║ {:<12} ║ {:<18} ║ {:<6} ║ {:<5} ║ {:<4} ║ {:<14} ║ {:<17} ║ {:<9} ║ {:<8} ║",
            vm.vm_id, 
            truncate_str(hash_disp, 12), 
            truncate_str(name_disp, 18), 
            vm.pid, 
            ram_disp, 
            vm.chrs, 
            vm.guest_ip, 
            disks_str, 
            ports_str,
            uptime_str);
    }

    eprintln!("╚═════╩══════════════╩════════════════════╩════════╩═══════╩══════╩════════════════╩═══════════════════╩═══════════╩══════════╝");    eprintln!("[NKR] {} VM(s) activa(s)", vms.len());
}

// =============================================================================
// Utilidades internas
// =============================================================================

fn state_path(vm_id: u8) -> PathBuf {
    PathBuf::from(STATE_DIR).join(format!("{}.json", vm_id))
}

fn is_pid_alive(pid: u32) -> bool {
    // kill(pid, 0) verifica si el proceso existe sin enviar señal
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

pub fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
}

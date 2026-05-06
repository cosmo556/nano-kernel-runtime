// =============================================================================
// NKR State — Tracking of active micro-VMs
// =============================================================================
//
// Each `nkr run` registers a JSON file at /tmp/nkr-vms/{vm_id}.json with
// VM metadata (PID, config, timestamps). `nkr ps` reads these files and
// `nkr stop` sends SIGTERM to the registered PID.
// =============================================================================

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Directory where state files are stored
const STATE_DIR: &str = "/tmp/nkr-vms";

/// Active micro-VM metadata
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
    /// VirtIO-PMEM + DAX active (saves ~200 MB of duplicated page cache)
    #[serde(default)]
    pub use_pmem: bool,
    /// DAX active via any mechanism (virtio-pmem+DAX or virtio-fs+DAX window).
    /// Drives the `DAX save` column in `nkr stats`.
    #[serde(default)]
    pub use_dax: bool,
    /// MB currently inflated in the balloon (returned to the host)
    #[serde(default)]
    pub balloon_mb: u32,
    /// Cell ID (0 = legacy, 1-254 = cell with isolated bridge)
    #[serde(default)]
    pub cell_id: u8,
}

/// Registers an active VM by writing its state to disk.
/// Filename includes cell_id so two cells sharing vm_ids don't overwrite each other.
pub fn register_vm(state: &VmState) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(STATE_DIR)?;
    let path = state_path_scoped(state.cell_id, state.vm_id);
    let json = serde_json::to_string_pretty(state)?;
    let mut file = fs::File::create(&path)?;
    file.write_all(json.as_bytes())?;
    eprintln!("[NKR] VM {}/{} registrada en {}", state.cell_id, state.vm_id, path.display());
    Ok(())
}

/// Removes a VM's registration. Accepts vm_id alone (legacy) and scans the
/// state dir for any file matching that vm_id regardless of cell_id.
pub fn unregister_vm(vm_id: u8) {
    // Remove all state files matching this vm_id across any cell_id.
    let dir = Path::new(STATE_DIR);
    if !dir.exists() { return; }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map_or(false, |e| e == "json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(state) = serde_json::from_str::<VmState>(&content) {
                        if state.vm_id == vm_id {
                            let _ = fs::remove_file(&path);
                            eprintln!("[NKR] VM {}/{} desregistrada", state.cell_id, vm_id);
                        }
                    }
                }
            }
        }
    }
}

/// Lists all registered VMs (filters out those no longer alive)
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
                        // Check if the process is still alive
                        if is_pid_alive(state.pid) {
                            vms.push(state);
                        } else {
                            // Clean up orphaned state
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

/// Finds a VM by ID. Scans the state dir since the file name now includes
/// cell_id; if multiple cells share the vm_id this returns the first live one.
pub fn find_vm(vm_id: u8) -> Option<VmState> {
    for state in list_vms() {
        if state.vm_id == vm_id {
            return Some(state);
        }
    }
    None
}

/// Finds a VM by numeric ID, short hash, or name
pub fn find_vm_by_id_str(id_str: &str) -> Option<VmState> {
    // Try parsing as legacy numeric ID
    if let Ok(num) = id_str.parse::<u8>() {
        if let Some(state) = find_vm(num) {
            return Some(state);
        }
    }
    
    // Search by exact hash or exact name
    for state in list_vms() {
        if state.hash == id_str || state.name == id_str {
            return Some(state);
        }
    }
    None
}

/// Stops a VM by sending SIGTERM to its PID
pub fn stop_vm(vm_id: u8) -> Result<(), Box<dyn std::error::Error>> {
    let state = find_vm(vm_id)
        .ok_or_else(|| format!("VM {} no encontrada o ya detenida", vm_id))?;
    stop_vm_by_state(state)
}

/// Detiene una VM dado su nombre lógico — busca primero en state files;
/// si no aparece (state perdido por crash o desregistro previo), hace
/// fallback escaneando `/proc/<pid>/cmdline` por `nkr run --name <name>`.
/// Sin este fallback, los `nkr stop` zombian procesos cuando el state file
/// fue eliminado prematuramente.
pub fn stop_vm_by_name(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(s) = list_vms().into_iter().find(|v| v.name == name) {
        return stop_vm_by_state(s);
    }
    // Fallback: scan /proc por nkr run --name <name>
    let target_arg = format!("--name\0{}\0", name);
    let mut killed = 0u32;
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let pid: u32 = match e.file_name().to_string_lossy().parse() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let cmd = match std::fs::read(format!("/proc/{}/cmdline", pid)) {
                Ok(b) => b,
                Err(_) => continue,
            };
            // Necesita ser un `nkr run` con nuestro --name target.
            let cmd_str = String::from_utf8_lossy(&cmd);
            if cmd_str.contains("nkr") && cmd_str.contains("run") && cmd_str.contains(&target_arg) {
                eprintln!("[NKR] stop_vm_by_name: PID huérfano {} ({}). SIGTERM.", pid, name);
                unsafe { libc::kill(pid as i32, libc::SIGTERM); }
                killed += 1;
            }
        }
    }
    if killed == 0 {
        return Err(format!("VM '{}' no encontrada (ni en state ni como /proc/<pid> match)", name).into());
    }
    eprintln!("[NKR] stop_vm_by_name: {} proc(s) huérfanos terminados con SIGTERM", killed);
    Ok(())
}

fn stop_vm_by_state(state: VmState) -> Result<(), Box<dyn std::error::Error>> {
    let vm_id = state.vm_id;

    eprintln!("[NKR] Deteniendo VM {} (PID {})...", vm_id, state.pid);

    // Send SIGTERM
    let ret = unsafe { libc::kill(state.pid as i32, libc::SIGTERM) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        // If the process no longer exists, clean up and report
        if err.raw_os_error() == Some(libc::ESRCH) {
            unregister_vm(vm_id);
            return Err(format!("VM {} ya no existe (PID {} muerto)", vm_id, state.pid).into());
        }
        return Err(format!("No se pudo enviar SIGTERM a PID {}: {}", state.pid, err).into());
    }

    // Wait up to 90s (vmm.rs has an internal 60s timeout for guest shutdown
    // + cleanup/extract_volumes ~5-20s). SIGKILL only if something hung.
    for _ in 0..900 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !is_pid_alive(state.pid) {
            unregister_vm(vm_id);
            eprintln!("[NKR] VM {} detenida exitosamente", vm_id);
            return Ok(());
        }
    }

    // If it didn't exit with SIGTERM, send SIGKILL
    eprintln!("[NKR] VM {} no respondió a SIGTERM, enviando SIGKILL...", vm_id);
    unsafe { libc::kill(state.pid as i32, libc::SIGKILL); }
    std::thread::sleep(std::time::Duration::from_millis(200));
    unregister_vm(vm_id);
    eprintln!("[NKR] VM {} forzada a detenerse", vm_id);
    Ok(())
}

/// Prints a table of active VMs
pub fn print_vm_table() {
    let mut vms = list_vms();

    if vms.is_empty() {
        eprintln!("[NKR] No hay micro-VMs activas");
        return;
    }

    // Resolver cell_id → cell_name (cache para evitar re-leer el registry N veces).
    let mut cell_names: std::collections::HashMap<u8, String> =
        std::collections::HashMap::new();
    for vm in &vms {
        if !cell_names.contains_key(&vm.cell_id) {
            let name = if vm.cell_id == 0 {
                "—".to_string()
            } else {
                crate::cell::lookup_cell_name(vm.cell_id)
                    .unwrap_or_else(|| format!("cell-{}", vm.cell_id))
            };
            cell_names.insert(vm.cell_id, name);
        }
    }

    // Ordenar: primero por cell_name, luego por vm_id (db=1, pgb=2, odoos=3..N).
    vms.sort_by(|a, b| {
        let na = cell_names.get(&a.cell_id).cloned().unwrap_or_default();
        let nb = cell_names.get(&b.cell_id).cloned().unwrap_or_default();
        na.cmp(&nb).then(a.vm_id.cmp(&b.vm_id))
    });

    let headers = [
        ("CELL", 14usize),
        ("ID", 3),
        ("NOMBRE", 24),
        ("PID", 6),
        ("RAM", 6),
        ("CHRS", 4),
        ("GUEST IP", 14),
        ("PUERTOS", 11),
        ("UPTIME", 10),
    ];
    print_header_row(&headers);

    for vm in &vms {
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

        let name_disp = if vm.name.is_empty() { "—" } else { &vm.name };
        let ram_disp = format!("{}M", vm.ram_mb);
        let cell_disp = cell_names.get(&vm.cell_id).map(|s| s.as_str()).unwrap_or("—");

        let cells = [
            truncate_str(cell_disp, 14),
            vm.vm_id.to_string(),
            truncate_str(name_disp, 24),
            vm.pid.to_string(),
            ram_disp,
            vm.chrs.to_string(),
            vm.guest_ip.clone(),
            truncate_str(&ports_str, 11),
            uptime_str,
        ];
        print_data_row(&headers, &cells);
    }

    print_footer_separator(&headers);
    eprintln!("[NKR] {} NKR(s) activa(s) — usa 'nkr stats' para CPU%/RAM real/red/disco en vivo",
        vms.len());
}

/// Imprime header + separador inferior (`─`). Espaciado: 2 chars entre columnas.
pub fn print_header_row(headers: &[(&str, usize)]) {
    let mut line = String::new();
    for (i, (name, w)) in headers.iter().enumerate() {
        if i > 0 { line.push_str("  "); }
        line.push_str(&format!("{:<width$}", name, width = w));
    }
    eprintln!("{}", line.trim_end());
    eprintln!("{}", "─".repeat(table_total_width(headers)));
}

/// Imprime una fila de datos. Si `cells.len() != headers.len()` se trunca/rellena.
pub fn print_data_row(headers: &[(&str, usize)], cells: &[String]) {
    let mut line = String::new();
    for (i, (_, w)) in headers.iter().enumerate() {
        if i > 0 { line.push_str("  "); }
        let val = cells.get(i).map(|s| s.as_str()).unwrap_or("");
        line.push_str(&format!("{:<width$}", val, width = w));
    }
    eprintln!("{}", line.trim_end());
}

/// Línea `─` del mismo ancho que la tabla (separador inferior antes del footer).
pub fn print_footer_separator(headers: &[(&str, usize)]) {
    eprintln!("{}", "─".repeat(table_total_width(headers)));
}

fn table_total_width(headers: &[(&str, usize)]) -> usize {
    let cols: usize = headers.iter().map(|(_, w)| *w).sum();
    let gaps = if headers.is_empty() { 0 } else { (headers.len() - 1) * 2 };
    cols + gaps
}

// =============================================================================
// Internal utilities
// =============================================================================

fn state_path_scoped(cell_id: u8, vm_id: u8) -> PathBuf {
    PathBuf::from(STATE_DIR).join(format!("c{}-v{}.json", cell_id, vm_id))
}

fn is_pid_alive(pid: u32) -> bool {
    // kill(pid, 0) is insufficient: returns 0 for zombies (vmm exits but if
    // the parent 'nkr compose up' doesn't call wait(), it stays as Z and
    // stop_vm waited 90s in vain). Read /proc/<pid>/status and treat Z as dead.
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{}/status", pid)) {
        Ok(s) => !s.lines().any(|l| l.starts_with("State:") && l.contains('Z')),
        Err(_) => false,
    }
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

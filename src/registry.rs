// =============================================================================
// NKR Registry — Persistent ID assignment by NVM name
// =============================================================================
//
// Each NVM (name) receives a stable numeric ID that determines:
//   - Guest IP:   10.0.{cell_id}.{vm_id + 1}
//   - TAP device: nkr-c{cell_id}-tap{vm_id}
//   - MAC address: 52:54:00:{cell_id}:34:{vm_id}
//
// The registry is persisted in /mnt/nkr/registry.json to guarantee that the
// same name always gets the same IP, even after restarts.
//
// IDs are cell-scoped: the key is "cell_name/vm_name" (or "vm_name" for legacy cell_id=0).
// ID range: 2..254 (1 reserved internally, 0 discarded)
// =============================================================================

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Persistent registry file
fn registry_path() -> PathBuf {
    let nkr_data = std::env::var("NKR_DATA_DIR").unwrap_or_else(|_| "/mnt/nkr".to_string());
    PathBuf::from(nkr_data).join("registry.json")
}

/// Lockfile for serializing registry mutations across processes.
/// Without it, two parallel `nkr run` processes (compose up) can issue
/// concurrent load → check → save sequences and assign the same vm_id —
/// resulting in two VMs sharing the same IP/MAC.
fn registry_lock_path() -> PathBuf {
    let nkr_data = std::env::var("NKR_DATA_DIR").unwrap_or_else(|_| "/mnt/nkr".to_string());
    PathBuf::from(nkr_data).join("registry.lock")
}

/// Range of IDs auto-assignable
const MIN_ID: u8 = 2;
const MAX_ID: u8 = 254;

// =============================================================================
// flock guard — RAII exclusive lock for registry mutations
// =============================================================================

struct RegistryLock {
    _file: Option<File>,
}

impl RegistryLock {
    /// Acquires the exclusive lock (blocking). If the lockfile can't be
    /// opened/locked, degrades silently (warning + no lock) — an occasional
    /// race is preferable to a boot that hangs when /mnt/nkr is degraded.
    fn acquire() -> Self {
        let path = registry_lock_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let file = match OpenOptions::new()
            .create(true).read(true).write(true).truncate(false)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[NKR-REGISTRY] WARN: could not open {}: {} — no lock",
                    path.display(), e);
                return RegistryLock { _file: None };
            }
        };
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            eprintln!("[NKR-REGISTRY] WARN: flock(LOCK_EX) failed: {} — no lock",
                std::io::Error::last_os_error());
            return RegistryLock { _file: None };
        }
        RegistryLock { _file: Some(file) }
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        if let Some(f) = &self._file {
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN); }
        }
    }
}

// =============================================================================
// Persistent model
// =============================================================================

#[derive(Serialize, Deserialize, Default, Clone)]
struct Registry {
    /// name → assigned ID map
    entries: HashMap<String, u8>,
}

impl Registry {
    /// Loads the registry from disk, or creates an empty one if it doesn't exist
    fn load() -> Self {
        let path = registry_path();
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(reg) = serde_json::from_str::<Registry>(&content) {
                    return reg;
                }
            }
        }
        Registry::default()
    }

    /// Persists the registry to disk atomically: write a sibling .tmp file
    /// and rename(2) over the target. rename(2) on the same filesystem is
    /// atomic — a reader never observes a partial JSON. Without this, a
    /// SIGKILL/OOM mid-write leaves a truncated registry and the next
    /// load() returns Default (losing all assigned IDs).
    fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = registry_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, json)?;
        fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    /// Set of already-assigned IDs (global, legacy)
    #[allow(dead_code)]
    fn used_ids(&self) -> Vec<u8> {
        self.entries.values().cloned().collect()
    }

    /// IDs used within a cell's scope (prefix "cell_name/")
    /// If scope=None, returns IDs without "/" prefix (legacy cell_id=0).
    fn used_ids_in_scope(&self, scope: Option<&str>) -> Vec<u8> {
        self.entries.iter().filter_map(|(k, v)| {
            let in_scope = match scope {
                Some(cell) => k.starts_with(&format!("{}/", cell)),
                None => !k.contains('/'),
            };
            if in_scope { Some(*v) } else { None }
        }).collect()
    }

    /// Next free ID within a cell's scope
    fn next_free_id_in_scope(&self, scope: Option<&str>) -> Option<u8> {
        let used = self.used_ids_in_scope(scope);
        (MIN_ID..=MAX_ID).find(|id| !used.contains(id))
    }
}

// =============================================================================
// Public API
// =============================================================================

/// Resolves the ID for a given name, optionally scoped by cell.
/// Resolves the vm_id within a cell's scope (or legacy if cell_name=None).
/// If an ID is already assigned, returns it. Otherwise, assigns the next free one.
/// With cell_name=Some("nazcatex"), the key is "nazcatex/vm_name".
/// With cell_name=None, the key is "vm_name" (legacy, cell_id=0).
pub fn resolve_id_scoped(cell_name: Option<&str>, name: &str) -> Result<u8, Box<dyn std::error::Error>> {
    // The exclusive lock covers the WHOLE load → check → next_free → save
    // section. This closes the allocation race: two parallel processes asking
    // for a new vm_id are serialized; the second sees the ID assigned by the
    // first and picks the next free one.
    let _guard = RegistryLock::acquire();

    let mut reg = Registry::load();

    let vm_key = name.trim().to_lowercase();
    if vm_key.is_empty() {
        return Err("El nombre del NVM no puede estar vacío".into());
    }

    // Scoped key: "cell/vm" or "vm" (legacy)
    let key = match cell_name {
        Some(cell) => format!("{}/{}", cell.trim().to_lowercase(), vm_key),
        None => vm_key.clone(),
    };

    if let Some(&id) = reg.entries.get(&key) {
        return Ok(id);
    }

    // Assign new ID (cell-scoped — each cell has its own subnet)
    let scope_key = cell_name.map(|c| c.trim().to_lowercase());
    let new_id = reg.next_free_id_in_scope(scope_key.as_deref())
        .ok_or("No hay IDs disponibles (rango 2-254 agotado)")?;

    reg.entries.insert(key.clone(), new_id);
    reg.save()?;

    let cell_id = cell_name.and_then(|c| crate::cell::lookup_cell_id(c)).unwrap_or(0);
    eprintln!("[NKR-REGISTRY] Nuevo: '{}' → id={} (IP={})", key, new_id, id_to_ip(cell_id, new_id));

    Ok(new_id)
}

/// Registers a name with a specific ID (for backward-compat with explicit id:).
/// If the name already had another ID, updates it. If the ID is already in use
/// by another name, returns an error.
#[allow(dead_code)]
pub fn register_explicit(name: &str, id: u8) -> Result<(), Box<dyn std::error::Error>> {
    register_explicit_scoped(None, name, id)
}

/// Cell-scoped version of register_explicit
pub fn register_explicit_scoped(cell_name: Option<&str>, name: &str, id: u8) -> Result<(), Box<dyn std::error::Error>> {
    let _guard = RegistryLock::acquire();
    let mut reg = Registry::load();
    let vm_key = name.trim().to_lowercase();

    let key = match cell_name {
        Some(cell) => format!("{}/{}", cell.trim().to_lowercase(), vm_key),
        None => vm_key,
    };

    // Check ID conflict ONLY within the same cell scope
    // (IDs across different cells do NOT collide: they live in separate subnets)
    let scope_prefix: Option<String> = cell_name.map(|c| format!("{}/", c.trim().to_lowercase()));
    for (existing_name, &existing_id) in &reg.entries {
        if existing_id != id || *existing_name == key {
            continue;
        }
        let same_scope = match &scope_prefix {
            Some(p) => existing_name.starts_with(p),
            None => !existing_name.contains('/'),
        };
        if same_scope {
            let cell_id = cell_name.and_then(|c| crate::cell::lookup_cell_id(c)).unwrap_or(0);
            return Err(format!(
                "Conflicto de ID: '{}' ya usa id={} (IP={}). \
                 No se puede asignar a '{}'. Elimina el id: del compose o cambia el otro.",
                existing_name, id, id_to_ip(cell_id, id), name
            ).into());
        }
    }

    // Register or update
    if reg.entries.get(&key) != Some(&id) {
        reg.entries.insert(key, id);
        reg.save()?;
    }

    Ok(())
}

/// Returns the ID assigned to a name, if it exists
#[allow(dead_code)]
pub fn lookup(name: &str) -> Option<u8> {
    let reg = Registry::load();
    let key = name.trim().to_lowercase();
    reg.entries.get(&key).cloned()
}

/// Lists all registry entries (for debug/info)
#[allow(dead_code)]
pub fn list_all() -> Vec<(String, u8)> {
    let reg = Registry::load();
    let mut entries: Vec<(String, u8)> = reg.entries.into_iter().collect();
    entries.sort_by_key(|(_, id)| *id);
    entries
}

/// Removes a name from the registry
#[allow(dead_code)]
pub fn remove(name: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let _guard = RegistryLock::acquire();
    let mut reg = Registry::load();
    let key = name.trim().to_lowercase();
    let removed = reg.entries.remove(&key).is_some();
    if removed {
        reg.save()?;
    }
    Ok(removed)
}

/// Guest IP for cell_id + vm_id: 10.0.{cell_id}.{vm_id+1}
pub fn id_to_ip(cell_id: u8, vm_id: u8) -> String {
    format!("10.0.{}.{}", cell_id, vm_id + 1)
}

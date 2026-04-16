// =============================================================================
// NKR Registry — Asignación persistente de IDs por nombre de NVM
// =============================================================================
//
// Cada NVM (nombre) recibe un ID numérico estable que determina:
//   - IP del guest:   10.0.{cell_id}.{vm_id + 1}
//   - TAP device:     nkr-c{cell_id}-tap{vm_id}
//   - MAC address:    52:54:00:{cell_id}:34:{vm_id}
//
// El registro se persiste en /mnt/nkr/registry.json para garantizar que
// el mismo nombre siempre obtenga la misma IP, incluso tras reinicios.
//
// IDs son cell-scoped: la key es "cell_name/vm_name" (o "vm_name" para legacy cell_id=0).
// Rango de IDs: 2..254 (1 reservado internamente, 0 descartado)
// =============================================================================

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Archivo de registro persistente
fn registry_path() -> PathBuf {
    let nkr_data = std::env::var("NKR_DATA_DIR").unwrap_or_else(|_| "/mnt/nkr".to_string());
    PathBuf::from(nkr_data).join("registry.json")
}

/// Rango de IDs asignables automáticamente
const MIN_ID: u8 = 2;
const MAX_ID: u8 = 254;

// =============================================================================
// Modelo persistente
// =============================================================================

#[derive(Serialize, Deserialize, Default, Clone)]
struct Registry {
    /// Mapa nombre → ID asignado
    entries: HashMap<String, u8>,
}

impl Registry {
    /// Carga el registro desde disco, o crea uno vacío si no existe
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

    /// Persiste el registro a disco
    fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = registry_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, json)?;
        Ok(())
    }

    /// Conjunto de IDs ya asignados (global, legacy)
    #[allow(dead_code)]
    fn used_ids(&self) -> Vec<u8> {
        self.entries.values().cloned().collect()
    }

    /// IDs usados dentro del scope de una cell (prefijo "cell_name/")
    /// Si scope=None, devuelve IDs sin prefijo "/" (legacy cell_id=0).
    fn used_ids_in_scope(&self, scope: Option<&str>) -> Vec<u8> {
        self.entries.iter().filter_map(|(k, v)| {
            let in_scope = match scope {
                Some(cell) => k.starts_with(&format!("{}/", cell)),
                None => !k.contains('/'),
            };
            if in_scope { Some(*v) } else { None }
        }).collect()
    }

    /// Siguiente ID libre dentro del scope de una cell
    fn next_free_id_in_scope(&self, scope: Option<&str>) -> Option<u8> {
        let used = self.used_ids_in_scope(scope);
        (MIN_ID..=MAX_ID).find(|id| !used.contains(id))
    }
}

// =============================================================================
// API pública
// =============================================================================

/// Resuelve el ID para un nombre dado, opcionalmente scoped por cell.
/// Si ya tiene un ID asignado, lo retorna. Si no, asigna el siguiente libre.
/// Con cell_name=Some("nazcatex"), la key es "nazcatex/vm_name".
/// Con cell_name=None, la key es "vm_name" (legacy, cell_id=0).
pub fn resolve_id(name: &str) -> Result<u8, Box<dyn std::error::Error>> {
    resolve_id_scoped(None, name)
}

/// Versión cell-scoped de resolve_id
pub fn resolve_id_scoped(cell_name: Option<&str>, name: &str) -> Result<u8, Box<dyn std::error::Error>> {
    let mut reg = Registry::load();

    let vm_key = name.trim().to_lowercase();
    if vm_key.is_empty() {
        return Err("El nombre del NVM no puede estar vacío".into());
    }

    // Key scoped: "cell/vm" o "vm" (legacy)
    let key = match cell_name {
        Some(cell) => format!("{}/{}", cell.trim().to_lowercase(), vm_key),
        None => vm_key.clone(),
    };

    if let Some(&id) = reg.entries.get(&key) {
        return Ok(id);
    }

    // Asignar nuevo ID (scoped por cell — cada cell tiene su propia subnet)
    let scope_key = cell_name.map(|c| c.trim().to_lowercase());
    let new_id = reg.next_free_id_in_scope(scope_key.as_deref())
        .ok_or("No hay IDs disponibles (rango 2-254 agotado)")?;

    reg.entries.insert(key.clone(), new_id);
    reg.save()?;

    let cell_id = cell_name.and_then(|c| crate::cell::lookup_cell_id(c)).unwrap_or(0);
    eprintln!("[NKR-REGISTRY] Nuevo: '{}' → id={} (IP={})", key, new_id, id_to_ip(cell_id, new_id));

    Ok(new_id)
}

/// Registra un nombre con un ID específico (para backward-compat con id: explícito).
/// Si el nombre ya tenía otro ID, lo actualiza. Si el ID ya está en uso por
/// otro nombre, devuelve error.
#[allow(dead_code)]
pub fn register_explicit(name: &str, id: u8) -> Result<(), Box<dyn std::error::Error>> {
    register_explicit_scoped(None, name, id)
}

/// Versión cell-scoped de register_explicit
pub fn register_explicit_scoped(cell_name: Option<&str>, name: &str, id: u8) -> Result<(), Box<dyn std::error::Error>> {
    let mut reg = Registry::load();
    let vm_key = name.trim().to_lowercase();

    let key = match cell_name {
        Some(cell) => format!("{}/{}", cell.trim().to_lowercase(), vm_key),
        None => vm_key,
    };

    // Verificar conflicto de ID SOLO dentro del mismo scope de cell
    // (IDs entre cells distintas NO colisionan: viven en subnets separadas)
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

    // Registrar o actualizar
    if reg.entries.get(&key) != Some(&id) {
        reg.entries.insert(key, id);
        reg.save()?;
    }

    Ok(())
}

/// Devuelve el ID asignado a un nombre, si existe
#[allow(dead_code)]
pub fn lookup(name: &str) -> Option<u8> {
    let reg = Registry::load();
    let key = name.trim().to_lowercase();
    reg.entries.get(&key).cloned()
}

/// Lista todos los registros (para debug/info)
#[allow(dead_code)]
pub fn list_all() -> Vec<(String, u8)> {
    let reg = Registry::load();
    let mut entries: Vec<(String, u8)> = reg.entries.into_iter().collect();
    entries.sort_by_key(|(_, id)| *id);
    entries
}

/// Elimina un nombre del registro
#[allow(dead_code)]
pub fn remove(name: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let mut reg = Registry::load();
    let key = name.trim().to_lowercase();
    let removed = reg.entries.remove(&key).is_some();
    if removed {
        reg.save()?;
    }
    Ok(removed)
}

/// IP del guest para un cell_id + vm_id: 10.0.{cell_id}.{vm_id+1}
pub fn id_to_ip(cell_id: u8, vm_id: u8) -> String {
    format!("10.0.{}.{}", cell_id, vm_id + 1)
}

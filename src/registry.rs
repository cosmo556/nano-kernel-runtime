// =============================================================================
// NKR Registry — Asignación persistente de IDs por nombre de NVM
// =============================================================================
//
// Cada NVM (nombre) recibe un ID numérico estable que determina:
//   - IP del guest:   10.0.0.(id + 1)
//   - TAP device:     nkr-tap{id}
//   - MAC address:    52:54:00:12:34:{id}
//
// El registro se persiste en /mnt/nkr/registry.json para garantizar que
// el mismo nombre siempre obtenga la misma IP, incluso tras reinicios.
//
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

    /// Conjunto de IDs ya asignados
    fn used_ids(&self) -> Vec<u8> {
        self.entries.values().cloned().collect()
    }

    /// Siguiente ID libre en el rango [MIN_ID, MAX_ID]
    fn next_free_id(&self) -> Option<u8> {
        let used = self.used_ids();
        (MIN_ID..=MAX_ID).find(|id| !used.contains(id))
    }
}

// =============================================================================
// API pública
// =============================================================================

/// Resuelve el ID para un nombre dado.
/// Si ya tiene un ID asignado, lo retorna. Si no, asigna el siguiente libre.
pub fn resolve_id(name: &str) -> Result<u8, Box<dyn std::error::Error>> {
    let mut reg = Registry::load();

    // Normalizar nombre (lowercase, trim)
    let key = name.trim().to_lowercase();
    if key.is_empty() {
        return Err("El nombre del NVM no puede estar vacío".into());
    }

    if let Some(&id) = reg.entries.get(&key) {
        return Ok(id);
    }

    // Asignar nuevo ID
    let new_id = reg.next_free_id()
        .ok_or("No hay IDs disponibles (rango 2-254 agotado)")?;

    reg.entries.insert(key.clone(), new_id);
    reg.save()?;

    eprintln!("[NKR-REGISTRY] Nuevo: '{}' → id={} (IP=10.0.0.{})", name, new_id, new_id + 1);

    Ok(new_id)
}

/// Registra un nombre con un ID específico (para backward-compat con id: explícito).
/// Si el nombre ya tenía otro ID, lo actualiza. Si el ID ya está en uso por
/// otro nombre, devuelve error.
pub fn register_explicit(name: &str, id: u8) -> Result<(), Box<dyn std::error::Error>> {
    let mut reg = Registry::load();
    let key = name.trim().to_lowercase();

    // Verificar si otro nombre ya tiene ese ID
    for (existing_name, &existing_id) in &reg.entries {
        if existing_id == id && *existing_name != key {
            return Err(format!(
                "Conflicto de ID: '{}' ya usa id={} (IP=10.0.0.{}). \
                 No se puede asignar a '{}'. Elimina el id: del compose o cambia el otro.",
                existing_name, id, id + 1, name
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
pub fn lookup(name: &str) -> Option<u8> {
    let reg = Registry::load();
    let key = name.trim().to_lowercase();
    reg.entries.get(&key).cloned()
}

/// Lista todos los registros (para debug/info)
pub fn list_all() -> Vec<(String, u8)> {
    let reg = Registry::load();
    let mut entries: Vec<(String, u8)> = reg.entries.into_iter().collect();
    entries.sort_by_key(|(_, id)| *id);
    entries
}

/// Elimina un nombre del registro
pub fn remove(name: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let mut reg = Registry::load();
    let key = name.trim().to_lowercase();
    let removed = reg.entries.remove(&key).is_some();
    if removed {
        reg.save()?;
    }
    Ok(removed)
}

/// IP del guest para un ID dado
pub fn id_to_ip(id: u8) -> String {
    format!("10.0.0.{}", id + 1)
}

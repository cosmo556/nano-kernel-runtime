// =============================================================================
// NKR Cell — Gestión de Células (grupos de VMs con red aislada)
// =============================================================================
//
// Cada célula es un stack independiente (ej. 20 Odoo + PgBouncer + PG) con
// su propia subnet L2/L3:
//
//   Cell "nazcatex" (cell_id=1) → bridge nkr-br1, subnet 10.0.1.0/24
//   Cell "cafeteria" (cell_id=2) → bridge nkr-br2, subnet 10.0.2.0/24
//
// El cell_id se auto-asigna por el Cell Registry (persistido en disco).
// cell_id=0 es el modo legacy (bridge nkr0, subnet 10.0.0.0/24).
// =============================================================================

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Directorio base de datos NKR
fn nkr_data_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("NKR_DATA_DIR").unwrap_or_else(|_| "/mnt/nkr".to_string()),
    )
}

fn cell_registry_path() -> PathBuf {
    nkr_data_dir().join("cell-registry.json")
}

fn cells_dir() -> PathBuf {
    nkr_data_dir().join("cells")
}

const MIN_CELL_ID: u8 = 1;
const MAX_CELL_ID: u8 = 254;

// =============================================================================
// Cell Registry — Mapa persistente nombre → cell_id
// =============================================================================

#[derive(Serialize, Deserialize, Default, Clone)]
struct CellRegistry {
    entries: HashMap<String, u8>,
}

impl CellRegistry {
    fn load() -> Self {
        let path = cell_registry_path();
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(reg) = serde_json::from_str::<CellRegistry>(&content) {
                    return reg;
                }
            }
        }
        CellRegistry::default()
    }

    fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = cell_registry_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, json)?;
        Ok(())
    }

    fn used_ids(&self) -> Vec<u8> {
        self.entries.values().cloned().collect()
    }

    fn next_free_id(&self) -> Option<u8> {
        let used = self.used_ids();
        (MIN_CELL_ID..=MAX_CELL_ID).find(|id| !used.contains(id))
    }
}

// =============================================================================
// CellConfig — Metadata persistida en cell.yml
// =============================================================================

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CellConfig {
    pub name: String,
    pub cell_id: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub odoo_version: Option<String>,
}

impl CellConfig {
    /// Subnet prefix para esta cell: "10.0.{cell_id}"
    #[allow(dead_code)]
    pub fn subnet_prefix(&self) -> String {
        cell_id_to_subnet(self.cell_id)
    }

    /// Nombre del bridge Linux para esta cell
    #[allow(dead_code)]
    pub fn bridge_name(&self) -> String {
        cell_bridge_name(self.cell_id)
    }
}

// =============================================================================
// API pública
// =============================================================================

/// Devuelve el subnet prefix para un cell_id: "10.0.{cell_id}"
pub fn cell_id_to_subnet(cell_id: u8) -> String {
    format!("10.0.{}", cell_id)
}

/// Nombre del bridge para un cell_id
pub fn cell_bridge_name(cell_id: u8) -> String {
    if cell_id == 0 {
        "nkr0".to_string()
    } else {
        format!("nkr-br{}", cell_id)
    }
}

/// Crea una nueva célula: registra, crea directorios, escribe cell.yml
pub fn create_cell(name: &str, odoo_version: Option<&str>) -> Result<CellConfig, Box<dyn std::error::Error>> {
    let key = name.trim().to_lowercase();
    if key.is_empty() {
        return Err("El nombre de la célula no puede estar vacío".into());
    }

    let mut reg = CellRegistry::load();

    // Verificar si ya existe
    if let Some(&existing_id) = reg.entries.get(&key) {
        return Err(format!(
            "La célula '{}' ya existe (cell_id={}, subnet=10.0.{}.0/24)",
            name, existing_id, existing_id
        ).into());
    }

    // Asignar cell_id
    let cell_id = reg.next_free_id()
        .ok_or("No hay cell_ids disponibles (rango 1-254 agotado)")?;

    reg.entries.insert(key.clone(), cell_id);
    reg.save()?;

    // Crear estructura de directorios
    let cell_dir = cells_dir().join(&key);
    fs::create_dir_all(cell_dir.join("addons"))?;
    fs::create_dir_all(cell_dir.join("files"))?;
    fs::create_dir_all(cell_dir.join("config"))?;
    fs::create_dir_all(cell_dir.join("logs"))?;
    fs::create_dir_all(cell_dir.join("pg"))?;

    // Escribir cell.yml
    let config = CellConfig {
        name: key.clone(),
        cell_id,
        odoo_version: odoo_version.map(|s| s.to_string()),
    };

    let cell_yml_path = cell_dir.join("cell.yml");
    let yaml = serde_yaml::to_string(&config)?;
    fs::write(&cell_yml_path, yaml)?;

    eprintln!("[NKR-CELL] Célula '{}' creada (cell_id={}, subnet=10.0.{}.0/24)",
        name, cell_id, cell_id);
    eprintln!("[NKR-CELL] Directorio: {}", cell_dir.display());

    Ok(config)
}

/// Carga la configuración de una célula desde su cell.yml
pub fn load_cell(name: &str) -> Result<CellConfig, Box<dyn std::error::Error>> {
    let key = name.trim().to_lowercase();
    let cell_dir = cells_dir().join(&key);
    let cell_yml = cell_dir.join("cell.yml");

    if !cell_yml.exists() {
        return Err(format!("Célula '{}' no encontrada (no existe {})", name, cell_yml.display()).into());
    }

    let content = fs::read_to_string(&cell_yml)?;
    let config: CellConfig = serde_yaml::from_str(&content)?;
    Ok(config)
}

/// Intenta cargar cell.yml desde un directorio (para compose auto-detection)
pub fn load_cell_from_dir(dir: &Path) -> Option<CellConfig> {
    let cell_yml = dir.join("cell.yml");
    if !cell_yml.exists() {
        return None;
    }
    let content = fs::read_to_string(&cell_yml).ok()?;
    serde_yaml::from_str(&content).ok()
}

/// Lista todas las células registradas
pub fn list_cells() -> Vec<CellConfig> {
    let cells = cells_dir();
    if !cells.exists() {
        return Vec::new();
    }

    let mut result = Vec::new();
    if let Ok(entries) = fs::read_dir(&cells) {
        for entry in entries.flatten() {
            let cell_yml = entry.path().join("cell.yml");
            if cell_yml.exists() {
                if let Ok(content) = fs::read_to_string(&cell_yml) {
                    if let Ok(config) = serde_yaml::from_str::<CellConfig>(&content) {
                        result.push(config);
                    }
                }
            }
        }
    }

    result.sort_by_key(|c| c.cell_id);
    result
}

/// Busca el cell_id para un nombre, si existe
pub fn lookup_cell_id(name: &str) -> Option<u8> {
    let reg = CellRegistry::load();
    let key = name.trim().to_lowercase();
    reg.entries.get(&key).cloned()
}

/// Elimina una célula del registry (no borra datos del disco)
pub fn destroy_cell(name: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let key = name.trim().to_lowercase();
    let mut reg = CellRegistry::load();

    let cell_id = match reg.entries.remove(&key) {
        Some(id) => id,
        None => return Ok(false),
    };
    reg.save()?;

    // Eliminar cell.yml pero NO los datos
    let cell_yml = cells_dir().join(&key).join("cell.yml");
    let _ = fs::remove_file(&cell_yml);

    // Destruir bridge si existe
    let _ = destroy_cell_bridge(cell_id);

    eprintln!("[NKR-CELL] Célula '{}' eliminada del registry (datos preservados en {})",
        name, cells_dir().join(&key).display());
    Ok(true)
}

/// Ruta al compose de una célula
pub fn cell_compose_path(name: &str) -> PathBuf {
    let key = name.trim().to_lowercase();
    cells_dir().join(&key).join("nkr-compose.yml")
}

/// Directorio de una célula
#[allow(dead_code)]
pub fn cell_dir(name: &str) -> PathBuf {
    let key = name.trim().to_lowercase();
    cells_dir().join(&key)
}

// =============================================================================
// Bridge Management — Crea/destruye bridges Linux per-cell
// =============================================================================

/// Crea el bridge de red para una célula con NAT y forwarding
pub fn ensure_cell_bridge(cell_id: u8) -> Result<(), Box<dyn std::error::Error>> {
    let bridge = cell_bridge_name(cell_id);
    let subnet = cell_id_to_subnet(cell_id);
    let gateway = format!("{}.1/24", subnet);
    let subnet_cidr = format!("{}.0/24", subnet);

    // Verificar si ya existe
    let check = std::process::Command::new("ip")
        .args(["link", "show", &bridge])
        .output();
    if check.map_or(false, |o| o.status.success()) {
        return Ok(()); // Ya existe
    }

    eprintln!("[NKR-CELL] Creando bridge {} ({})...", bridge, subnet_cidr);

    // Crear bridge
    let status = std::process::Command::new("ip")
        .args(["link", "add", "name", &bridge, "type", "bridge"])
        .status()
        .map_err(|e| format!("Fallo creando bridge {}: {}", bridge, e))?;
    if !status.success() {
        return Err(format!("Fallo 'ip link add {}' (¿ejecutando con sudo?)", bridge).into());
    }

    // Asignar IP gateway
    let _ = std::process::Command::new("ip")
        .args(["addr", "add", &gateway, "dev", &bridge])
        .status();

    // Levantar bridge
    let _ = std::process::Command::new("ip")
        .args(["link", "set", &bridge, "up"])
        .status();

    // IP forwarding
    let _ = fs::write("/proc/sys/net/ipv4/ip_forward", "1");
    let _ = fs::write(&format!("/proc/sys/net/ipv4/conf/{}/route_localnet", bridge), "1");
    let _ = fs::write("/proc/sys/net/ipv4/conf/all/route_localnet", "1");

    // Helper: añade regla iptables solo si no existe (silencia stderr del -C)
    let iptables_ensure = |args_check: &[&str], args_add: &[&str]| {
        let check = std::process::Command::new("iptables")
            .args(args_check)
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status();
        if !check.map(|s| s.success()).unwrap_or(false) {
            let _ = std::process::Command::new("iptables")
                .args(args_add)
                .status();
        }
    };

    // NAT/Masquerade
    iptables_ensure(
        &["-t", "nat", "-C", "POSTROUTING", "-s", &subnet_cidr, "-j", "MASQUERADE"],
        &["-t", "nat", "-A", "POSTROUTING", "-s", &subnet_cidr, "-j", "MASQUERADE"],
    );

    // FORWARD rules
    iptables_ensure(
        &["-C", "FORWARD", "-i", &bridge, "-j", "ACCEPT"],
        &["-A", "FORWARD", "-i", &bridge, "-j", "ACCEPT"],
    );
    iptables_ensure(
        &["-C", "FORWARD", "-o", &bridge, "-j", "ACCEPT"],
        &["-A", "FORWARD", "-o", &bridge, "-j", "ACCEPT"],
    );

    eprintln!("[NKR-CELL] Bridge {} creado ({}, NAT habilitado)", bridge, subnet_cidr);
    Ok(())
}

/// Destruye el bridge de una célula y limpia reglas iptables
pub fn destroy_cell_bridge(cell_id: u8) -> Result<(), Box<dyn std::error::Error>> {
    if cell_id == 0 {
        return Ok(()); // No destruir bridge legacy nkr0
    }

    let bridge = cell_bridge_name(cell_id);
    let subnet_cidr = format!("{}.0/24", cell_id_to_subnet(cell_id));

    // Eliminar reglas iptables
    let _ = std::process::Command::new("iptables")
        .args(["-t", "nat", "-D", "POSTROUTING", "-s", &subnet_cidr, "-j", "MASQUERADE"])
        .status();
    let _ = std::process::Command::new("iptables")
        .args(["-D", "FORWARD", "-i", &bridge, "-j", "ACCEPT"])
        .status();
    let _ = std::process::Command::new("iptables")
        .args(["-D", "FORWARD", "-o", &bridge, "-j", "ACCEPT"])
        .status();

    // Bajar y eliminar bridge
    let _ = std::process::Command::new("ip")
        .args(["link", "set", &bridge, "down"])
        .status();
    let _ = std::process::Command::new("ip")
        .args(["link", "delete", &bridge])
        .status();

    eprintln!("[NKR-CELL] Bridge {} eliminado", bridge);
    Ok(())
}

// =============================================================================
// Tabla ASCII para `nkr cell ls`
// =============================================================================

pub fn print_cell_table() {
    let cells = list_cells();

    if cells.is_empty() {
        eprintln!("[NKR] No hay células registradas. Crear con: nkr cell create <nombre>");
        return;
    }

    // Contar VMs activas por cell
    let active_vms = crate::state::list_vms();

    eprintln!("╔════════╦══════════════════════╦════════════════════╦═══════════╦═══════════╗");
    eprintln!("║ CellID ║       Nombre         ║      Subnet        ║   Odoo    ║ VMs Vivas ║");
    eprintln!("╠════════╬══════════════════════╬════════════════════╬═══════════╬═══════════╣");

    for cell in &cells {
        let vm_count = active_vms.iter()
            .filter(|vm| vm.cell_id == cell.cell_id)
            .count();
        let version = cell.odoo_version.as_deref().unwrap_or("—");
        let subnet = format!("10.0.{}.0/24", cell.cell_id);

        eprintln!("║ {:<6} ║ {:<20} ║ {:<18} ║ {:<9} ║ {:<9} ║",
            cell.cell_id,
            truncate(cell.name.as_str(), 20),
            subnet,
            version,
            vm_count,
        );
    }

    eprintln!("╚════════╩══════════════════════╩════════════════════╩═══════════╩═══════════╝");
    eprintln!("[NKR] {} célula(s) registrada(s)", cells.len());
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…", &s[..max - 1]) }
}

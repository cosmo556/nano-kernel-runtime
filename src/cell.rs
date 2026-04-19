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

/// Búsqueda inversa: cell_id → nombre de cell. Devuelve None si no existe o si id=0.
pub fn lookup_cell_name(cell_id: u8) -> Option<String> {
    if cell_id == 0 { return None; }
    let reg = CellRegistry::load();
    reg.entries.iter()
        .find(|(_, &v)| v == cell_id)
        .map(|(k, _)| k.clone())
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

    // Verificar si ya existe (fuera del lock — el check es barato y sirve de fast-path)
    let check = std::process::Command::new("ip")
        .args(["link", "show", &bridge])
        .output();
    if check.map_or(false, |o| o.status.success()) {
        return Ok(()); // Ya existe
    }

    // Serializa creación de bridge + reglas iptables entre procesos `nkr run`
    // concurrentes (evita duplicados y "File exists" en rtnetlink).
    let _netlock = crate::netlock::NetLock::acquire("cell-bridge");

    // Re-verificar dentro del lock: otro proceso puede haber creado el bridge
    // mientras esperábamos.
    let recheck = std::process::Command::new("ip")
        .args(["link", "show", &bridge])
        .output();
    if recheck.map_or(false, |o| o.status.success()) {
        return Ok(());
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
        let check = crate::netlock::iptables()
            .args(args_check)
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status();
        if !check.map(|s| s.success()).unwrap_or(false) {
            let _ = crate::netlock::iptables()
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
    let _ = crate::netlock::iptables()
        .args(["-t", "nat", "-D", "POSTROUTING", "-s", &subnet_cidr, "-j", "MASQUERADE"])
        .status();
    let _ = crate::netlock::iptables()
        .args(["-D", "FORWARD", "-i", &bridge, "-j", "ACCEPT"])
        .status();
    let _ = crate::netlock::iptables()
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

// =============================================================================
// Clone — Duplica una instancia Odoo dentro de la misma cell
// =============================================================================

/// Localiza en qué cell vive una instancia dada su nkr_name.
/// Busca `cells/*/instances/<nkr_name>/` y devuelve (CellConfig, path).
fn find_instance_cell(nkr_name: &str) -> Result<(CellConfig, PathBuf), Box<dyn std::error::Error>> {
    let base = cells_dir();
    if !base.exists() {
        return Err(format!("No existe {} — no hay cells", base.display()).into());
    }
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        let cell_yml = entry.path().join("cell.yml");
        let inst_dir = entry.path().join("instances").join(nkr_name);
        if cell_yml.exists() && inst_dir.exists() && inst_dir.is_dir() {
            let content = fs::read_to_string(&cell_yml)?;
            let config: CellConfig = serde_yaml::from_str(&content)?;
            return Ok((config, inst_dir));
        }
    }
    Err(format!("Instancia '{}' no encontrada bajo ninguna cell en {}", nkr_name, base.display()).into())
}

/// Reescribe odoo.conf del destino sustituyendo cadenas del origen.
/// Maneja `db_name`, `dbfilter` y cualquier ruta con el nkr_name.
fn rewrite_odoo_conf(dst_dir: &Path, src_nkr: &str, dst_nkr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let conf_path = dst_dir.join("config").join("odoo.conf");
    if !conf_path.exists() {
        eprintln!("[NKR-CLONE] WARN: {} no existe, omitido", conf_path.display());
        return Ok(());
    }
    let content = fs::read_to_string(&conf_path)?;
    let new_content = content.replace(src_nkr, dst_nkr);
    fs::write(&conf_path, new_content)?;
    eprintln!("[NKR-CLONE] odoo.conf: {} → {}", src_nkr, dst_nkr);
    Ok(())
}

/// Clona la DB vía `CREATE DATABASE ... TEMPLATE`. Requiere `psql` en el host.
/// Estrategia: ALTER DATABASE ... ALLOW_CONNECTIONS=false → pg_terminate_backend
/// → CREATE DATABASE WITH TEMPLATE → ALTER ... ALLOW_CONNECTIONS=true.
/// Durante la ventana (~segundos) los clientes del src pierden conexión y
/// reconectan vía pgbouncer al reabrirse.
fn clone_database(cell_id: u8, src_nkr: &str, dst_nkr: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Por convención de célula: pg tiene vm_id=1 → IP .2
    let db_ip = format!("10.0.{}.2", cell_id);
    let src_db = format!("db-{}", src_nkr);
    let dst_db = format!("db-{}", dst_nkr);

    // Sanity check de conectividad
    let check = std::process::Command::new("pg_isready")
        .args(["-h", &db_ip, "-p", "5432", "-U", "odoo", "-t", "3"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !check.map(|s| s.success()).unwrap_or(false) {
        return Err(format!(
            "PostgreSQL en {}:5432 no responde. Asegúrate de que la cell esté arriba \
             (nkr cell up <cell>) o usa --no-db para saltar la clonación de DB.",
            db_ip
        ).into());
    }

    // Cada statement se envía por stdin para que psql lo ejecute en su propio
    // transaction block (auto-commit en modo no-interactivo). `CREATE DATABASE`
    // NO puede correr dentro de un BEGIN...COMMIT, así que -c con múltiples
    // statements fallaría ("CREATE DATABASE cannot run inside a transaction block").
    let sql = format!(
        "ALTER DATABASE \"{src}\" WITH ALLOW_CONNECTIONS false;\n\
         SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
           WHERE datname = '{src}' AND pid <> pg_backend_pid();\n\
         CREATE DATABASE \"{dst}\" WITH TEMPLATE \"{src}\" OWNER odoo;\n\
         ALTER DATABASE \"{src}\" WITH ALLOW_CONNECTIONS true;\n",
        src = src_db, dst = dst_db
    );

    eprintln!("[NKR-CLONE] Clonando DB '{}' → '{}' en {}:5432...", src_db, dst_db, db_ip);
    let out = run_psql_stdin(&db_ip, &sql);

    if out.as_ref().map(|o| !o.status.success()).unwrap_or(true) {
        // Reabrir conexiones al src si quedó capado (best-effort, no puede estar
        // en transacción con DROP/CREATE así que va por stdin separado).
        let _ = run_psql_stdin(&db_ip,
            &format!("ALTER DATABASE \"{}\" WITH ALLOW_CONNECTIONS true;\n", src_db));
        let msg = out.as_ref()
            .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
            .unwrap_or_else(|e| e.to_string());
        return Err(format!("psql falló: {}", msg).into());
    }

    eprintln!("[NKR-CLONE] DB '{}' creada desde TEMPLATE '{}'", dst_db, src_db);
    Ok(())
}

/// Ejecuta `psql` alimentando el SQL por stdin — cada statement corre en su propio
/// transaction block (auto-commit). Necesario para CREATE/DROP DATABASE que no
/// pueden correr dentro de un BEGIN...COMMIT implícito de `-c`.
fn run_psql_stdin(db_ip: &str, sql: &str) -> Result<std::process::Output, std::io::Error> {
    use std::io::Write;
    let mut child = std::process::Command::new("psql")
        .env("PGPASSWORD", "odoo")
        .args([
            "-h", db_ip, "-p", "5432", "-U", "odoo", "-d", "postgres",
            "-v", "ON_ERROR_STOP=1", "-X", "-q", "-f", "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(sql.as_bytes())?;
    }
    child.wait_with_output()
}

/// Anexa un bloque `services.<name>:` al nkr-compose.yml clonando el bloque del src.
/// Trabaja con el texto (no re-emite YAML) para preservar comentarios y formato.
fn append_compose_block(
    cell: &CellConfig,
    src_nkr: &str,
    dst_nkr: &str,
    dst_vm_id: u8,
) -> Result<(), Box<dyn std::error::Error>> {
    let compose_path = cell_compose_path(&cell.name);
    if !compose_path.exists() {
        return Err(format!("No existe {} — omite --no-compose para editar manual", compose_path.display()).into());
    }

    let content = fs::read_to_string(&compose_path)?;
    let lines: Vec<&str> = content.lines().collect();

    // 1. Localizar el bloque del src: buscamos el servicio cuyo `nkr_name: "<src>"`
    //    matchea exactamente. El bloque empieza en el header de servicio
    //    (2 espacios + key + ':') previo más cercano.
    let mut src_block_start: Option<usize> = None;
    let mut src_block_end: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("nkr_name:") {
            let rhs = trimmed.trim_start_matches("nkr_name:").trim().trim_matches('"').trim_matches('\'');
            if rhs == src_nkr {
                // Retroceder al header (2 espacios, no whitespace extra)
                for j in (0..=i).rev() {
                    let l = lines[j];
                    if l.len() >= 3
                        && l.starts_with("  ")
                        && !l.starts_with("   ")
                        && l.trim_end().ends_with(':')
                        && !l.trim_start().starts_with('-')
                    {
                        src_block_start = Some(j);
                        break;
                    }
                }
                break;
            }
        }
    }
    let block_start = src_block_start
        .ok_or_else(|| format!("No encontré el bloque 'services.*' con nkr_name: \"{}\"", src_nkr))?;

    // 2. Fin del bloque: siguiente header de servicio al mismo indent, o EOF / sección nueva.
    for i in (block_start + 1)..lines.len() {
        let l = lines[i];
        let is_service_header = l.len() >= 3
            && l.starts_with("  ")
            && !l.starts_with("   ")
            && l.trim_end().ends_with(':')
            && !l.trim_start().starts_with('-')
            && !l.trim_start().starts_with('#');
        let is_top_level = !l.is_empty() && !l.starts_with(' ') && !l.starts_with('#');
        if is_service_header || is_top_level {
            src_block_end = Some(i);
            break;
        }
    }
    let block_end = src_block_end.unwrap_or(lines.len());

    // 3. Clonar el bloque, sustituyendo:
    //    - Header (primera línea): `  odoo-XX:` → `  <short_dst>:`
    //    - `id: N` → `id: <dst_vm_id>`
    //    - Cualquier ocurrencia de src_nkr → dst_nkr
    let short_dst = dst_nkr
        .strip_prefix(&format!("{}-", cell.name))
        .unwrap_or(dst_nkr)
        .to_string();

    let mut new_block: Vec<String> = Vec::with_capacity(block_end - block_start + 2);
    new_block.push(String::new()); // línea en blanco separadora
    for (idx, line) in lines[block_start..block_end].iter().enumerate() {
        let mut s = line.to_string();
        if idx == 0 {
            // Header: `  odoo-01:` → `  <short_dst>:`
            s = format!("  {}:", short_dst);
        } else {
            let t = s.trim_start();
            if let Some(rest) = t.strip_prefix("id:") {
                let indent = &s[..s.len() - t.len()];
                s = format!("{}id: {}{}", indent, dst_vm_id, rest_trailing_comment(rest));
            } else {
                s = s.replace(src_nkr, dst_nkr);
            }
        }
        new_block.push(s);
    }

    // 4. Reescribir el fichero: contenido original + bloque nuevo al final
    let mut out = String::with_capacity(content.len() + 1024);
    out.push_str(&content);
    if !content.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&new_block.join("\n"));
    out.push('\n');

    // Backup antes de escribir
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bak = compose_path.with_extension(format!("yml.bak.{}", ts));
    let _ = fs::copy(&compose_path, &bak);

    fs::write(&compose_path, out)?;
    eprintln!("[NKR-CLONE] Compose actualizado: bloque '{}' añadido ({}). Backup: {}",
        short_dst, compose_path.display(), bak.display());
    Ok(())
}

/// Conserva cualquier comentario/espacio tras el valor de `id:`.
fn rest_trailing_comment(rest_after_id: &str) -> String {
    // `rest_after_id` incluye: " 3  # foo" o " 3"
    // Queremos extraer el posible `  # foo` (indent + comentario) si existe.
    let trimmed = rest_after_id.trim_start();
    if let Some(hash_pos) = trimmed.find('#') {
        // Reinsertar dos espacios + comentario
        return format!("  {}", &trimmed[hash_pos..]);
    }
    String::new()
}

/// Punto de entrada legacy (CLI): clona una instancia Odoo con opciones por defecto.
/// Para uso desde el API HTTP, llamar `clone_instance_with_opts`.
pub fn clone_instance(
    src_nkr: &str,
    dst_nkr: &str,
    no_db: bool,
    no_compose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let opts = CloneOptions {
        mode: if no_db { InstanceMode::Production } else { InstanceMode::Dev },
        no_compose,
        dns: None,
        edition: None,
        odoo_version: None,
        pg_version: None,
        workers: None,
        list_db: None,
        limit_memory_soft: None,
        limit_memory_hard: None,
        addons_path: None,
        python_libs: Vec::new(),
    };
    clone_instance_with_opts(src_nkr, dst_nkr, &opts).map(|_| ())
}

/// Clona + configura una instancia desde el API. Devuelve metadata del destino.
pub fn clone_instance_with_opts(
    src_nkr: &str,
    dst_nkr: &str,
    opts: &CloneOptions,
) -> Result<InstanceInfo, Box<dyn std::error::Error>> {
    if src_nkr == dst_nkr {
        return Err("src y dst son idénticos".into());
    }

    let (cell, src_dir) = find_instance_cell(src_nkr)?;
    let dst_dir = cells_dir().join(&cell.name).join("instances").join(dst_nkr);
    if dst_dir.exists() {
        return Err(format!("La instancia destino '{}' ya existe: {}", dst_nkr, dst_dir.display()).into());
    }

    // Validación versión: cada cell soporta una sola odoo_version. Si el panel
    // manda una versión, debe coincidir con la de la cell del source.
    if let Some(ref req_v) = opts.odoo_version {
        match cell.odoo_version.as_deref() {
            Some(cell_v) if cell_v == req_v => {}
            Some(cell_v) => return Err(format!(
                "Versión incompatible: source '{}' vive en cell '{}' (odoo_version={}), \
                 panel pidió odoo_version={}. Cada cell soporta una sola versión.",
                src_nkr, cell.name, cell_v, req_v
            ).into()),
            None => return Err(format!(
                "Cell '{}' no tiene odoo_version registrada (cell.yml) pero panel \
                 pidió odoo_version={}. Setear cell.odoo_version antes de clonar.",
                cell.name, req_v
            ).into()),
        }
    }

    // Validación capacidad: máximo 20 Odoos por cell.
    let used = count_odoo_instances(&cell.name);
    if used >= MAX_ODOOS_PER_CELL {
        return Err(format!(
            "Cell '{}' llena: {}/{} Odoos. Crear nueva cell o borrar instancia.",
            cell.name, used, MAX_ODOOS_PER_CELL
        ).into());
    }

    // Verificar si la VM src está activa. Si está, avisar — TEMPLATE en caliente
    // requiere desconectar sesiones. Se permite, pero hay una ventana de ~segundos.
    let active_src = crate::state::list_vms().iter().any(|v| v.name == src_nkr);
    if active_src && !opts.no_db() {
        eprintln!("[NKR-CLONE] WARN: '{}' está activo. La clonación cerrará sus sesiones \
                   PG por ~segundos para ejecutar CREATE DATABASE ... TEMPLATE.", src_nkr);
    }

    // python_libs no se soporta todavía: requiere build pipeline de master ext4
    // (nkr build con Nkrfile que haga pip install antes del exportar). Lo reportamos
    // como error explícito para que el panel sepa que tiene que llamar a /build primero.
    if !opts.python_libs.is_empty() {
        return Err(format!(
            "python_libs={:?} requiere rebuild del master ext4 vía 'nkr build'. \
             Endpoint /build pendiente — por ahora usá el master existente.",
            opts.python_libs
        ).into());
    }

    // Registrar nuevo vm_id (scope = cell)
    let dst_vm_id = crate::registry::resolve_id_scoped(Some(&cell.name), dst_nkr)?;
    let dst_ip = crate::registry::id_to_ip(cell.cell_id, dst_vm_id);

    // Estrategia de copia ordenada por preferencia:
    //  1. Si src es un subvolumen btrfs → `btrfs subvolume snapshot` (O(1) real).
    //  2. Si no, `cp -a --reflink=auto` (reflink en btrfs/xfs, copia física
    //     en ext4 o como fallback).
    //
    // Los .ext4 dentro del dir (filestore, pg/data.ext4) tienen `chattr +C`
    // aplicado en creación vía fsutil::create_ext4_disk/compose, así que el
    // snapshot no degrada su comportamiento CoW-free.
    eprintln!("[NKR-CLONE] Copiando {} → {} ...", src_dir.display(), dst_dir.display());

    let snapshot_ok = crate::fsutil::try_btrfs_snapshot(&src_dir, &dst_dir)
        .unwrap_or(false);

    if !snapshot_ok {
        let status = std::process::Command::new("cp")
            .args([
                "-a", "--reflink=auto",
                &src_dir.to_string_lossy(),
                &dst_dir.to_string_lossy(),
            ])
            .status()?;
        if !status.success() {
            return Err(format!("cp -a falló al copiar {} → {}", src_dir.display(), dst_dir.display()).into());
        }
    }

    // Limpiar logs del clon (no tiene sentido heredar logs del src)
    let dst_logs = dst_dir.join("logs");
    if dst_logs.exists() {
        for e in fs::read_dir(&dst_logs)?.flatten() {
            let _ = fs::remove_file(e.path());
        }
    }

    // Re-aplicar +C a los .ext4 clonados. btrfs subvolume snapshot y cp --reflink
    // NO heredan el flag NODATACOW → sin esto el nuevo odoo.ext4 fragmenta en CoW.
    for ext4 in walk_ext4_files(&dst_dir)? {
        if let Err(e) = crate::fsutil::preserve_nocow(&ext4) {
            eprintln!("[NKR-CLONE] WARN: preserve_nocow falló en {}: {}", ext4.display(), e);
        }
    }

    // Clonar archivos de `.nkr-data/` (filestore + pg-per-instance volumes).
    // Sin esto, el clon arranca con filestore vacío y Odoo tira FileNotFoundError
    // al buscar ir.attachment referenciadas en la DB clonada vía TEMPLATE.
    if let Err(e) = clone_nkr_data_files(&cell, src_nkr, dst_nkr) {
        eprintln!("[NKR-CLONE] WARN: clone_nkr_data_files: {} — filestore puede quedar vacío", e);
    }

    rewrite_odoo_conf(&dst_dir, src_nkr, dst_nkr)?;
    rewrite_odoo_conf_full(&dst_dir, dst_nkr, opts)?;

    if !opts.no_db() {
        clone_database(cell.cell_id, src_nkr, dst_nkr)?;
    } else {
        eprintln!("[NKR-CLONE] mode=production: DB no clonada. Crear manualmente antes de arrancar.");
    }

    if !opts.no_compose {
        append_compose_block(&cell, src_nkr, dst_nkr, dst_vm_id)?;
    } else {
        eprintln!("[NKR-CLONE] no_compose=true: añade el bloque al nkr-compose.yml manualmente.");
    }

    // Persistir metadata para el API (/instances/{name} GET)
    let meta = InstanceMeta {
        nkr_name: dst_nkr.to_string(),
        cell: cell.name.clone(),
        source: Some(src_nkr.to_string()),
        mode: opts.mode,
        dns: opts.dns.clone(),
        edition: opts.edition,
        odoo_version: opts.odoo_version.clone().or_else(|| cell.odoo_version.clone()),
        pg_version: opts.pg_version.clone(),
        workers: opts.workers,
        list_db: opts.list_db,
        limit_memory_soft: opts.limit_memory_soft,
        limit_memory_hard: opts.limit_memory_hard,
        addons_path: opts.addons_path.clone(),
        created_at: now_unix_secs(),
    };
    save_instance_meta(&dst_dir, &meta)?;

    eprintln!("[NKR-CLONE] ✅ '{}' clonado → '{}' (vm_id={}, IP={})",
        src_nkr, dst_nkr, dst_vm_id, dst_ip);
    if !opts.no_compose {
        eprintln!("[NKR-CLONE]   Arrancar: cd {} && sudo nkr compose up -d",
            cells_dir().join(&cell.name).display());
    }

    Ok(build_instance_info(&cell, dst_nkr, dst_vm_id, &meta))
}

// =============================================================================
// Tipos expuestos al API HTTP
// =============================================================================

/// Modo de instancia. Afecta si se clona la DB.
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum InstanceMode {
    /// Clon completo: copia archivos + clona DB (TEMPLATE). Cliente de desarrollo.
    Dev,
    /// Copia archivos pero DB vacía — el panel la hidrata aparte. Producción.
    Production,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Edition {
    Community,
    Enterprise,
}

/// Opciones de clonado enviadas por el panel.
#[derive(Clone, Debug, Default)]
pub struct CloneOptions {
    pub mode: InstanceMode,
    pub no_compose: bool,
    pub dns: Option<String>,
    pub edition: Option<Edition>,
    pub odoo_version: Option<String>,
    pub pg_version: Option<String>,
    pub workers: Option<u32>,
    pub list_db: Option<bool>,
    pub limit_memory_soft: Option<u64>, // bytes → odoo.conf
    pub limit_memory_hard: Option<u64>, // bytes → odoo.conf
    /// Si se pasa, sobrescribe `addons_path` en odoo.conf. En null se conserva el del src.
    pub addons_path: Option<String>,
    /// Librerías Python extra — requieren rebuild del master (no soportado hoy).
    pub python_libs: Vec<String>,
}

impl CloneOptions {
    pub fn no_db(&self) -> bool {
        matches!(self.mode, InstanceMode::Production)
    }
}

impl Default for InstanceMode {
    fn default() -> Self { InstanceMode::Production }
}

/// Metadata persistida por instancia (`meta.json` junto al dir de la instancia).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InstanceMeta {
    pub nkr_name: String,
    pub cell: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub mode: InstanceMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dns: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edition: Option<Edition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub odoo_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pg_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workers: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_db: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_memory_soft: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_memory_hard: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addons_path: Option<String>,
    pub created_at: u64,
}

/// Payload de respuesta que el panel consume.
#[derive(Serialize, Clone, Debug)]
pub struct InstanceInfo {
    pub nkr_name: String,
    pub cell: String,
    pub vm_id: u8,
    pub guest_ip: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dns: Option<String>,
    pub db_name: String,
    pub addons_path: String,
    pub logs_path: String,
    pub config_path: String,
    pub instance_dir: String,
    pub meta: InstanceMeta,
    pub nkr_status: NkrStatus,
}

/// Estado runtime: vivo/no-vivo, PID, puerto HTTP accesible.
#[derive(Serialize, Clone, Debug)]
pub struct NkrStatus {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<u64>,
    pub port_8069_up: bool,
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn instance_meta_path(instance_dir: &Path) -> PathBuf {
    instance_dir.join("meta.json")
}

pub fn save_instance_meta(instance_dir: &Path, meta: &InstanceMeta) -> Result<(), Box<dyn std::error::Error>> {
    let path = instance_meta_path(instance_dir);
    let json = serde_json::to_string_pretty(meta)?;
    fs::write(&path, json)?;
    Ok(())
}

pub fn load_instance_meta(instance_dir: &Path) -> Option<InstanceMeta> {
    let path = instance_meta_path(instance_dir);
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn build_instance_info(cell: &CellConfig, nkr_name: &str, vm_id: u8, meta: &InstanceMeta) -> InstanceInfo {
    let instance_dir = cells_dir().join(&cell.name).join("instances").join(nkr_name);
    let guest_ip = crate::registry::id_to_ip(cell.cell_id, vm_id);
    let running_vm = crate::state::list_vms().into_iter().find(|v| v.name == nkr_name);
    let port_up = tcp_probe(&guest_ip, 8069, std::time::Duration::from_millis(300));
    let (pid, ram_mb, uptime_s) = match &running_vm {
        Some(v) => (
            Some(v.pid),
            read_rss_mb(v.pid),
            started_at_secs(v.pid),
        ),
        None => (None, None, None),
    };
    InstanceInfo {
        nkr_name: nkr_name.to_string(),
        cell: cell.name.clone(),
        vm_id,
        guest_ip: guest_ip.clone(),
        dns: meta.dns.clone(),
        db_name: format!("db-{}", nkr_name),
        addons_path: instance_dir.join("addons").to_string_lossy().to_string(),
        logs_path: instance_dir.join("logs").join("odoo.log").to_string_lossy().to_string(),
        config_path: instance_dir.join("config").join("odoo.conf").to_string_lossy().to_string(),
        instance_dir: instance_dir.to_string_lossy().to_string(),
        meta: meta.clone(),
        nkr_status: NkrStatus {
            running: running_vm.is_some(),
            pid,
            ram_mb,
            uptime_s,
            port_8069_up: port_up,
        },
    }
}

fn tcp_probe(ip: &str, port: u16, timeout: std::time::Duration) -> bool {
    let addr = format!("{}:{}", ip, port);
    addr.parse()
        .ok()
        .and_then(|sa: std::net::SocketAddr| std::net::TcpStream::connect_timeout(&sa, timeout).ok())
        .is_some()
}

fn read_rss_mb(pid: u32) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

fn started_at_secs(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    // start_time es el campo 22 (después del comm entre paréntesis que puede tener espacios).
    let after_comm = stat.rsplit_once(')').map(|(_, r)| r)?.trim();
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // start_time es fields[19] (campo 22 del stat, con offset por el corte).
    let start_clock_ticks: u64 = fields.get(19)?.parse().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) as u64 };
    if hz == 0 { return None; }
    let boot = fs::read_to_string("/proc/stat").ok()?;
    let btime: u64 = boot.lines()
        .find_map(|l| l.strip_prefix("btime ").and_then(|s| s.parse().ok()))?;
    let started_unix = btime + start_clock_ticks / hz;
    let now = now_unix_secs();
    Some(now.saturating_sub(started_unix))
}

// =============================================================================
// Rewrite completo de odoo.conf con workers/list_db/memory limits/addons_path
// =============================================================================

/// Aplica los campos opcionales de `CloneOptions` al `odoo.conf` del destino.
/// Se ejecuta DESPUÉS de `rewrite_odoo_conf` (que ya reemplazó el nkr_name).
fn rewrite_odoo_conf_full(
    dst_dir: &Path,
    dst_nkr: &str,
    opts: &CloneOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let conf_path = dst_dir.join("config").join("odoo.conf");
    if !conf_path.exists() {
        eprintln!("[NKR-CLONE] WARN: odoo.conf no existe en {}, omitido full-rewrite", conf_path.display());
        return Ok(());
    }

    let content = fs::read_to_string(&conf_path)?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    // Siempre fijamos dbfilter al dst y list_db=False por seguridad (override si opts lo pide true).
    let db_name = format!("db-{}", dst_nkr);
    upsert_key(&mut lines, "dbfilter", &format!("^{}$", db_name));
    if let Some(list_db) = opts.list_db {
        upsert_key(&mut lines, "list_db", if list_db { "True" } else { "False" });
    }
    if let Some(workers) = opts.workers {
        upsert_key(&mut lines, "workers", &workers.to_string());
    }
    if let Some(soft) = opts.limit_memory_soft {
        upsert_key(&mut lines, "limit_memory_soft", &soft.to_string());
    }
    if let Some(hard) = opts.limit_memory_hard {
        upsert_key(&mut lines, "limit_memory_hard", &hard.to_string());
    }
    if let Some(ref ap) = opts.addons_path {
        upsert_key(&mut lines, "addons_path", ap);
    }
    // db_name lo fuerza el dbfilter; también lo dejamos explícito por si el conf lo tenía.
    upsert_key(&mut lines, "db_name", &db_name);

    fs::write(&conf_path, lines.join("\n") + "\n")?;
    eprintln!("[NKR-CLONE] odoo.conf actualizado: dbfilter, workers, list_db, limits");
    Ok(())
}

/// Inserta o reemplaza `key = value` en el array de líneas (estilo INI).
/// Busca bajo el header `[options]`. Si la key ya existe bajo cualquier sección, la reemplaza.
fn upsert_key(lines: &mut Vec<String>, key: &str, value: &str) {
    let target = format!("{} = {}", key, value);
    // Intento 1: reemplazo in-place si la key existe.
    for line in lines.iter_mut() {
        let trimmed = line.trim_start();
        if trimmed.starts_with(&format!("{} =", key))
            || trimmed.starts_with(&format!("{}=", key))
            || trimmed.starts_with(&format!("{} =", key))
        {
            *line = target.clone();
            return;
        }
    }
    // Intento 2: append bajo [options]. Si no existe el header, lo agregamos al final.
    if let Some(pos) = lines.iter().position(|l| l.trim() == "[options]") {
        lines.insert(pos + 1, target);
    } else {
        if !lines.is_empty() && !lines.last().unwrap().is_empty() {
            lines.push(String::new());
        }
        lines.push("[options]".to_string());
        lines.push(target);
    }
}

// =============================================================================
// Delete instance — stop VM, drop DB, remove dir, remove compose block
// =============================================================================

/// Elimina por completo una instancia: detiene VM, borra DB, remueve dir y compose block.
/// Idempotente: si algo no existe, sigue. Retorna el nombre de cell para el panel.
pub fn delete_instance(nkr_name: &str, drop_db: bool) -> Result<String, Box<dyn std::error::Error>> {
    let (cell, instance_dir) = find_instance_cell(nkr_name)?;

    // 1. Parar VM si corre
    let running = crate::state::list_vms().into_iter().find(|v| v.name == nkr_name);
    if let Some(vm) = running {
        eprintln!("[NKR-DELETE] Deteniendo VM '{}' (PID {})...", nkr_name, vm.pid);
        if let Err(e) = crate::state::stop_vm(vm.vm_id) {
            eprintln!("[NKR-DELETE] WARN: stop_vm falló: {} (continuando)", e);
        }
    }

    // 2. Drop DB
    if drop_db {
        if let Err(e) = drop_database(cell.cell_id, nkr_name) {
            eprintln!("[NKR-DELETE] WARN: drop DB falló: {} (continuando)", e);
        }
    }

    // 3. Remover bloque del compose YAML
    if let Err(e) = remove_compose_block(&cell, nkr_name) {
        eprintln!("[NKR-DELETE] WARN: no se pudo editar compose: {} (continuando)", e);
    }

    // 4. Liberar vm_id del registry de instancias (key scoped "cell/vm")
    let registry_key = format!("{}/{}", cell.name.to_lowercase(), nkr_name.to_lowercase());
    let _ = crate::registry::remove(&registry_key);

    // 5. Borrar directorio de instancia (datos persistentes del cliente)
    if instance_dir.exists() {
        fs::remove_dir_all(&instance_dir)?;
        eprintln!("[NKR-DELETE] dir removido: {}", instance_dir.display());
    }

    // 6. Limpiar archivos de `.nkr-data/` asociados (filestore + per-instance disks)
    //    Naming convention: `.nkr-data/<short_name>-<suffix>` o `.nkr-data/<short_name>-<suffix>.ext4`
    let cell_prefix = format!("{}-", cell.name);
    let short = nkr_name.strip_prefix(&cell_prefix).unwrap_or(nkr_name);
    let match_prefix = format!("{}-", short);
    let nkr_data = cells_dir().join(&cell.name).join(".nkr-data");
    if let Ok(it) = fs::read_dir(&nkr_data) {
        for e in it.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            if fname.starts_with(&match_prefix) {
                let p = e.path();
                if p.is_dir() {
                    let _ = fs::remove_dir_all(&p);
                } else {
                    let _ = fs::remove_file(&p);
                }
                eprintln!("[NKR-DELETE] nkr-data removido: {}", p.display());
            }
        }
    }

    eprintln!("[NKR-DELETE] ✅ instancia '{}' eliminada", nkr_name);
    Ok(cell.name)
}

fn drop_database(cell_id: u8, nkr_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let db_ip = format!("10.0.{}.2", cell_id);
    let db_name = format!("db-{}", nkr_name);

    let check = std::process::Command::new("pg_isready")
        .args(["-h", &db_ip, "-p", "5432", "-U", "odoo", "-t", "3"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !check.map(|s| s.success()).unwrap_or(false) {
        return Err(format!("PostgreSQL {}:5432 no responde — salteo drop DB", db_ip).into());
    }

    // Desconectar clientes primero, luego DROP. DROP DATABASE tampoco puede
    // correr dentro de transacción → enviamos por stdin (ver run_psql_stdin).
    let sql = format!(
        "ALTER DATABASE \"{db}\" WITH ALLOW_CONNECTIONS false;\n\
         SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
           WHERE datname = '{db}' AND pid <> pg_backend_pid();\n\
         DROP DATABASE IF EXISTS \"{db}\";\n",
        db = db_name
    );
    let out = run_psql_stdin(&db_ip, &sql)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("psql drop falló: {}", stderr.trim()).into());
    }
    eprintln!("[NKR-DELETE] DB '{}' eliminada en {}:5432", db_name, db_ip);
    Ok(())
}

fn remove_compose_block(cell: &CellConfig, nkr_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let compose_path = cell_compose_path(&cell.name);
    if !compose_path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(&compose_path)?;
    let lines: Vec<&str> = content.lines().collect();

    // Localizar el bloque por `nkr_name: "<name>"` — mismo criterio que append_compose_block.
    let mut blk_start: Option<usize> = None;
    let mut blk_end: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("nkr_name:") {
            let rhs = trimmed.trim_start_matches("nkr_name:").trim().trim_matches('"').trim_matches('\'');
            if rhs == nkr_name {
                for j in (0..=i).rev() {
                    let l = lines[j];
                    if l.len() >= 3
                        && l.starts_with("  ")
                        && !l.starts_with("   ")
                        && l.trim_end().ends_with(':')
                        && !l.trim_start().starts_with('-')
                    {
                        blk_start = Some(j);
                        break;
                    }
                }
                break;
            }
        }
    }
    let start = match blk_start { Some(s) => s, None => return Ok(()) };
    for i in (start + 1)..lines.len() {
        let l = lines[i];
        let is_service_header = l.len() >= 3
            && l.starts_with("  ")
            && !l.starts_with("   ")
            && l.trim_end().ends_with(':')
            && !l.trim_start().starts_with('-')
            && !l.trim_start().starts_with('#');
        let is_top_level = !l.is_empty() && !l.starts_with(' ') && !l.starts_with('#');
        if is_service_header || is_top_level {
            blk_end = Some(i);
            break;
        }
    }
    let end = blk_end.unwrap_or(lines.len());

    // Backup
    let ts = now_unix_secs();
    let bak = compose_path.with_extension(format!("yml.bak.{}", ts));
    let _ = fs::copy(&compose_path, &bak);

    let kept: Vec<String> = lines.iter().enumerate()
        .filter(|(i, _)| *i < start || *i >= end)
        .map(|(_, l)| l.to_string())
        .collect();
    fs::write(&compose_path, kept.join("\n") + "\n")?;
    eprintln!("[NKR-DELETE] bloque '{}' removido de {} (backup {})",
        nkr_name, compose_path.display(), bak.display());
    Ok(())
}

// =============================================================================
// Capacity / version planning — para auto-selección de cell en el API
// =============================================================================

/// Límite fijo de Odoos por cell (convención NKR). 20 Odoos + 1 PG + 1 PgB = 22 VMs
/// por cell, máximo 5 cells en 32 GB RAM = 110 VMs.
pub const MAX_ODOOS_PER_CELL: usize = 20;

/// Cuenta instancias Odoo bajo `cells/<cell>/instances/*` (PG y pgbouncer no
/// viven allí, así que cualquier dir cuenta como un Odoo deployado).
pub fn count_odoo_instances(cell_name: &str) -> usize {
    let dir = cells_dir().join(cell_name).join("instances");
    match fs::read_dir(&dir) {
        Ok(it) => it.flatten().filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false)).count(),
        Err(_) => 0,
    }
}

/// Capacity libre en una cell. `None` si la cell no existe.
pub fn cell_free_slots(cell_name: &str) -> Option<usize> {
    let reg = CellRegistry::load();
    reg.entries.get(&cell_name.to_lowercase())?;
    let used = count_odoo_instances(cell_name);
    Some(MAX_ODOOS_PER_CELL.saturating_sub(used))
}

/// Selecciona la primera cell con `odoo_version` coincidente y al menos 1 slot libre.
/// Ordena por cell_id ascendente (determinista). Retorna error si ninguna matchea
/// o todas están llenas.
pub fn select_cell_for_version(
    odoo_version: &str,
) -> Result<CellConfig, Box<dyn std::error::Error>> {
    let candidates: Vec<(CellConfig, usize)> = list_cells().into_iter()
        .filter(|c| c.odoo_version.as_deref() == Some(odoo_version))
        .map(|c| {
            let used = count_odoo_instances(&c.name);
            (c, used)
        })
        .collect();

    if candidates.is_empty() {
        return Err(format!(
            "No hay cells con odoo_version={}. Cells disponibles: {:?}",
            odoo_version,
            list_cells().iter().map(|c| format!("{}={}",
                c.name,
                c.odoo_version.as_deref().unwrap_or("?"))).collect::<Vec<_>>()
        ).into());
    }

    // Preferir la cell MENOS llena: balancea carga sin intervención del panel.
    let mut with_slots: Vec<_> = candidates.into_iter()
        .filter(|(_, used)| *used < MAX_ODOOS_PER_CELL)
        .collect();
    with_slots.sort_by_key(|(c, used)| (*used, c.cell_id));

    with_slots.into_iter().next()
        .map(|(c, _)| c)
        .ok_or_else(|| format!(
            "Todas las cells con odoo_version={} están llenas ({}/{} Odoos)",
            odoo_version, MAX_ODOOS_PER_CELL, MAX_ODOOS_PER_CELL
        ).into())
}

/// Resuelve el "source" para clonado cuando el panel no lo especifica.
/// Convención: primer instance dir ordenado alfabéticamente en la cell.
/// En una cell recién creada sin instancias, devuelve error.
pub fn default_source_in_cell(cell_name: &str) -> Result<String, Box<dyn std::error::Error>> {
    let dir = cells_dir().join(cell_name).join("instances");
    let mut names: Vec<String> = fs::read_dir(&dir)
        .map_err(|e| format!("Cell '{}' sin dir instances/: {}", cell_name, e))?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names.into_iter().next()
        .ok_or_else(|| format!("Cell '{}' no tiene instancias para usar como template", cell_name).into())
}

/// Normaliza el nkr_name aceptando forma corta (sin prefijo cell) o completa.
/// Ej. ensure_cell_prefix("nazcatex", "tst-1") → "nazcatex-tst-1"
///     ensure_cell_prefix("nazcatex", "nazcatex-tst-1") → "nazcatex-tst-1"
pub fn ensure_cell_prefix(cell_name: &str, nkr_name: &str) -> String {
    let prefix = format!("{}-", cell_name);
    if nkr_name.starts_with(&prefix) {
        nkr_name.to_string()
    } else {
        format!("{}{}", prefix, nkr_name)
    }
}

/// Walk recursivo — devuelve todos los `.ext4` bajo `dir`. Usado para
/// re-aplicar +C a cada disco clonado.
fn walk_ext4_files(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    fn recurse(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for e in fs::read_dir(dir)?.flatten() {
            let p = e.path();
            let ft = e.file_type()?;
            if ft.is_dir() { recurse(&p, out)?; }
            else if ft.is_file() && p.extension().map(|s| s == "ext4").unwrap_or(false) {
                out.push(p);
            }
        }
        Ok(())
    }
    recurse(dir, &mut out)?;
    Ok(out)
}

/// Clona los archivos de `.nkr-data/` asociados a `src_nkr` como equivalentes para
/// `dst_nkr`. Naming convention (ver `src/compose.rs:745`):
///   `.nkr-data/<short_name>-<guest_path_sanitized>.ext4`
/// donde `short_name = strip_prefix("<cell>-", nkr_name)`.
///
/// Ejemplo: para cell "nazcatex" al clonar "nazcatex-odoo-01" → "nazcatex-odoo-04"
///   - `odoo-01-var_lib_odoo.ext4` → `odoo-04-var_lib_odoo.ext4`
///
/// Usa `cp -a --reflink=auto` (O(1) en btrfs) y re-aplica `+C` vía preserve_nocow.
/// Si el dst ya existía (compose lo creó vacío on-demand), se sobrescribe.
fn clone_nkr_data_files(
    cell: &CellConfig,
    src_nkr: &str,
    dst_nkr: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let nkr_data = cells_dir().join(&cell.name).join(".nkr-data");
    if !nkr_data.exists() { return Ok(()); }

    let cell_prefix = format!("{}-", cell.name);
    let short_src = src_nkr.strip_prefix(&cell_prefix).unwrap_or(src_nkr);
    let short_dst = dst_nkr.strip_prefix(&cell_prefix).unwrap_or(dst_nkr);
    let match_prefix = format!("{}-", short_src);

    let mut cloned = 0usize;
    for entry in fs::read_dir(&nkr_data)?.flatten() {
        let fname_os = entry.file_name();
        let fname = fname_os.to_string_lossy().to_string();
        let suffix = match fname.strip_prefix(&match_prefix) {
            Some(s) => s,
            None => continue,
        };
        // Sólo clonamos archivos (ignora subdirs que empezaron con el prefijo por casualidad)
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) { continue; }

        let src_path = entry.path();
        let dst_path = nkr_data.join(format!("{}-{}", short_dst, suffix));

        if dst_path.exists() {
            let _ = fs::remove_file(&dst_path);
        }

        let status = std::process::Command::new("cp")
            .args([
                "-a", "--reflink=auto",
                &*src_path.to_string_lossy(),
                &*dst_path.to_string_lossy(),
            ])
            .status()?;
        if !status.success() {
            return Err(format!("cp nkr-data falló: {} → {}", src_path.display(), dst_path.display()).into());
        }

        // Re-aplicar +C si el archivo clonado es un .ext4 sobre btrfs.
        if dst_path.extension().map(|s| s == "ext4").unwrap_or(false) {
            if let Err(e) = crate::fsutil::preserve_nocow(&dst_path) {
                eprintln!("[NKR-CLONE] WARN: preserve_nocow falló en {}: {}", dst_path.display(), e);
            }
            // Si el .ext4 contiene un `filestore/db-<src>/` (típico para el
            // disco de /var/lib/odoo), renombrarlo a `filestore/db-<dst>/`
            // para que Odoo encuentre los attachments tras la clonación.
            if let Err(e) = rename_filestore_dir_inside(&dst_path, src_nkr, dst_nkr) {
                eprintln!("[NKR-CLONE] WARN: rename filestore en {}: {}", dst_path.display(), e);
            }
        }
        eprintln!("[NKR-CLONE] nkr-data: {} → {}",
            src_path.file_name().unwrap().to_string_lossy(),
            dst_path.file_name().unwrap().to_string_lossy());
        cloned += 1;
    }
    if cloned > 0 {
        eprintln!("[NKR-CLONE] {} archivos .nkr-data clonados de '{}' → '{}'", cloned, src_nkr, dst_nkr);
    }
    Ok(())
}

/// Monta el `.ext4` en un tmp dir y renombra `filestore/db-<src>/` →
/// `filestore/db-<dst>/`. No-op si el dir no existe (el `.ext4` puede ser para
/// otra cosa, no `/var/lib/odoo`).
///
/// Requiere root (mount loop). El server HTTP corre bajo sudo, así que OK.
fn rename_filestore_dir_inside(
    ext4_path: &Path,
    src_nkr: &str,
    dst_nkr: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mnt_base = format!("/tmp/nkr-clone-mnt-{}-{}",
        std::process::id(),
        ext4_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
    fs::create_dir_all(&mnt_base)?;

    let mount_status = std::process::Command::new("mount")
        .args(["-o", "loop", &*ext4_path.to_string_lossy(), &mnt_base])
        .status()?;
    if !mount_status.success() {
        let _ = fs::remove_dir(&mnt_base);
        return Err(format!("mount loop {} falló", ext4_path.display()).into());
    }

    // Helper para siempre desmontar al retornar
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let src_db = format!("db-{}", src_nkr);
        let dst_db = format!("db-{}", dst_nkr);
        let src_filestore = Path::new(&mnt_base).join("filestore").join(&src_db);
        let dst_filestore = Path::new(&mnt_base).join("filestore").join(&dst_db);
        if !src_filestore.exists() {
            // No es un disco de /var/lib/odoo, nada que renombrar
            return Ok(());
        }
        if dst_filestore.exists() {
            // El dst ya existía (improbable tras clone limpio); no pisar
            eprintln!("[NKR-CLONE] WARN: {} ya existe, no renombro", dst_filestore.display());
            return Ok(());
        }
        fs::rename(&src_filestore, &dst_filestore)?;
        eprintln!("[NKR-CLONE] filestore renombrado: {} → {}", src_db, dst_db);
        Ok(())
    })();

    let _ = std::process::Command::new("umount").arg(&mnt_base).status();
    let _ = fs::remove_dir(&mnt_base);
    result
}

/// Devuelve InstanceInfo para una instancia existente (para GET del API).
pub fn get_instance_info(nkr_name: &str) -> Result<InstanceInfo, Box<dyn std::error::Error>> {
    let (cell, dir) = find_instance_cell(nkr_name)?;
    let meta = load_instance_meta(&dir).unwrap_or_else(|| {
        // Instancia pre-existente al API (creada a mano): meta minima deducida.
        InstanceMeta {
            nkr_name: nkr_name.to_string(),
            cell: cell.name.clone(),
            source: None,
            mode: InstanceMode::Production,
            dns: None,
            edition: None,
            odoo_version: cell.odoo_version.clone(),
            pg_version: None,
            workers: None,
            list_db: None,
            limit_memory_soft: None,
            limit_memory_hard: None,
            addons_path: None,
            created_at: 0,
        }
    });
    let vm_id = crate::registry::resolve_id_scoped(Some(&cell.name), nkr_name)?;
    Ok(build_instance_info(&cell, nkr_name, vm_id, &meta))
}


// =============================================================================
// NKR Watchdog — Detección y restart automático de tenants colgados
// =============================================================================
//
// Runs en un thread del daemon `nkr serve`. Cada `PROBE_INTERVAL_SECS` (15s):
//
//   1. Para cada VM running, hace probe TCP rápida a :8069.
//   2. Si falla, registra `hung_since=now()` en estado in-memory.
//   3. Si la próxima probe es OK, limpia el estado.
//   4. Si la VM lleva `HUNG_THRESHOLD_SECS` (60s) consecutivos sin :8069,
//      dispara `nkr_action(restart)` async — equivale al restart manual del
//      panel pero sin necesidad de operador en vivo.
//
// Por qué existe (postmortem 2026-05-11):
//   Bug observado en intech-devp donde Odoo dentro del guest queda colgado
//   (D-state o deadlock interno) y no responde a REL_OD vía SIGUSR1. Causa
//   raíz desconocida (no es race wipe/balloon/memory limit/WS — todos
//   descartados). Sin shell al guest no se puede diagnosticar más. El
//   watchdog cubre la sintomática: si no responde por 60s, restart.
//
// Bypass: env var `NKR_WATCHDOG_DISABLED=1` deshabilita el loop completo.
//
// Filosofía: conservador. Sólo dispara cuando running=true Y port_8069_up=false
// por tiempo SOSTENIDO. NUNCA toca VMs apagadas explícitamente (running=false)
// — esas tienen su propio control del panel.
// =============================================================================

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// Cada cuánto sondea cada VM. Bajo = detección rápida + overhead bajo
/// (1 TCP probe / VM / 15s ≈ 0.07 RPS por VM, trivial).
const PROBE_INTERVAL_SECS: u64 = 15;

/// Cuánto tiempo de :8069 unreachable antes de disparar restart. Conservador:
/// un Odoo cargando módulos pesados o procesando un import grande puede
/// tardar 30-50s en responder. 60s deja margen sin ser demasiado paciente.
const HUNG_THRESHOLD_SECS: u64 = 60;

/// Timeout de la probe TCP. Bajo: si la VM está colgada no nos colgamos
/// nosotros. Si la VM está lenta (no colgada), 2s da margen suficiente.
const PROBE_TIMEOUT_MS: u64 = 2000;

/// Estado in-memory: nkr_name → unix_ts del primer probe fallido consecutivo.
/// Se limpia al primer probe OK. Se limpia tras disparar restart (para no
/// retriggerar inmediatamente; el restart en sí toma ~25s, después el next
/// probe encontrará el puerto down esperando boot — eso es OK y NO debe
/// contar como nuevo cuelgue).
static HUNG_SINCE: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn tcp_probe(ip: &str, port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    use std::str::FromStr;
    let addr = match SocketAddr::from_str(&format!("{}:{}", ip, port)) {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(PROBE_TIMEOUT_MS)).is_ok()
}

/// Entry point del watchdog. Bloquea para siempre — spawnearlo en thread
/// dedicado desde `ipc_server::run`.
pub fn run_loop() {
    if std::env::var("NKR_WATCHDOG_DISABLED").is_ok() {
        eprintln!("[NKR-WATCHDOG] deshabilitado por NKR_WATCHDOG_DISABLED");
        return;
    }
    eprintln!("[NKR-WATCHDOG] iniciado (probe={}s, threshold={}s)",
        PROBE_INTERVAL_SECS, HUNG_THRESHOLD_SECS);
    // Inicializar mapa (Mutex<Option> para const-construct + lazy init).
    *HUNG_SINCE.lock().unwrap() = Some(HashMap::new());
    // Grace period inicial para no pisar el boot del daemon.
    std::thread::sleep(Duration::from_secs(30));
    loop {
        sweep();
        std::thread::sleep(Duration::from_secs(PROBE_INTERVAL_SECS));
    }
}

fn sweep() {
    let vms = crate::state::list_vms();
    let now = now_secs();
    // Solo tenants Odoo: cell_id > 0 (cell_id=0 es el bridge legacy, ahí
    // viven things como pgbouncer/postgres por nombre `*-db` / `*-pgb`
    // que no exponen :8069). Heurística por nombre: si termina en -db
    // o -pgb, skip.
    for vm in &vms {
        if vm.name.ends_with("-db") || vm.name.ends_with("-pgb") {
            continue;
        }
        let ip = crate::registry::id_to_ip(vm.cell_id, vm.vm_id);
        let up = tcp_probe(&ip, 8069);
        let mut guard = HUNG_SINCE.lock().unwrap();
        let map = guard.as_mut().expect("watchdog map not initialized");
        if up {
            // Recuperado: limpiar marca si existía
            if map.remove(&vm.name).is_some() {
                eprintln!("[NKR-WATCHDOG] {} :8069 recuperado",
                    vm.name);
            }
            continue;
        }
        // Probe falló. Marcar si es la primera vez O chequear si pasó el
        // threshold.
        let hung_since = *map.entry(vm.name.clone()).or_insert(now);
        let elapsed = now.saturating_sub(hung_since);
        if elapsed < HUNG_THRESHOLD_SECS {
            // Aún dentro del margen — sólo log a cada 30s para no llenar journal
            if elapsed % 30 == 0 {
                eprintln!("[NKR-WATCHDOG] {} :8069 down hace {}s (threshold {}s)",
                    vm.name, elapsed, HUNG_THRESHOLD_SECS);
            }
            continue;
        }
        // Cruzó el threshold → trigger restart. Limpiar marca para no
        // retriggerar mientras el restart está en vuelo. El próximo probe
        // post-restart encontrará :8069 down (boot ~25s), pero el state
        // limpio significa que recién en `t + 25 + 60 = t+85s` podría
        // re-disparar si el boot fallara también. Esa cadena igual es
        // detectable y manejable manualmente.
        map.remove(&vm.name);
        drop(guard); // liberar el lock antes de hacer la llamada async

        eprintln!("[NKR-WATCHDOG] {} colgado {}s sin :8069 — disparando restart automático",
            vm.name, elapsed);
        trigger_restart(&vm.name);
    }
}

/// Dispara `nkr_action(restart)` en background. Reusa el flow de
/// `api::handle_action` que ya hace start-action async.
fn trigger_restart(nkr_name: &str) {
    let name = nkr_name.to_string();
    std::thread::Builder::new()
        .name(format!("nkr-watchdog-restart-{}", nkr_name))
        .spawn(move || {
            let resp = crate::api::handle_action(&name, "restart");
            if resp.status >= 400 {
                eprintln!("[NKR-WATCHDOG] {} restart trigger error status={} body={}",
                    name, resp.status, resp.body);
            } else {
                eprintln!("[NKR-WATCHDOG] {} restart dispatched (HTTP {})",
                    name, resp.status);
            }
        })
        .ok();
}

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
/// tardar 30-50s en responder. 120s deja margen amplio sin ser excesivo
/// (subido de 60→120 el 2026-05-15: REL_OD legítimo con 72 módulos custom
/// en intech-devp tardó 90s post-shutdown, el watchdog disparó restart
/// innecesario y duplicó el downtime de ~30s → ~150s).
const HUNG_THRESHOLD_SECS: u64 = 120;

/// Grace window post-REL_OD. Cuando `api::handle_reload_workers` mete un
/// SIGUSR1, llama a `note_reload(nkr_name)` que registra el timestamp acá.
/// Mientras `now - last_reload < RELOAD_GRACE_SECS`, el threshold efectivo
/// es `RELOAD_THRESHOLD_SECS` (180s) en vez de 120s, porque un reload con
/// muchos módulos puede legítimamente tener :8069 down durante 90-150s.
const RELOAD_GRACE_SECS: u64 = 240;
const RELOAD_THRESHOLD_SECS: u64 = 180;

/// Timeout de la probe TCP. Bajo: si la VM está colgada no nos colgamos
/// nosotros. Si la VM está lenta (no colgada), 2s da margen suficiente.
const PROBE_TIMEOUT_MS: u64 = 2000;

/// Estado in-memory: nkr_name → unix_ts del primer probe fallido consecutivo.
/// Se limpia al primer probe OK. Se limpia tras disparar restart (para no
/// retriggerar inmediatamente; el restart en sí toma ~25s, después el next
/// probe encontrará el puerto down esperando boot — eso es OK y NO debe
/// contar como nuevo cuelgue).
static HUNG_SINCE: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// Estado in-memory: nkr_name → unix_ts del último REL_OD inyectado.
/// Lo escribe `api::handle_reload_workers` vía `note_reload()`. Lo lee
/// `sweep()` para extender el threshold si el reload es reciente.
static LAST_RELOAD: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// Llamado por `api::handle_reload_workers` justo después de SIGUSR1. Permite
/// al watchdog dar grace adicional durante el ciclo natural de REL_OD (que
/// con muchos módulos puede tener :8069 down 90-150s).
pub fn note_reload(nkr_name: &str) {
    let mut guard = LAST_RELOAD.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(nkr_name.to_string(), now_secs());
}

fn effective_threshold(nkr_name: &str, now: u64) -> u64 {
    let guard = LAST_RELOAD.lock().unwrap();
    if let Some(map) = guard.as_ref() {
        if let Some(&ts) = map.get(nkr_name) {
            if now.saturating_sub(ts) < RELOAD_GRACE_SECS {
                return RELOAD_THRESHOLD_SECS;
            }
        }
    }
    HUNG_THRESHOLD_SECS
}

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
    eprintln!("[NKR-WATCHDOG] iniciado (probe={}s, threshold={}s, reload_grace={}s/{}s)",
        PROBE_INTERVAL_SECS, HUNG_THRESHOLD_SECS, RELOAD_GRACE_SECS, RELOAD_THRESHOLD_SECS);
    // Inicializar mapas (Mutex<Option> para const-construct + lazy init).
    *HUNG_SINCE.lock().unwrap() = Some(HashMap::new());
    *LAST_RELOAD.lock().unwrap() = Some(HashMap::new());
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
        let threshold = effective_threshold(&vm.name, now);
        if elapsed < threshold {
            // Aún dentro del margen — sólo log a cada 30s para no llenar journal
            if elapsed % 30 == 0 {
                eprintln!("[NKR-WATCHDOG] {} :8069 down hace {}s (threshold {}s)",
                    vm.name, elapsed, threshold);
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
///
/// **Coordination guard (audit 2026-05-15, fix C6):** antes de disparar,
/// intenta `InstanceLock::try_acquire` para detectar si hay otra operación
/// del operador/panel ya en vuelo sobre esta misma instancia (delete, start
/// manual, restart manual). Si no se puede obtener el lock → otro proceso
/// ya está actuando → el watchdog NO compite (skip). Esto cierra el race:
///   operador hace `nkr stop X` ↔ stop_vm está en su loop de 90s ↔ :8069
///   cae ↔ watchdog detecta colgado ↔ ANTES disparaba restart en paralelo
///   con el stop del operador → caos. Ahora el watchdog ve el lock tomado
///   y se hace a un lado.
///
/// Lo soltamos antes del thread::spawn porque el handle_action toma sus
/// propios guards internos (inflight_actions) — no queremos doble-lock.
fn trigger_restart(nkr_name: &str) {
    let name = nkr_name.to_string();
    // Resolver cell para construir el lock path. Si no encontramos la cell,
    // omitimos el guard (best-effort) y procedemos como antes.
    let cell_opt = crate::state::list_vms().into_iter()
        .find(|v| v.name == nkr_name)
        .map(|v| v.cell_id)
        .and_then(crate::cell::lookup_cell_name);
    if let Some(cell) = cell_opt.as_deref() {
        match crate::inst_lock::InstanceLock::try_acquire(cell, nkr_name) {
            Ok(Some(_lock)) => {
                // OK, libre — soltamos inmediatamente (handle_action toma
                // sus propios guards). El _lock vive solo en este scope.
            }
            Ok(None) => {
                eprintln!("[NKR-WATCHDOG] {} skip restart — instance lock tomado \
                          (operador/panel está haciendo algo)", nkr_name);
                return;
            }
            Err(e) => {
                eprintln!("[NKR-WATCHDOG] {} lock check error: {} — procediendo igual",
                    nkr_name, e);
            }
        }
    }
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

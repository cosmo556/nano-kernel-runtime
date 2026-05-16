// =============================================================================
// nkr-watcher — guest-side hvc0 dispatcher
// =============================================================================
//
// Reemplaza el subshell busybox de initramfs.rs:695 ("Clean-shutdown watcher").
// Diseñado para ser robusto bajo la race rara del shell + virtio-console que
// causaba cuelgues en el N-ésimo reload consecutivo (audit 2026-05-15).
//
// Diferencias clave vs el subshell shell:
//   - File descriptor de /dev/hvc0 PERSISTENTE (open ONCE).
//   - Buffered reader propio sobre el fd, no close/reopen per iter.
//   - Logging directo a /newroot/var/log/odoo/nkr-watcher.log via std::fs.
//   - kill(2) llamado vía libc (no fork+pkill).
//
// Compilado estático con musl: el binario es self-contained, no depende de
// glibc ni de busybox. Vive en `/bin/nkr-watcher` del initramfs (embebido
// como `include_bytes!` desde el build principal de NKR).
//
// Invocación: el init script lo lanza como
//     /bin/nkr-watcher --label <NAME> &
//
// Protocolo hvc0 (mensajes separados por \n):
//   REL_OD     → reload de Odoo. Lee /newroot/tmp/odoo.pid + workers de
//                /newroot/etc/odoo/odoo.conf. SIGKILL si workers=0 (threaded),
//                SIGHUP si workers>0 (prefork).
//   SHUTDOWN   → apagado limpio (no implementado todavía — el init script
//                viejo sigue handling SHUTDOWN para no romper compat).
//
// Output:
//   stdout/stderr → /dev/ttyS0 si lo abrimos explícito; sino tirado.
//   log file     → /newroot/var/log/odoo/nkr-watcher.log (visible desde host).
// =============================================================================

use std::env;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const HVC0_PATH: &str = "/dev/hvc0";
const ODOO_PID_PATH: &str = "/newroot/tmp/odoo.pid";
const ODOO_CONF_PATH: &str = "/newroot/etc/odoo/odoo.conf";
const LOG_PATH: &str = "/newroot/var/log/odoo/nkr-watcher.log";

/// Path al script de SHUTDOWN — set por --shutdown-cmd al inicio. Leído por
/// `handle_shutdown` cuando llega un SHUTDOWN por hvc0.
static mut SHUTDOWN_CMD: &'static str = "/bin/nkr-shutdown.sh";

fn main() {
    let mut args = env::args().skip(1);
    let mut label = String::from("NKR");
    let mut shutdown_cmd = String::from("/bin/nkr-shutdown.sh");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--label" => {
                if let Some(v) = args.next() { label = v; }
            }
            "--shutdown-cmd" => {
                if let Some(v) = args.next() { shutdown_cmd = v; }
            }
            _ => {}
        }
    }
    let label = label;
    // Stash shutdown_cmd in a static-ish location via a closure-passed
    // string. main() is simple so we just thread it down.
    unsafe { SHUTDOWN_CMD = Box::leak(shutdown_cmd.into_boxed_str()); }

    // Crear directorio del log file (es virtio-fs share, ya debería existir
    // como mountpoint, pero idempotente).
    if let Some(parent) = Path::new(LOG_PATH).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    log(&label, &format!("watcher (Rust) iniciado, PID={}, parent={}",
        std::process::id(), unsafe { libc::getppid() }));

    // Esperar a que /dev/hvc0 aparezca (init paralelo puede crearlo tras
    // un breve delay). Polling 100ms hasta 60s.
    let mut waited_ms = 0u32;
    while !Path::new(HVC0_PATH).exists() {
        if waited_ms >= 60_000 {
            log(&label, "ERROR: /dev/hvc0 no apareció tras 60s — abortando");
            std::process::exit(1);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        waited_ms += 100;
    }
    log(&label, &format!("/dev/hvc0 listo (esperó {}ms) — abriendo fd persistente", waited_ms));

    log(&label, "entrando al loop con libc::open + read raw (sin BufReader)");
    // Test 3 de I/O patterns:
    //   1. BufReader persistente — falla, no despierta tras primer mensaje
    //   2. File::open + BufReader per iter — falla igual
    //   3. libc::open + read raw byte-by-byte — usamos esta porque replica
    //      EXACTAMENTE lo que el shell hace internamente al read.
    //
    // Estrategia: open hvc0, read bytes uno por uno hasta encontrar \n,
    // formar la línea, close. Reabrir y repetir. read(2) syscall directo,
    // sin buffering intermedio.
    use std::ffi::CString;
    let c_path = CString::new(HVC0_PATH).unwrap();
    let mut iter = 0u64;
    loop {
        iter += 1;
        log(&label, &format!("iter={}: libc::open(/dev/hvc0, O_RDONLY)", iter));
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            log(&label, &format!("iter={}: open ERROR {} — sleep 1s", iter, e));
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        // Lee bytes hasta encontrar \n. Buffer cap 256 (mensajes son cortos:
        // "REL_OD\n" = 7, "SHUTDOWN\n" = 9).
        let mut line: Vec<u8> = Vec::with_capacity(64);
        let mut byte = [0u8; 1];
        loop {
            let n = unsafe { libc::read(fd, byte.as_mut_ptr() as *mut _, 1) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                log(&label, &format!("iter={}: read ERROR {} — abandono esta iter", iter, e));
                break;
            }
            if n == 0 {
                // EOF — virtio-console raramente devuelve 0; pero por defensa
                log(&label, &format!("iter={}: read 0 bytes (EOF) — close+retry", iter));
                break;
            }
            if byte[0] == b'\n' { break; }
            if byte[0] == b'\r' { continue; } // skip CR
            line.push(byte[0]);
            if line.len() >= 256 {
                log(&label, &format!("iter={}: línea >=256 bytes — truncando", iter));
                break;
            }
        }
        unsafe { libc::close(fd); }
        let msg = String::from_utf8_lossy(&line).to_string();
        if msg.is_empty() {
            // Read interrumpido o vacío — corto sleep para no spin.
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }
        log(&label, &format!("iter={}: línea recibida ({}b): '{}'", iter, line.len(), msg));
        dispatch(&label, &msg);
    }
}

fn dispatch(label: &str, cmd: &str) {
    match cmd {
        "REL_OD" => handle_rel_od(label),
        "SHUTDOWN" => handle_shutdown(label),
        _ => log(label, &format!("cmd desconocido '{}' — ignorado", cmd)),
    }
}

fn handle_rel_od(label: &str) {
    // 1) Detectar modo: workers=0 (threaded) vs workers>0 (prefork)
    let workers = read_workers(ODOO_CONF_PATH);
    log(label, &format!("REL_OD: workers detectados='{}'", workers));

    // 2) Leer PID del supervisor
    let pid_str = std::fs::read_to_string(ODOO_PID_PATH).unwrap_or_default();
    let pid: i32 = pid_str.trim().parse().unwrap_or(0);
    log(label, &format!("REL_OD: leyendo {} → '{}'", ODOO_PID_PATH, pid_str.trim()));

    if pid <= 1 {
        log(label, "REL_OD: PID file ausente/inválido → fallback pkill -KILL -f /usr/bin/odoo");
        pkill_odoo();
        return;
    }
    // 3) Verificar que el proceso está vivo
    if !pid_alive(pid) {
        log(label, &format!("REL_OD: PID {} no está vivo → fallback pkill", pid));
        pkill_odoo();
        return;
    }
    // 4) Mandar señal correspondiente
    if workers == 0 {
        log(label, &format!("REL_OD: PID {} vivo, enviando SIGKILL (threaded)", pid));
        let rc = unsafe { libc::kill(pid, libc::SIGKILL) };
        log(label, &format!("REL_OD: kill -KILL rc={}", rc));
    } else {
        log(label, &format!("REL_OD: master PID {} vivo, enviando SIGHUP (prefork, workers={})",
            pid, workers));
        let rc = unsafe { libc::kill(pid, libc::SIGHUP) };
        log(label, &format!("REL_OD: kill -HUP rc={}", rc));
    }
}

fn handle_shutdown(label: &str) {
    let cmd = unsafe { SHUTDOWN_CMD };
    log(label, &format!("SHUTDOWN: exec'ing {} (logica de cleanup en shell)", cmd));
    // execv reemplaza nuestro proceso por el script de shutdown. No volvemos.
    // Si exec falla (script ausente, no ejecutable, etc.) caemos al fallback:
    // sync + reboot directo via syscall.
    use std::ffi::CString;
    if let Ok(c_cmd) = CString::new(cmd) {
        let args = [c_cmd.as_ptr(), std::ptr::null()];
        unsafe { libc::execv(c_cmd.as_ptr(), args.as_ptr()); }
        // Si llegamos acá, execv falló.
        log(label, &format!("SHUTDOWN: execv {} falló — fallback sync+reboot", cmd));
    }
    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_RESTART);
    }
}

/// Lee `workers = N` del odoo.conf. Retorna 0 si no encuentra o no parsea.
fn read_workers(path: &str) -> u32 {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    for line in content.lines() {
        let t = line.trim_start();
        if t.starts_with('#') || t.starts_with(';') || t.starts_with('[') {
            continue;
        }
        if let Some(rest) = t.strip_prefix("workers") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let v = rest.trim();
                if let Ok(n) = v.parse::<u32>() {
                    return n;
                }
            }
        }
    }
    0
}

fn pid_alive(pid: i32) -> bool {
    if pid <= 1 { return false; }
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Fallback: matar todo proceso con `/usr/bin/odoo` en su cmdline (scaneando
/// /proc). Equivalente al `pkill -f` de busybox.
fn pkill_odoo() {
    let proc_dir = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return,
    };
    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let pid: i32 = match name_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if pid <= 1 { continue; }
        let cmdline_path = format!("/proc/{}/cmdline", pid);
        let cmdline = match std::fs::read_to_string(&cmdline_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if cmdline.contains("/usr/bin/odoo") {
            let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn log(label: &str, msg: &str) {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let line = format!("[ts={} pid={}] [{}] {}\n",
        ts, std::process::id(), label, msg);
    // Best-effort: open+append+close por cada línea. Cero buffering del shell.
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(LOG_PATH) {
        let _ = f.write_all(line.as_bytes());
    }
    // También a stderr (puede o no ir a algún lugar visible).
    eprint!("{}", line);
}

// =============================================================================
// NKR IPC Server — Listens on /var/run/nkr.sock and dispatches IpcRequest
// =============================================================================
//
// Started by `nkr serve`. Runs with root privileges (required for cgroup
// writes, TAP creation, iptables, pg_isready against local PG, etc.).
//
// The unprivileged nkr-api-server connects over UDS, sends one IpcRequest
// per connection, and reads one IpcResponse. This module owns the listener,
// dispatch, and bounded concurrency.
//
// Socket perms are set after bind: 0660, owner root:nkr-api. If the nkr-api
// group does not exist the chown fails and we fall back to 0660 root:root —
// operators must then run the proxy as root too (unusual for prod).
// =============================================================================

use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::api;
use crate::ipc::{read_frame, socket_path, write_frame, IpcRequest, IpcResponse};

/// Bounded concurrency for the UDS listener. Each live IPC call occupies one
/// slot; over this we reply 503-equivalent and drop the connection.
const MAX_INFLIGHT: u32 = 64;

struct InflightGuard {
    counter: Arc<AtomicU32>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Start the UDS server. Blocks the caller's thread — spawn it separately
/// if the caller must continue.
pub fn run() -> std::io::Result<()> {
    let path = socket_path();

    // Remove stale socket from a previous run (only if it is actually a socket).
    if path.exists() {
        if let Ok(meta) = fs::metadata(&path) {
            if meta.file_type().is_socket() {
                let _ = fs::remove_file(&path);
            } else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("{} existe y no es un socket — no lo toco", path.display()),
                ));
            }
        }
    }

    // Ensure parent dir exists (e.g. /var/run/).
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let listener = UnixListener::bind(&path)?;
    set_socket_perms(&path);
    eprintln!(
        "[NKR-IPC] UDS server escuchando en {} (mode=0660 root:nkr-api)",
        path.display()
    );

    // Lanzar thread de mantenimiento de recursos (mounts/cgroups/loops/locks
    // huérfanos). Corre cada 5 min, idempotente, sólo borra cosas no
    // referenciadas por procesos vivos.
    std::thread::Builder::new()
        .name("nkr-janitor".into())
        .spawn(crate::janitor::run_loop)
        .ok();

    let inflight = Arc::new(AtomicU32::new(0));

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[NKR-IPC] accept: {}", e);
                continue;
            }
        };

        let current = inflight.load(Ordering::Relaxed);
        if current >= MAX_INFLIGHT {
            // Best-effort 503 — proxy should reconnect.
            let resp = IpcResponse::error(503, "server_busy", Some("retry in 1s"));
            let mut s = stream;
            let _ = write_frame(&mut s, &resp);
            continue;
        }
        inflight.fetch_add(1, Ordering::Relaxed);
        let inflight_clone = Arc::clone(&inflight);

        std::thread::spawn(move || {
            let _guard = InflightGuard {
                counter: inflight_clone,
            };
            handle_connection(stream);
        });
    }
    Ok(())
}

fn set_socket_perms(path: &Path) {
    use std::os::unix::fs::MetadataExt;

    // 0660 first — even if chown fails the socket stays tight.
    if let Ok(meta) = fs::metadata(path) {
        let mut perm = meta.permissions();
        perm.set_mode(0o660);
        if let Err(e) = fs::set_permissions(path, perm) {
            eprintln!("[NKR-IPC] WARN: no pude chmod 0660 {}: {}", path.display(), e);
        }
    }

    // Try to chown root:nkr-api. If nkr-api group does not exist, leave as root:root.
    let gid = lookup_group_gid("nkr-api");
    match gid {
        Some(g) => {
            let cpath = match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
                Ok(c) => c,
                Err(_) => return,
            };
            let current_uid = match fs::metadata(path) {
                Ok(m) => m.uid(),
                Err(_) => 0,
            };
            let ret = unsafe { libc::chown(cpath.as_ptr(), current_uid, g) };
            if ret != 0 {
                eprintln!(
                    "[NKR-IPC] WARN: chown root:nkr-api {} falló: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                );
            } else {
                eprintln!("[NKR-IPC] socket chown root:nkr-api (gid={})", g);
            }
        }
        None => {
            eprintln!(
                "[NKR-IPC] WARN: grupo 'nkr-api' no existe — socket queda root:root. \
                 Crea el grupo: `sudo groupadd -r nkr-api` y el usuario de \
                 nkr-api-server debe pertenecer a él."
            );
        }
    }
}

fn lookup_group_gid(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let grp = unsafe { libc::getgrnam(cname.as_ptr()) };
    if grp.is_null() {
        return None;
    }
    Some(unsafe { (*grp).gr_gid })
}

fn handle_connection(mut stream: UnixStream) {
    // Per-connection timeout: 120s covers the longest operation (clone ~30-40s).
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(120)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(120)));

    let req: IpcRequest = match read_frame(&mut stream) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[NKR-IPC] read_frame: {}", e);
            return;
        }
    };

    let resp = dispatch(req);

    if let Err(e) = write_frame(&mut stream, &resp) {
        eprintln!("[NKR-IPC] write_frame: {}", e);
    }
}

fn dispatch(req: IpcRequest) -> IpcResponse {
    match req {
        IpcRequest::Health => api::handle_health(),
        IpcRequest::ListCells => api::handle_list_cells(),
        IpcRequest::RenderMetrics => {
            let body = crate::metrics::render_prometheus_metrics();
            IpcResponse::text(200, "text/plain; version=0.0.4; charset=utf-8", body)
        }
        IpcRequest::CreateInstance { cell_hint, body_json } => {
            api::handle_create(cell_hint.as_deref(), &body_json)
        }
        IpcRequest::GetInfo { nkr_name } => api::handle_get_info(&nkr_name),
        IpcRequest::DeleteInstance { nkr_name, drop_db } => {
            api::handle_delete(&nkr_name, drop_db)
        }
        IpcRequest::Action { nkr_name, action } => api::handle_action(&nkr_name, &action),
        IpcRequest::GetLogs { nkr_name, tail, from_offset, max_lines, wait_ms } => {
            api::handle_logs(&nkr_name, tail, from_offset, max_lines, wait_ms)
        }
        IpcRequest::ModulesAction { nkr_name, op, modules, admin_login, admin_password } => {
            api::handle_modules_action(&nkr_name, &op, &modules, &admin_login, &admin_password)
        }
        IpcRequest::CreateDns { nkr_name, dns, enable_websocket } => {
            api::handle_create_dns(&nkr_name, &dns, enable_websocket)
        }
        IpcRequest::DeleteDns { nkr_name, delete_cert } => {
            api::handle_delete_dns(&nkr_name, delete_cert)
        }
        IpcRequest::InitDb { nkr_name, db_name, admin_login, admin_password,
                              demo, lang, country_code, phone } => {
            api::handle_init_db(&nkr_name, db_name.as_deref(), &admin_login,
                &admin_password, demo, lang.as_deref(), country_code.as_deref(),
                phone.as_deref())
        }
        IpcRequest::PatchConfig { nkr_name, body_json } => {
            api::handle_patch_config(&nkr_name, &body_json)
        }
        IpcRequest::Psql { nkr_name, query, max_rows } => {
            api::handle_psql(&nkr_name, &query, max_rows)
        }
        IpcRequest::PurgeCache => api::handle_purge_cache(),
        IpcRequest::GetEnterpriseStatus { cell } => api::handle_enterprise_status(&cell),
    }
}

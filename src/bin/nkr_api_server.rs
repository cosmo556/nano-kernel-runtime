// =============================================================================
// nkr-api-server — Unprivileged HTTP proxy to the root nkr daemon
// =============================================================================
//
// Runs as a dedicated unprivileged user (nkr-api) and exposes the panel API
// over HTTP. Translates each HTTP request into an IpcRequest over the UDS
// exposed by the root daemon at /var/run/nkr.sock.
//
// The proxy does:
//   - HTTP framing (read method/path/body, write status/headers/body)
//   - Bearer token auth (NKR_API_TOKEN)
//   - Identifier validation (defense in depth; daemon re-validates)
//   - Body size limits (64 KiB for POST /instances, 1 KiB for actions)
//   - Bounded concurrency (MAX_INFLIGHT threads)
//
// The proxy does NOT:
//   - Touch any NKR business logic (cell creation, VM spawn, DB drop)
//   - Open /proc or /sys
//   - Require root, SYS_ADMIN, or any capabilities
//
// TLS: terminate in nginx/caddy in front of this binary. Let's Encrypt wildcard
// certs already exist for the Odoo edge — reuse the same nginx:
//
//     server {
//       listen 443 ssl http2;
//       server_name nkr.yourdomain.com;
//       ssl_certificate     /etc/letsencrypt/live/yourdomain.com/fullchain.pem;
//       ssl_certificate_key /etc/letsencrypt/live/yourdomain.com/privkey.pem;
//       location / {
//         proxy_pass http://127.0.0.1:9090;
//         proxy_http_version 1.1;
//         proxy_set_header Host $host;
//         proxy_read_timeout 120s;
//       }
//     }
// =============================================================================

#[path = "../ipc.rs"]
mod ipc;

#[path = "../api_http.rs"]
mod api_http;

use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use api_http::{
    check_auth, is_safe_addons_path, is_safe_dns, is_safe_git_ref, is_safe_git_url,
    is_safe_identifier, parse_headers, parse_request_line, query_get, read_request,
    HttpResponse, GIT_BODY_LIMIT, PYLIBS_BODY_LIMIT,
};
use ipc::{IpcRequest, IpcResponse};

const MAX_INFLIGHT: u32 = 64;
const CREATE_BODY_LIMIT: usize = 64 * 1024;
const ACTION_BODY_LIMIT: usize = 1024;

// Filesystem root managed by the proxy (addons, enterprise, pylibs live here).
const NKR_ROOT: &str = "/mnt/nkr";

// Timeouts for git / pip subprocesses (wall-clock seconds).
const GIT_TIMEOUT_S: u64 = 600;
const PIP_TIMEOUT_S: u64 = 300;

struct InflightGuard {
    counter: Arc<AtomicU32>,
}
impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

fn main() {
    let bind_host = std::env::var("NKR_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("NKR_API_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9090);
    let addr = format!("{}:{}", bind_host, port);

    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[NKR-API-SERVER] bind {} falló: {}", addr, e);
            std::process::exit(1);
        }
    };
    eprintln!(
        "[NKR-API-SERVER] Escuchando HTTP en http://{} → UDS {}",
        addr,
        ipc::socket_path().display()
    );
    if bind_host == "0.0.0.0" {
        eprintln!(
            "[NKR-API-SERVER] WARN: 0.0.0.0 expone HTTP plano a la red. \
             Poner nginx/caddy delante con TLS, o bindear a 127.0.0.1."
        );
    }

    let inflight = Arc::new(AtomicU32::new(0));
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };

        if inflight.load(Ordering::Relaxed) >= MAX_INFLIGHT {
            let body = r#"{"error":"server_busy","retry_after":"1s"}"#;
            let resp = format!(
                "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            let _ = stream.write_all(resp.as_bytes());
            continue;
        }
        inflight.fetch_add(1, Ordering::Relaxed);
        let guard_counter = Arc::clone(&inflight);

        std::thread::spawn(move || {
            // Cut slowloris: a client that opens the connection and never
            // sends bytes (or sends them byte-by-byte) would hold an
            // MAX_INFLIGHT slot indefinitely. 15s is enough for a healthy
            // panel and kills any stuck handshake.
            let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
            let _guard = InflightGuard {
                counter: guard_counter,
            };
            let read_stream = match stream.try_clone() {
                Ok(s) => s,
                Err(_) => return,
            };
            let (headers, body) = match read_request(read_stream) {
                Some(x) => x,
                None => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
            };
            let (method, path, query) = match parse_request_line(&headers) {
                Some(x) => x,
                None => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
            };
            let parsed_headers = parse_headers(&headers);

            let resp = route(method, path, query, &parsed_headers, &body);
            let _ = stream.write_all(resp.to_wire().as_bytes());
        });
    }
}

// =============================================================================
// HTTP → IPC routing
// =============================================================================

fn route(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> HttpResponse {
    // /api/v1/health is exempt from auth (used by load balancers).
    if method == "GET" && path == "/api/v1/health" {
        return ipc_to_http(ipc_call(&IpcRequest::Health));
    }
    // /metrics also exempt — Prometheus scrapers usually don't send a token.
    if method == "GET" && path == "/metrics" {
        return ipc_to_http(ipc_call(&IpcRequest::RenderMetrics));
    }

    if let Err(resp) = check_auth(headers) {
        return resp;
    }

    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method, segs.as_slice()) {
        ("GET", ["api", "v1", "cells"]) => ipc_to_http(ipc_call(&IpcRequest::ListCells)),

        ("POST", ["api", "v1", "instances"]) => handle_create_http(None, body),

        ("POST", ["api", "v1", "admin", "cache", "purge"]) => {
            ipc_to_http(ipc_call(&IpcRequest::PurgeCache))
        }

        ("GET", ["api", "v1", "cells", cell, "enterprise"]) => {
            if !is_safe_identifier(cell) {
                return HttpResponse::error(400, "invalid_cell", None);
            }
            ipc_to_http(ipc_call(&IpcRequest::GetEnterpriseStatus {
                cell: (*cell).to_string(),
            }))
        }

        ("POST", ["api", "v1", "cells", cell, "instances"]) => handle_create_http(Some(cell), body),

        ("GET", ["api", "v1", "cells", _cell, "instances", name]) => {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            ipc_to_http(ipc_call(&IpcRequest::GetInfo {
                nkr_name: (*name).to_string(),
            }))
        }

        ("DELETE", ["api", "v1", "cells", _cell, "instances", name]) => {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            let drop_db = query_get(query, "drop_db").as_deref() != Some("0");
            ipc_to_http(ipc_call(&IpcRequest::DeleteInstance {
                nkr_name: (*name).to_string(),
                drop_db,
            }))
        }

        ("POST", ["api", "v1", "cells", _cell, "instances", name, "actions"]) => {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            if body.len() > ACTION_BODY_LIMIT {
                return HttpResponse::error(413, "body_too_large", None);
            }
            // Extract the "action" field — pass string to daemon, which validates.
            let action: String = match serde_json::from_slice::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v.get("action").and_then(|x| x.as_str()).map(|s| s.to_string()))
            {
                Some(a) if ["start", "stop", "restart"].contains(&a.as_str()) => a,
                _ => {
                    return HttpResponse::error(
                        400,
                        "invalid_action",
                        Some("expected {\"action\":\"start|stop|restart\"}"),
                    )
                }
            };
            ipc_to_http(ipc_call(&IpcRequest::Action {
                nkr_name: (*name).to_string(),
                action,
            }))
        }

        ("PATCH", ["api", "v1", "cells", _cell, "instances", name, "config"]) => {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            if body.len() > 16 * 1024 {
                return HttpResponse::error(413, "body_too_large", None);
            }
            let body_str = match std::str::from_utf8(body) {
                Ok(s) => s.to_string(),
                Err(_) => return HttpResponse::error(400, "invalid_utf8", None),
            };
            ipc_to_http(ipc_call(&IpcRequest::PatchConfig {
                nkr_name: (*name).to_string(),
                body_json: body_str,
            }))
        }

        ("POST", ["api", "v1", "cells", _cell, "instances", name, "psql"]) => {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            if body.len() > 16 * 1024 {
                return HttpResponse::error(413, "body_too_large", None);
            }
            let v: serde_json::Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => return HttpResponse::error(400, "invalid_json", None),
            };
            let query = match v.get("query").and_then(|x| x.as_str()) {
                Some(q) if !q.is_empty() => q.to_string(),
                _ => return HttpResponse::error(400, "missing_query", None),
            };
            let max_rows = v.get("max_rows")
                .and_then(|x| x.as_u64())
                .unwrap_or(1000)
                .min(10_000) as usize;
            ipc_to_http(ipc_call(&IpcRequest::Psql {
                nkr_name: (*name).to_string(),
                query,
                max_rows,
            }))
        }

        ("GET", ["api", "v1", "cells", _cell, "instances", name, "logs"]) => {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            let tail: Option<usize> = query_get(query, "tail")
                .and_then(|v| v.parse().ok())
                .map(|v: usize| v.min(10_000));
            let from_offset: Option<u64> = query_get(query, "from_offset")
                .and_then(|v| v.parse().ok());
            let max_lines: Option<usize> = query_get(query, "max_lines")
                .and_then(|v| v.parse().ok())
                .map(|v: usize| v.clamp(1, 10_000));
            let wait_ms: Option<u64> = query_get(query, "wait_ms")
                .and_then(|v| v.parse().ok())
                .map(|v: u64| v.min(25_000));
            ipc_to_http(ipc_call(&IpcRequest::GetLogs {
                nkr_name: (*name).to_string(),
                tail,
                from_offset,
                max_lines,
                wait_ms,
            }))
        }

        ("GET", ["api", "v1", "cells", cell, "instances", name, "logs", "download"]) => {
            if !is_safe_identifier(cell) || !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_path_segment", None);
            }
            handle_logs_download(cell, name)
        }

        ("POST", ["api", "v1", "cells", _cell, "instances", name, "modules", op])
            if matches!(*op, "install" | "upgrade" | "uninstall") =>
        {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            if body.len() > 16 * 1024 {
                return HttpResponse::error(413, "body_too_large", None);
            }
            let v: serde_json::Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => return HttpResponse::error(400, "invalid_json", None),
            };
            let modules: Vec<String> = match v.get("modules").and_then(|x| x.as_array()) {
                Some(arr) => arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect(),
                None => return HttpResponse::error(400, "missing_modules",
                    Some("expected {\"modules\": [\"...\"], \"admin_login\":..., \"admin_password\":...}")),
            };
            if modules.is_empty() {
                return HttpResponse::error(400, "missing_modules", None);
            }
            let admin_login = match v.get("admin_login").and_then(|x| x.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return HttpResponse::error(400, "missing_admin_login", None),
            };
            let admin_password = match v.get("admin_password").and_then(|x| x.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return HttpResponse::error(400, "missing_admin_password", None),
            };
            ipc_to_http(ipc_call(&IpcRequest::ModulesAction {
                nkr_name: (*name).to_string(),
                op: (*op).to_string(),
                modules,
                admin_login,
                admin_password,
            }))
        }

        // --- Panel-ops endpoints: executed locally by the proxy (no daemon IPC) ---
        // The proxy runs unprivileged but has g:nkr-addons rwx over /mnt/nkr/cells
        // and /mnt/nkr/enterprise via setfacl. git/pip are invoked as subprocesses.

        ("POST", ["api", "v1", "cells", cell, "instances", name, "addons", "git"]) => {
            if !is_safe_identifier(cell) || !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_path_segment", None);
            }
            handle_addons_git(cell, name, body)
        }

        ("POST", ["api", "v1", "cells", cell, "enterprise", "git"]) => {
            if !is_safe_identifier(cell) {
                return HttpResponse::error(400, "invalid_cell", None);
            }
            handle_enterprise_git(cell, body)
        }

        ("PUT", ["api", "v1", "cells", cell, "instances", name, "pylibs"]) => {
            if !is_safe_identifier(cell) || !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_path_segment", None);
            }
            handle_pylibs_put(cell, name, body)
        }

        ("POST", ["api", "v1", "cells", cell, "instances", name, "dns"]) => {
            if !is_safe_identifier(cell) || !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_path_segment", None);
            }
            if body.len() > ACTION_BODY_LIMIT {
                return HttpResponse::error(413, "body_too_large", None);
            }
            let v: serde_json::Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => return HttpResponse::error(400, "invalid_json", None),
            };
            let dns = match v.get("dns").and_then(|x| x.as_str()) {
                Some(d) if api_http::is_safe_dns(d) => d.to_string(),
                _ => return HttpResponse::error(400, "invalid_dns",
                    Some("expected safe DNS name in body field 'dns'")),
            };
            let enable_ws = v.get("enable_websocket")
                .and_then(|x| x.as_bool())
                .unwrap_or(true);
            ipc_to_http(ipc_call(&IpcRequest::CreateDns {
                nkr_name: (*name).to_string(),
                dns,
                enable_websocket: enable_ws,
            }))
        }

        ("POST", ["api", "v1", "cells", cell, "instances", name, "init-db"]) => {
            if !is_safe_identifier(cell) || !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_path_segment", None);
            }
            if body.len() > CREATE_BODY_LIMIT {
                return HttpResponse::error(413, "body_too_large", None);
            }
            let v: serde_json::Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => return HttpResponse::error(400, "invalid_json", None),
            };
            let admin_login = v.get("admin_login").and_then(|x| x.as_str())
                .unwrap_or("admin").to_string();
            let admin_password = match v.get("admin_password").and_then(|x| x.as_str()) {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => return HttpResponse::error(400, "missing_admin_password",
                    Some("'admin_password' is required (≥4 chars)")),
            };
            let db_name = v.get("db_name").and_then(|x| x.as_str()).map(|s| s.to_string());
            let demo = v.get("demo").and_then(|x| x.as_bool()).unwrap_or(false);
            let lang = v.get("lang").and_then(|x| x.as_str()).map(|s| s.to_string());
            let country = v.get("country_code").and_then(|x| x.as_str()).map(|s| s.to_string());
            let phone = v.get("phone").and_then(|x| x.as_str()).map(|s| s.to_string());
            ipc_to_http(ipc_call(&IpcRequest::InitDb {
                nkr_name: (*name).to_string(),
                db_name,
                admin_login,
                admin_password,
                demo,
                lang,
                country_code: country,
                phone,
            }))
        }

        ("DELETE", ["api", "v1", "cells", cell, "instances", name, "dns"]) => {
            if !is_safe_identifier(cell) || !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_path_segment", None);
            }
            let delete_cert = query_get(query, "delete_cert").as_deref() == Some("1");
            ipc_to_http(ipc_call(&IpcRequest::DeleteDns {
                nkr_name: (*name).to_string(),
                delete_cert,
            }))
        }

        _ => HttpResponse::json(
            404,
            serde_json::json!({"error":"not_found","method":method,"path":path}),
        ),
    }
}

fn handle_create_http(cell_hint: Option<&str>, body: &[u8]) -> HttpResponse {
    if body.len() > CREATE_BODY_LIMIT {
        return HttpResponse::error(413, "body_too_large", None);
    }
    let body_str = match std::str::from_utf8(body) {
        Ok(s) => s.to_string(),
        Err(_) => return HttpResponse::error(400, "invalid_utf8", None),
    };

    // Lightweight JSON validation of the fields we care about — daemon
    // re-validates with the typed schema but we sanitize early to give good
    // error messages and prevent obvious injections from reaching the socket.
    let v: serde_json::Value = match serde_json::from_str(&body_str) {
        Ok(v) => v,
        Err(_) => return HttpResponse::error(400, "invalid_json", None),
    };

    let check_ident = |key: &str| -> Option<HttpResponse> {
        v.get(key).and_then(|x| x.as_str()).and_then(|s| {
            if !is_safe_identifier(s) {
                Some(HttpResponse::error(400, &format!("invalid_{}", key), None))
            } else {
                None
            }
        })
    };
    for k in ["nkr_name", "odoo_version", "cell", "source", "pg_version"] {
        if let Some(r) = check_ident(k) {
            return r;
        }
    }
    if let Some(d) = v.get("dns").and_then(|x| x.as_str()) {
        if !is_safe_dns(d) {
            return HttpResponse::error(400, "invalid_dns", None);
        }
    }
    if let Some(ap) = v.get("addons_path").and_then(|x| x.as_str()) {
        if !is_safe_addons_path(ap) {
            return HttpResponse::error(400, "invalid_addons_path", None);
        }
    }
    if let Some(h) = cell_hint {
        if !is_safe_identifier(h) {
            return HttpResponse::error(400, "invalid_cell_in_url", None);
        }
    }

    ipc_to_http(ipc_call(&IpcRequest::CreateInstance {
        cell_hint: cell_hint.map(|s| s.to_string()),
        body_json: body_str,
    }))
}

// =============================================================================
// IPC client wrapper
// =============================================================================

fn ipc_call(req: &IpcRequest) -> Result<IpcResponse, std::io::Error> {
    // Operaciones que pueden tardar mucho necesitan timeout mayor que el default
    // 120s del ipc::call. CreateInstance con admin_user_password incluye
    // compose-up + wait HTTP + JSON-RPC change_password (~50-150s). InitDb,
    // ModulesAction y addons/git también pueden ser largos. 600s es suficiente
    // para todos sin atar al panel a esperas absurdas.
    let timeout = match req {
        IpcRequest::CreateInstance { .. }
        | IpcRequest::InitDb { .. }
        | IpcRequest::ModulesAction { .. }
        | IpcRequest::CreateDns { .. }
        | IpcRequest::Action { .. }
        | IpcRequest::DeleteInstance { .. } => std::time::Duration::from_secs(600),
        _ => std::time::Duration::from_secs(120),
    };
    ipc::call_with_timeout(req, timeout)
}

fn ipc_to_http(res: Result<IpcResponse, std::io::Error>) -> HttpResponse {
    match res {
        Ok(r) => HttpResponse {
            status: r.status,
            body: r.body,
            content_type: r.content_type,
            extra_headers: Vec::new(),
        },
        Err(e) => {
            eprintln!("[NKR-API-SERVER] IPC error: {}", e);
            HttpResponse::error(
                502,
                "daemon_unreachable",
                Some("nkr daemon no responde en UDS"),
            )
        }
    }
}

// =============================================================================
// Panel-ops handlers (git / pylibs)
// =============================================================================
//
// These run inside the proxy, not the root daemon. The proxy speaks to the
// local filesystem via the nkr-addons supplementary group (setfacl) and to
// the network for git/pip clones. Everything the panel sends (repo URL, ref,
// subdir, deploy key, requirements.txt) is validated before touching disk or
// exec, because the proxy sits on the boundary of the tenant's trust domain.

fn handle_addons_git(cell: &str, instance: &str, body: &[u8]) -> HttpResponse {
    if body.len() > GIT_BODY_LIMIT {
        return HttpResponse::error(413, "body_too_large", None);
    }
    let req = match parse_git_body(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // Target dir: /mnt/nkr/cells/<cell>/instances/<instance>/addons/
    let instance_dir = format!("{}/cells/{}/instances/{}", NKR_ROOT, cell, instance);
    if !std::path::Path::new(&instance_dir).is_dir() {
        return HttpResponse::error(404, "instance_not_found", None);
    }
    let addons_dir = format!("{}/addons", instance_dir);
    let _ = std::fs::create_dir_all(&addons_dir);

    let subdir = req.subdir.as_deref()
        .map(|s| s.to_string())
        .unwrap_or_else(|| derive_repo_basename(&req.repo_url));
    if !is_safe_identifier(&subdir) {
        return HttpResponse::error(400, "invalid_subdir", None);
    }

    // Layout flat: todo módulo termina en addons/<module>/. Para repos
    // multi-módulo (OCA-style: repo/<module>/__manifest__.py) explotamos al
    // nivel de addons/. Para single-module (repo/__manifest__.py) lo dejamos
    // bajo el nombre del subdir solicitado por el panel.
    //
    // Estrategia: clone siempre a un tmp dir efímero y luego mover módulos a
    // su posición final. Eso evita que git deje un dir intermedio y mantiene
    // addons_path simple (`/mnt/extra-addons` cubre todo).
    //
    // Cualquier action (clone|pull|sync) se materializa como re-clone fresco
    // del tmp — no hay git pull post-explote porque el .git está suelto. La
    // idempotencia se preserva via tracker `.nkr-source` por módulo.
    let tmp_target = format!("{}/.nkr-tmp-{}", addons_dir, subdir);
    let _ = std::fs::remove_dir_all(&tmp_target);

    // Forzar clone (ignorar req.action) — el tmp siempre está vacío, así que
    // el código de run_git_sync ejecuta git clone fresh.
    let mut clone_req = req.clone();
    clone_req.action = "clone".to_string();

    let clone_resp = run_git_sync(&clone_req, &tmp_target);
    if clone_resp.status >= 400 {
        let _ = std::fs::remove_dir_all(&tmp_target);
        return clone_resp;
    }

    let sha = git_head_sha(&tmp_target).unwrap_or_else(|| "unknown".to_string());

    // Detectar layout y explotear.
    let explode_result = explode_modules(
        &tmp_target,
        &addons_dir,
        &subdir,
        &req.repo_url,
        req.reference.as_deref(),
        &sha,
    );

    // Cleanup del tmp (siempre, exitoso o no).
    let _ = std::fs::remove_dir_all(&tmp_target);

    match explode_result {
        Ok(modules) => HttpResponse::json(200, serde_json::json!({
            "repo_url": req.repo_url,
            "ref": req.reference,
            "sha": sha,
            "module_count": modules.len(),
            "modules": modules,
        })),
        Err(err_resp) => err_resp,
    }
}

/// Walks `tmp_dir` recursively looking for any directory whose entry list
/// includes `__manifest__.py`. Skips `.git/` directories and any dotfile
/// directory. Returns a vector of (absolute-path-of-module-dir, dirname)
/// pairs. Dirname is the basename of the module dir, which becomes the
/// publication name in `addons/<dirname>/`.
///
/// Stops descending once it finds `__manifest__.py` in a directory: the
/// subdirs of an Odoo module (data/, models/, wizard/, security/) are not
/// modules themselves, so descending into them would yield false positives
/// if any of them happened to contain a stray `__manifest__.py`.
fn scan_modules_recursive(tmp_dir: &str) -> Result<Vec<(std::path::PathBuf, String)>, String> {
    let mut found: Vec<(std::path::PathBuf, String)> = Vec::new();
    let root = std::path::Path::new(tmp_dir);
    walk_for_modules(root, &mut found)?;
    Ok(found)
}

fn walk_for_modules(
    dir: &std::path::Path,
    acc: &mut Vec<(std::path::PathBuf, String)>,
) -> Result<(), String> {
    // If this dir IS a module, capture and don't descend further.
    if dir.join("__manifest__.py").is_file() {
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("invalid dir name at {}", dir.display()))?
            .to_string();
        acc.push((dir.to_path_buf(), name));
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("read_dir {} failed: {}", dir.display(), e))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Skip Git internals and any dotfile dir.
        if name.starts_with('.') {
            continue;
        }
        walk_for_modules(&path, acc)?;
    }
    Ok(())
}

/// Returns (name, [paths]) for any module name that appears more than once
/// across the recursive scan results. Empty vec when no collisions.
fn detect_name_collisions(
    modules: &[(std::path::PathBuf, String)],
) -> Vec<(String, Vec<std::path::PathBuf>)> {
    use std::collections::HashMap;
    let mut by_name: HashMap<String, Vec<std::path::PathBuf>> = HashMap::new();
    for (path, name) in modules {
        by_name.entry(name.clone()).or_default().push(path.clone());
    }
    by_name
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .collect()
}

/// Reads `.gitmodules` files recursively (the parent's plus each
/// submodule's nested `.gitmodules` if present). For each declared submodule
/// `path = X`, checks the destination directory has at least one entry.
/// Returns the list of empty paths (relative to root) so the caller can
/// build a 422 response listing every submodule that failed to clone.
fn validate_submodules_populated(tmp_dir: &str) -> Result<(), Vec<String>> {
    let root = std::path::Path::new(tmp_dir);
    let mut empty_paths: Vec<String> = Vec::new();
    walk_gitmodules(root, root, &mut empty_paths);
    if empty_paths.is_empty() {
        Ok(())
    } else {
        Err(empty_paths)
    }
}

fn walk_gitmodules(
    root: &std::path::Path,
    current: &std::path::Path,
    empty: &mut Vec<String>,
) {
    let gm = current.join(".gitmodules");
    if !gm.is_file() {
        return;
    }
    let content = match std::fs::read_to_string(&gm) {
        Ok(s) => s,
        Err(_) => return,
    };
    // Minimal INI parser: extract `path = X` lines from any [submodule "..."]
    // section. We don't need to parse the section header — `path =` lines
    // only appear under submodule sections in this format.
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("path") {
            let val = rest
                .trim_start_matches(|c: char| c == ' ' || c == '\t' || c == '=')
                .trim();
            if val.is_empty() {
                continue;
            }
            let sub_path = current.join(val);
            let count = std::fs::read_dir(&sub_path)
                .map(|rd| rd.flatten().count())
                .unwrap_or(0);
            if count == 0 {
                let rel = sub_path
                    .strip_prefix(root)
                    .unwrap_or(&sub_path)
                    .to_string_lossy()
                    .into_owned();
                empty.push(rel);
            } else {
                // Recurse into the submodule's own .gitmodules, if any.
                walk_gitmodules(root, &sub_path, empty);
            }
        }
    }
}

/// Detecta si el repo recién clonado es single-module (manifest en raíz),
/// multi-módulo (manifests en subdirs) o tiene submódulos jerárquicos
/// (`.gitmodules` presente, profundidad arbitraria), y mueve los módulos a
/// su posición final bajo `addons/<module>/`.
///
/// Single-module: `tmp/__manifest__.py` → `addons/<subdir>/`.
/// Multi-módulo:  `tmp/<m>/__manifest__.py` → `addons/<m>/` para cada `m`.
/// Submódulos:    walk recursivo del árbol entero, todo dir con manifest
///                a `addons/<m>/`. Falla con 409 si dos módulos colisionan
///                en nombre, o 422 si algún submódulo declarado en
///                `.gitmodules` quedó vacío.
///
/// Tracker `.nkr-source` por módulo: archivo INI con `repo_url=`, `ref=`,
/// `sha=`. Permite re-clone idempotente sobre módulos del mismo repo y
/// detectar conflictos genuinos (módulo con mismo nombre desde otro repo).
fn explode_modules(
    tmp_dir: &str,
    addons_dir: &str,
    subdir_hint: &str,
    repo_url: &str,
    reference: Option<&str>,
    sha: &str,
) -> Result<Vec<String>, HttpResponse> {
    let tmp = std::path::Path::new(tmp_dir);
    let manifest_at_root = tmp.join("__manifest__.py").exists();
    let has_submodules = tmp.join(".gitmodules").is_file();

    // Pre-scan: recolectar (src_path, name) de cada módulo detectado.
    // Para single-module y multi-módulo legacy, src_path corresponde a la
    // posición canónica que se usará al hacer rename. Para submódulos
    // (jerarquía profunda), los src_paths vienen del walk recursivo.
    let mut modules: Vec<(std::path::PathBuf, String)> = Vec::new();

    if has_submodules {
        // Submódulos: validar que ningún submódulo del árbol haya quedado
        // vacío post-clone (auth recursiva fallida, SHA inexistente, etc.).
        // Esto se chequea ANTES del scan de manifests para que el panel
        // reciba un error específico (`submodule_clone_partial`) en vez de
        // un misleading `no_modules_found` cuando el problema fue de auth.
        if let Err(empty) = validate_submodules_populated(tmp_dir) {
            return Err(HttpResponse::json(422, serde_json::json!({
                "error": "submodule_clone_partial",
                "message": format!(
                    "{} submódulo(s) no se clonaron — verificar scope del PAT \
                     y que cada repo declarado en .gitmodules sea accesible.",
                    empty.len()
                ),
                "failed_submodules": empty,
                "remediation": "Confirmar que el PAT tiene Contents:Read sobre \
                                todos los repos del árbol y reintentar el POST.",
            })));
        }

        // Recursive scan: encontrar todo dir con __manifest__.py a profundidad
        // arbitraria. Salta dotfile dirs (.git, .github) y no desciende dentro
        // de un módulo (data/, models/, etc. no son módulos).
        match scan_modules_recursive(tmp_dir) {
            Ok(found) => modules = found,
            Err(e) => return Err(HttpResponse::error(500, "scan_failed",
                Some(&e))),
        }

        if modules.is_empty() {
            return Err(HttpResponse::error(422, "no_modules_found",
                Some("el árbol clonado (incluyendo submódulos) no contiene \
                      ningún __manifest__.py")));
        }

        // Validar que cada nombre de módulo es seguro (mismo charset que
        // is_safe_identifier para evitar path-injection vía rename).
        for (path, name) in &modules {
            if !is_safe_identifier(name) {
                return Err(HttpResponse::error(400, "invalid_module_name",
                    Some(&format!("module dir '{}' (en {}) tiene caracteres inválidos",
                        name, path.display()))));
            }
        }

        // Detectar colisión de nombres dentro del mismo deploy: dos módulos
        // con el mismo dirname encontrados en distintas ramas del árbol Git.
        // NKR no elige un ganador — aborta para que el cliente lo resuelva
        // explícitamente. Ver §4.10.2 de NKR_API.md.
        let collisions = detect_name_collisions(&modules);
        if !collisions.is_empty() {
            let conflicts_json: Vec<serde_json::Value> = collisions
                .iter()
                .map(|(name, paths)| {
                    let rels: Vec<String> = paths
                        .iter()
                        .map(|p| {
                            p.strip_prefix(tmp)
                                .unwrap_or(p)
                                .to_string_lossy()
                                .into_owned()
                        })
                        .collect();
                    serde_json::json!({
                        "module_name": name,
                        "found_at": rels,
                    })
                })
                .collect();
            return Err(HttpResponse::json(409, serde_json::json!({
                "error": "module_name_collision",
                "message": "dos o más módulos con el mismo nombre fueron \
                            encontrados en distintas ramas del árbol Git. \
                            Renombrar uno de ellos en el repo y re-mandar.",
                "conflicts": conflicts_json,
                "remediation": "Renombrar el directorio del módulo en uno \
                                de los repos en conflicto y commit + push. \
                                NKR no eligió un ganador automáticamente \
                                para evitar perder código silenciosamente.",
                "repo_url": repo_url,
                "ref": reference,
            })));
        }
    } else if manifest_at_root {
        // Single-module legacy: nombre = subdir_hint (panel decidió cómo
        // llamarlo). El src_path es el tmp dir entero, que se renombra
        // como bloque.
        modules.push((tmp.to_path_buf(), subdir_hint.to_string()));
    } else {
        // Multi-módulo legacy: cada subdir directo de tmp con __manifest__.py.
        let entries = match std::fs::read_dir(tmp) {
            Ok(e) => e,
            Err(e) => return Err(HttpResponse::error(500, "scan_failed",
                Some(&e.to_string()))),
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() { continue; }
            let name = match p.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if name.starts_with('.') { continue; } // .git, .github, etc.
            if !p.join("__manifest__.py").exists() { continue; }
            if !is_safe_identifier(&name) {
                return Err(HttpResponse::error(400, "invalid_module_name",
                    Some(&format!("module dir '{}' tiene caracteres inválidos", name))));
            }
            modules.push((p, name));
        }
        if modules.is_empty() {
            return Err(HttpResponse::error(422, "no_modules_found",
                Some("repo no tiene __manifest__.py en raíz ni en subdirs de primer nivel")));
        }
    }

    // Pre-check de conflictos vs módulos preexistentes en addons/: cada
    // módulo destino debe (a) no existir, o (b) ser de este mismo repo
    // (según `.nkr-source`). Si es de otro repo → 409 module_conflict
    // sin modificar nada. Distinto del module_name_collision (que es
    // dentro del mismo deploy).
    let mut conflicts: Vec<serde_json::Value> = Vec::new();
    for (_, m) in &modules {
        let dst = format!("{}/{}", addons_dir, m);
        let dst_path = std::path::Path::new(&dst);
        if !dst_path.exists() { continue; }
        // Existe — leer .nkr-source si está.
        let src_file = format!("{}/.nkr-source", dst);
        let prev_repo = std::fs::read_to_string(&src_file)
            .ok()
            .and_then(|s| s.lines()
                .find(|l| l.starts_with("repo_url="))
                .map(|l| l.trim_start_matches("repo_url=").to_string()));
        match prev_repo {
            Some(p) if p == repo_url => {} // overwrite legítimo (re-clone del mismo repo)
            _ => conflicts.push(serde_json::json!({
                "module": m,
                "existing_repo": prev_repo.unwrap_or_else(|| "unknown".to_string()),
                "attempted_repo": repo_url,
            })),
        }
    }
    if !conflicts.is_empty() {
        return Err(HttpResponse::json(409, serde_json::json!({
            "error": "module_conflict",
            "message": "uno o más módulos ya existen y vienen de otro repo. Borrar manualmente antes de re-clonar.",
            "conflicts": conflicts,
        })));
    }

    // Mover cada módulo a su posición final. fs::rename es atómico en el
    // mismo filesystem, por módulo. La iteración multi-módulo no es
    // atómica en agregado — si el daemon muere a la mitad del loop, queda
    // mezcla. Deuda preexistente (no introducida por submódulos), ver
    // AUDIT_GH.md §4.6 sobre renameat2 para resolverlo.
    let mut module_names: Vec<String> = Vec::with_capacity(modules.len());
    if !has_submodules && manifest_at_root {
        // Single-module legacy: rename del tmp entero como bloque.
        let m = &modules[0].1;
        let dst = format!("{}/{}", addons_dir, m);
        let _ = std::fs::remove_dir_all(&dst);
        if let Err(e) = std::fs::rename(tmp_dir, &dst) {
            return Err(HttpResponse::error(500, "move_failed",
                Some(&format!("rename {} → {}: {}", tmp_dir, dst, e))));
        }
        write_nkr_source(&dst, repo_url, reference, sha);
        module_names.push(m.clone());
    } else {
        // Multi-módulo o submódulos jerárquicos: rename por módulo desde
        // su src_path (que en el caso de submódulos puede estar a
        // profundidad arbitraria del tmp).
        for (src_path, m) in &modules {
            let dst = format!("{}/{}", addons_dir, m);
            let _ = std::fs::remove_dir_all(&dst);
            if let Err(e) = std::fs::rename(src_path, &dst) {
                return Err(HttpResponse::error(500, "move_failed",
                    Some(&format!("rename {} → {}: {}",
                        src_path.display(), dst, e))));
            }
            write_nkr_source(&dst, repo_url, reference, sha);
            module_names.push(m.clone());
        }
    }

    Ok(module_names)
}

fn write_nkr_source(module_dir: &str, repo_url: &str, reference: Option<&str>, sha: &str) {
    let content = format!(
        "repo_url={}\nref={}\nsha={}\n",
        repo_url,
        reference.unwrap_or(""),
        sha,
    );
    let _ = std::fs::write(format!("{}/.nkr-source", module_dir), content);
}

fn handle_enterprise_git(cell: &str, body: &[u8]) -> HttpResponse {
    if body.len() > GIT_BODY_LIMIT {
        return HttpResponse::error(413, "body_too_large", None);
    }
    let req = match parse_git_body(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    // Enterprise lives at /mnt/nkr/enterprise/<odoo_version>/ — version
    // is read from the cell's cell.yml rather than passed by the panel, so
    // mismatches are impossible.
    let version = match read_cell_odoo_version(cell) {
        Some(v) => v,
        None => return HttpResponse::error(404, "cell_not_found", None),
    };
    if !is_safe_identifier(&version) {
        return HttpResponse::error(500, "invalid_cell_version", None);
    }
    let ent_dir = format!("{}/enterprise/{}", NKR_ROOT, version);
    let _ = std::fs::create_dir_all(&ent_dir);
    run_git_sync(&req, &ent_dir)
}

fn handle_pylibs_put(cell: &str, instance: &str, body: &[u8]) -> HttpResponse {
    if body.len() > PYLIBS_BODY_LIMIT {
        return HttpResponse::error(413, "body_too_large", None);
    }
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return HttpResponse::error(400, "invalid_json", None),
    };
    let reqs_txt = match v.get("requirements_txt").and_then(|x| x.as_str()) {
        Some(s) if !s.is_empty() && s.len() <= 64 * 1024 => s.to_string(),
        _ => return HttpResponse::error(400, "missing_or_invalid_requirements_txt", None),
    };

    let instance_dir = format!("{}/cells/{}/instances/{}", NKR_ROOT, cell, instance);
    if !std::path::Path::new(&instance_dir).is_dir() {
        return HttpResponse::error(404, "instance_not_found", None);
    }
    let pylibs_dir = format!("{}/pylibs", instance_dir);
    let lib_dir = format!("{}/lib", pylibs_dir);
    let _ = std::fs::create_dir_all(&lib_dir);
    let reqs_path = format!("{}/requirements.txt", pylibs_dir);
    if let Err(e) = std::fs::write(&reqs_path, &reqs_txt) {
        return HttpResponse::error(500, "write_requirements_failed",
            Some(&e.to_string()));
    }

    // pip install --target=<lib> --prefer-binary --no-cache-dir --upgrade
    // --prefer-binary: use wheels when available, fall back to sdist only if
    // pure-Python (no compilation needed — we don't ship gcc in the proxy).
    let mut cmd = std::process::Command::new("pip3");
    cmd.args([
        "install",
        "-r", &reqs_path,
        "--target", &lib_dir,
        "--prefer-binary",
        "--no-cache-dir",
        "--no-compile",
        "--upgrade",
        "--disable-pip-version-check",
    ]);
    let (status, stdout, stderr) = match run_with_timeout(cmd, PIP_TIMEOUT_S) {
        Ok(t) => t,
        Err(e) => return HttpResponse::error(500, "pip_spawn_failed", Some(&e)),
    };
    let tail = tail_lines(&format!("{}\n{}", stdout, stderr), 50);
    if !status {
        return HttpResponse::json(422, serde_json::json!({
            "error": "pip_install_failed",
            "log_tail": tail,
        }));
    }
    // pip install heredó UMask=0077 del systemd unit → files 0600, dirs 0700.
    // El guest Odoo corre como uid 101 (odoo user en la imagen) y virtio-fs no
    // hace UID remapping, así que sin world-readable los imports fallan con
    // PermissionError. Fix: chmod -R go+rX al árbol instalado.
    let _ = std::process::Command::new("chmod")
        .args(["-R", "go+rX", &lib_dir])
        .status();
    HttpResponse::json(200, serde_json::json!({
        "installed": true,
        "lib_dir": lib_dir,
        "pip_log_tail": tail,
    }))
}

// ----- Helpers ---------------------------------------------------------------

#[derive(Clone)]
struct GitReq {
    repo_url: String,
    subdir: Option<String>,
    reference: Option<String>,  // git ref (branch/tag/sha); default: remote HEAD
    action: String,              // "sync" | "clone" | "pull"
    deploy_key_b64: Option<String>,
    github_token: Option<String>, // PAT HTTPS alternative to deploy_key
}

fn parse_git_body(body: &[u8]) -> Result<GitReq, HttpResponse> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| HttpResponse::error(400, "invalid_json", None))?;
    let repo_url = v.get("repo_url").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if !is_safe_git_url(&repo_url) {
        return Err(HttpResponse::error(400, "invalid_repo_url",
            Some("only git@github.com:*, https://github.com/*, gitlab.com are accepted")));
    }
    let subdir = v.get("subdir").and_then(|x| x.as_str()).map(|s| s.to_string());
    let reference = v.get("ref").and_then(|x| x.as_str()).map(|s| s.to_string());
    if let Some(ref r) = reference {
        if !is_safe_git_ref(r) {
            return Err(HttpResponse::error(400, "invalid_ref", None));
        }
    }
    let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("sync").to_string();
    if !matches!(action.as_str(), "sync" | "clone" | "pull") {
        return Err(HttpResponse::error(400, "invalid_action",
            Some("expected sync|clone|pull")));
    }
    let deploy_key_b64 = v.get("deploy_key_b64")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let github_token = v.get("github_token")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    // Validación básica del PAT: GitHub tokens son [A-Za-z0-9_]{≤100}.
    if let Some(ref t) = github_token {
        if t.is_empty() || t.len() > 256
            || !t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(HttpResponse::error(400, "invalid_github_token", None));
        }
        // PAT sólo tiene sentido con HTTPS URLs.
        if !repo_url.starts_with("https://") {
            return Err(HttpResponse::error(400, "github_token_requires_https_url",
                Some("PAT auth only works with https://github.com/... URLs; use deploy_key_b64 for SSH (git@...)")));
        }
    }
    Ok(GitReq { repo_url, subdir, reference, action, deploy_key_b64, github_token })
}

fn run_git_sync(req: &GitReq, target: &str) -> HttpResponse {
    let target_path = std::path::Path::new(target);
    let has_git = target_path.join(".git").is_dir();

    // Materialize deploy key if present. tmp_key_path stays in scope until the
    // end of this function so the file isn't dropped while git is running.
    let tmp_key_path = match &req.deploy_key_b64 {
        Some(b64) => match write_deploy_key(b64) {
            Ok(p) => Some(p),
            Err(e) => return HttpResponse::error(400, "invalid_deploy_key", Some(&e)),
        },
        None => None,
    };

    // Si hay github_token, inyectar como x-access-token en la URL HTTPS. Sólo
    // en memoria, no se persiste ni se loguea.
    let effective_url: String = match &req.github_token {
        Some(tok) if req.repo_url.starts_with("https://") => {
            let after = req.repo_url.trim_start_matches("https://");
            format!("https://x-access-token:{}@{}", tok, after)
        }
        _ => req.repo_url.clone(),
    };

    let (action_executed, clone_res, pull_res) = match (req.action.as_str(), has_git) {
        ("clone", true) => (
            "clone_skipped_exists",
            None::<(bool, String, String)>,
            None,
        ),
        ("clone", false) | ("sync", false) => {
            // --branch handles both branches and tags at clone time.
            // For commit SHAs, caller must clone first then pull with a ref.
            // The PAT is also forwarded so git_clone can install
            // url.insteadOf rewrites that cover recursive submodule clones
            // under the same owner — the parent already authenticates via
            // effective_url, but submodule URLs read from .gitmodules
            // (typically SSH or vanilla HTTPS) need the rewrite to inherit
            // the credential.
            let r = git_clone(
                &effective_url,
                target,
                req.reference.as_deref(),
                tmp_key_path.as_deref(),
                req.github_token.as_deref(),
            );
            ("clone", Some(r), None)
        }
        ("pull", true) | ("sync", true) => {
            // Para pull con PAT necesitamos re-setear el remote URL, sino
            // el pull usa el URL guardado en .git/config (que podría venir
            // de un clone hecho sin token, ej. público → privado).
            if req.github_token.is_some() {
                let _ = std::process::Command::new("git")
                    .args(["-c", &format!("safe.directory={}", target),
                           "-c", "core.hooksPath=/dev/null",
                           "-c", "protocol.allow=user",
                           "-C", target, "remote", "set-url",
                           "origin", &effective_url])
                    .output();
            }
            let r = git_pull(target, req.reference.as_deref(), tmp_key_path.as_deref());
            // Restaurar el URL SIN token después del pull para no dejar
            // credenciales en .git/config.
            if req.github_token.is_some() {
                let _ = std::process::Command::new("git")
                    .args(["-c", &format!("safe.directory={}", target),
                           "-c", "core.hooksPath=/dev/null",
                           "-c", "protocol.allow=user",
                           "-C", target, "remote", "set-url",
                           "origin", &req.repo_url])
                    .output();
            }
            ("pull", None, Some(r))
        }
        ("pull", false) => {
            return HttpResponse::error(409, "not_a_repo",
                Some("target dir has no .git — call action=clone first"));
        }
        _ => unreachable!("action validated earlier"),
    };

    // Después de clone exitoso con token: re-escribir el URL en .git/config
    // SIN el token, para no persistir credenciales.
    if req.github_token.is_some() && matches!(action_executed, "clone")
        && matches!(&clone_res, Some((true, _, _))) {
        let _ = std::process::Command::new("git")
            .args(["-c", &format!("safe.directory={}", target),
                   "-c", "core.hooksPath=/dev/null",
                   "-c", "protocol.allow=user",
                   "-C", target, "remote", "set-url",
                   "origin", &req.repo_url])
            .output();
    }

    // Ref already handled inside clone (--branch) or pull (FETCH_HEAD), so no
    // second checkout step here.
    let checkout_res: Option<(bool, String, String)> = None;

    // Best-effort scrub of the deploy key before returning.
    if let Some(ref p) = tmp_key_path {
        let _ = std::fs::remove_file(p);
    }

    // Collate errors.
    for (label, res) in [
        ("clone", &clone_res),
        ("pull", &pull_res),
        ("checkout", &checkout_res),
    ] {
        if let Some((false, out, err)) = res {
            // Scrub embedded credentials from git's stdout/stderr before
            // they flow into journald or the panel's log_tail. Patterns:
            //   https://x-access-token:<PAT>@github.com/...
            //   https://oauth2:<PAT>@gitlab.com/...
            //   https://user:password@host/...
            // The classifier still works because the substituted text
            // ("***") doesn't match any of the auth-error tokens.
            let tail_raw = tail_lines(&format!("{}\n{}", out, err), 30);
            let tail = scrub_url_credentials(&tail_raw);
            let full = format!("{}\n{}", out, err);
            let lower = full.to_lowercase();
            let safe_repo_url = scrub_url_credentials(&req.repo_url);

            // Clasificación específica: distinguir auth (panel fix → agregar
            // token/deploy_key) de otras fallas (network, bad ref, etc.).
            let (status_code, error_slug, hint): (u16, String, &'static str) =
                if lower.contains("could not read username")
                    || lower.contains("authentication failed")
                    || lower.contains("invalid username or password")
                {
                    (401, "git_auth_required".into(),
                     "repo privado HTTPS sin credenciales. Reintentar con 'github_token' en el body (PAT) o usando SSH con 'deploy_key_b64' + repo_url git@github.com:...")
                } else if lower.contains("permission denied (publickey)")
                       || lower.contains("could not read from remote repository")
                {
                    (401, "git_ssh_auth_failed".into(),
                     "La SSH deploy key no tiene acceso al repo. Verificar que esté agregada en Settings → Deploy keys del repo en GitHub/GitLab.")
                } else if lower.contains("repository not found")
                       || lower.contains("not found")
                       || lower.contains("404")
                {
                    (404, "git_repo_not_found".into(),
                     "Repo no existe o el token no tiene permiso sobre él. Verificar repo_url y scope del token.")
                } else if lower.contains("remote branch")
                       && lower.contains("not found")
                {
                    (400, "git_ref_not_found".into(),
                     "La rama/tag pasada en 'ref' no existe en el remote.")
                } else if lower.contains("timeout") || lower.contains("timed out") {
                    (504, "git_timeout".into(),
                     "Git tardó más de 180s. Posible red lenta o repo muy grande.")
                } else {
                    (422, format!("git_{}_failed", label),
                     "git devolvió error no clasificado. Revisar log_tail.")
                };

            eprintln!("[NKR-API-SERVER] {} target={} repo={} ref={:?} code={}",
                error_slug, target, safe_repo_url, req.reference, status_code);
            eprintln!("[NKR-API-SERVER] git stdout/stderr tail:\n{}", tail);
            return HttpResponse::json(status_code, serde_json::json!({
                "error": error_slug,
                "message": hint,
                "repo_url": safe_repo_url,
                "ref": req.reference,
                "target": target,
                "log_tail": tail,
            }));
        }
    }

    // Fetch SHA and path for response.
    let sha = git_head_sha(target).unwrap_or_else(|| "unknown".to_string());
    HttpResponse::json(200, serde_json::json!({
        "action": action_executed,
        "path": target,
        "sha": sha,
        "ref": req.reference,
    }))
}

fn git_clone(
    url: &str,
    target: &str,
    reference: Option<&str>,
    key_path: Option<&str>,
    github_token: Option<&str>,
) -> (bool, String, String) {
    let mut cmd = std::process::Command::new("git");

    // Holders for the lifetime of the args slice. They live on the stack of
    // this function so the &str slices below stay valid until cmd.args runs.
    let pat_ssh;
    let pat_https;

    // Disable hooks and non-user protocols — a malicious repo could bring a
    // post-checkout/post-merge hook (via templatedir, etc.) that would run as
    // `nkr-api` during the clone. These flags propagate to recursive sub-git
    // invocations via GIT_CONFIG_PARAMETERS env var.
    let mut args: Vec<&str> = vec![
        "-c", "core.hooksPath=/dev/null",
        "-c", "protocol.allow=user",
    ];

    // If a PAT was supplied, rewrite SSH and bare-HTTPS URLs to embed the
    // token. This makes private submodules (and submodules-of-submodules)
    // under the same owner clone transparently with a single credential —
    // the .gitmodules in each repo can keep declaring SSH or vanilla HTTPS
    // URLs. The url.insteadOf rules also propagate to recursive sub-gits.
    if let Some(pat) = github_token {
        pat_ssh = format!(
            "url.https://x-access-token:{}@github.com/.insteadOf=git@github.com:",
            pat
        );
        pat_https = format!(
            "url.https://x-access-token:{}@github.com/.insteadOf=https://github.com/",
            pat
        );
        args.push("-c");
        args.push(&pat_ssh);
        args.push("-c");
        args.push(&pat_https);
    }

    args.extend_from_slice(&[
        "clone",
        "--depth", "1",
        "--recurse-submodules",   // resolve submodules recursively (any depth)
        "--shallow-submodules",   // --depth=1 also for each submodule
        "--jobs", "2",            // limited parallelism (see AUDIT_GH §5.7 on rate limits)
    ]);
    if let Some(r) = reference {
        args.push("--branch");
        args.push(r);
    }
    args.push("--");
    args.push(url);
    args.push(target);
    cmd.args(&args);
    apply_git_ssh(&mut cmd, key_path);
    match run_with_timeout(cmd, GIT_TIMEOUT_S) {
        Ok(t) => t,
        Err(e) => (false, String::new(), e),
    }
}

fn git_pull(target: &str, reference: Option<&str>, key_path: Option<&str>) -> (bool, String, String) {
    // With a ref: fetch it explicitly (covers tags/branches not tracked locally),
    // then hard-reset onto FETCH_HEAD so we're always at the panel-requested
    // state regardless of the local history.
    // Without a ref: plain ff-only pull against the current branch.
    let safe_dir = format!("safe.directory={}", target);
    if let Some(r) = reference {
        let mut fetch = std::process::Command::new("git");
        fetch.args(["-c", &safe_dir,
                    "-c", "core.hooksPath=/dev/null",
                    "-c", "protocol.allow=user",
                    "-C", target, "fetch", "--depth", "1", "origin", "--", r]);
        apply_git_ssh(&mut fetch, key_path);
        match run_with_timeout(fetch, GIT_TIMEOUT_S) {
            Ok((false, o, e)) => return (false, o, e),
            Err(e) => return (false, String::new(), e),
            _ => {}
        }
        let mut reset = std::process::Command::new("git");
        reset.args(["-c", &safe_dir,
                    "-c", "core.hooksPath=/dev/null",
                    "-C", target, "reset", "--hard", "FETCH_HEAD"]);
        match run_with_timeout(reset, GIT_TIMEOUT_S) {
            Ok(t) => t,
            Err(e) => (false, String::new(), e),
        }
    } else {
        let mut cmd = std::process::Command::new("git");
        cmd.args(["-c", &safe_dir,
                  "-c", "core.hooksPath=/dev/null",
                  "-c", "protocol.allow=user",
                  "-C", target, "pull", "--ff-only"]);
        apply_git_ssh(&mut cmd, key_path);
        match run_with_timeout(cmd, GIT_TIMEOUT_S) {
            Ok(t) => t,
            Err(e) => (false, String::new(), e),
        }
    }
}

fn git_head_sha(target: &str) -> Option<String> {
    // `safe.directory=<target>` (NOT `*`) — a global wildcard would disable
    // CVE-2022-24765 process-wide, allowing RCE if an attacker can plant a
    // .git with hooks under any readable dir. Scope it to the dir the panel
    // requested, and disable hooks for safety.
    let out = std::process::Command::new("git")
        .args(["-c", &format!("safe.directory={}", target),
               "-c", "core.hooksPath=/dev/null",
               "-c", "protocol.allow=user",
               "-C", target, "rev-parse", "HEAD"])
        .output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn apply_git_ssh(cmd: &mut std::process::Command, key_path: Option<&str>) {
    if let Some(k) = key_path {
        // accept-new: trust unknown hosts on first contact. Alternative is
        // a pre-populated /etc/ssh/ssh_known_hosts with GitHub/GitLab keys.
        let ssh = format!("ssh -i {} -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/dev/null -o BatchMode=yes", k);
        cmd.env("GIT_SSH_COMMAND", ssh);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0");
}

fn write_deploy_key(b64: &str) -> Result<String, String> {
    if b64.len() > 16 * 1024 {
        return Err("deploy_key too large".into());
    }
    let decoded = b64_decode(b64).map_err(|e| format!("base64: {}", e))?;
    if !decoded.starts_with(b"-----BEGIN") {
        return Err("not a PEM private key".into());
    }
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos()).unwrap_or(0);
    let path = format!("/tmp/nkr-api-dkey-{}-{}", pid, nanos);
    // O_CREAT | O_EXCL | O_WRONLY, mode 0600
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true).create_new(true).mode(0o600)
        .open(&path).map_err(|e| e.to_string())?;
    use std::io::Write as _;
    f.write_all(&decoded).map_err(|e| e.to_string())?;
    // Deploy keys often need a trailing newline.
    if !decoded.ends_with(b"\n") {
        f.write_all(b"\n").map_err(|e| e.to_string())?;
    }
    Ok(path)
}

fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    // Minimal RFC 4648 base64 decoder (skip whitespace, no alphabet deps).
    const T: [i8; 256] = {
        let mut t = [-1i8; 256];
        let mut i = 0u8;
        while i < 26 { t[(b'A' + i) as usize] = i as i8; i += 1; }
        i = 0;
        while i < 26 { t[(b'a' + i) as usize] = (26 + i) as i8; i += 1; }
        i = 0;
        while i < 10 { t[(b'0' + i) as usize] = (52 + i) as i8; i += 1; }
        t[b'+' as usize] = 62; t[b'/' as usize] = 63;
        t[b'-' as usize] = 62; t[b'_' as usize] = 63; // url-safe
        t
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for b in s.bytes() {
        if b == b'=' || b == b'\n' || b == b'\r' || b == b' ' || b == b'\t' {
            continue;
        }
        let v = T[b as usize];
        if v < 0 { return Err(format!("invalid char {:?}", b as char)); }
        buf = (buf << 6) | (v as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

fn run_with_timeout(
    mut cmd: std::process::Command,
    timeout_s: u64,
) -> Result<(bool, String, String), String> {
    use std::io::Read;
    use std::process::Stdio;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_s);

    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => {
                let mut out = String::new();
                if let Some(mut s) = child.stdout.take() { let _ = s.read_to_string(&mut out); }
                let mut err = String::new();
                if let Some(mut s) = child.stderr.take() { let _ = s.read_to_string(&mut err); }
                return Ok((status.success(), out, err));
            }
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timeout after {}s", timeout_s));
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Replaces `<scheme>://<user>:<password>@<host>/...` segments with
/// `<scheme>://***:***@<host>/...`. Used to strip PATs / basic-auth tokens
/// embedded in URLs before they reach journald or the panel's `log_tail`.
/// Hand-rolled to avoid pulling in the `regex` crate just for this.
/// Operates byte-by-byte on the search markers (':', '@', '/', whitespace —
/// all single-byte ASCII), so multi-byte UTF-8 in surrounding text is
/// preserved untouched.
fn scrub_url_credentials(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < input.len() {
        // Look for "://" — the start of a possible URL.
        if i + 3 <= input.len() && &bytes[i..i + 3] == b"://" {
            out.push_str("://");
            let scan_start = i + 3;
            // Walk forward looking for ':' and '@' in the authority section.
            // Stop at '/', whitespace, or end-of-string.
            let mut j = scan_start;
            let mut colon_at: Option<usize> = None;
            let mut at_at: Option<usize> = None;
            while j < input.len() {
                let b = bytes[j];
                if b == b'/' || b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                    break;
                }
                if b == b':' && colon_at.is_none() {
                    colon_at = Some(j);
                }
                if b == b'@' {
                    at_at = Some(j);
                    break;
                }
                j += 1;
            }
            if let (Some(_c), Some(at)) = (colon_at, at_at) {
                out.push_str("***:***@");
                i = at + 1;
                continue;
            }
            // No credentials → copy the authority slice unmodified (it's
            // valid UTF-8 because the surrounding string is).
            out.push_str(&input[scan_start..j]);
            i = j;
            continue;
        }
        // Default: copy one full UTF-8 char.
        let ch_start = i;
        // Find the end of this UTF-8 char.
        let mut ch_end = ch_start + 1;
        while ch_end < input.len() && (bytes[ch_end] & 0xC0) == 0x80 {
            ch_end += 1;
        }
        out.push_str(&input[ch_start..ch_end]);
        i = ch_end;
    }
    out
}

fn derive_repo_basename(url: &str) -> String {
    // git@host:owner/repo.git or https://host/owner/repo.git → "repo"
    let last = url.rsplit(&['/', ':'][..]).next().unwrap_or(url);
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

fn read_cell_odoo_version(cell: &str) -> Option<String> {
    let path = format!("{}/cells/{}/cell.yml", NKR_ROOT, cell);
    let content = std::fs::read_to_string(&path).ok()?;
    // Minimal YAML scan: `odoo_version: '17.0'` or `odoo_version: 17.0`.
    for line in content.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("odoo_version:") {
            let v = rest.trim().trim_matches(&['\'', '"'][..]).to_string();
            if !v.is_empty() { return Some(v); }
        }
    }
    None
}

/// Descarga `odoo.log` del tenant como text/plain. Cap defensivo: 64 MiB en RAM.
/// Si el archivo excede el cap, devuelve los últimos 64 MiB con un header
/// `X-NKR-Log-Truncated: <original_size>`. El panel puede entonces caer a
/// `GET /logs?from_offset=N` para paginar el resto.
const LOGS_DOWNLOAD_CAP: u64 = 64 * 1024 * 1024;

fn handle_logs_download(cell: &str, instance: &str) -> HttpResponse {
    use std::io::{Read, Seek, SeekFrom};

    let log_path = format!(
        "{}/cells/{}/instances/{}/logs/odoo.log",
        NKR_ROOT, cell, instance
    );
    let mut f = match std::fs::File::open(&log_path) {
        Ok(f) => f,
        Err(_) => return HttpResponse::error(404, "log_not_found", None),
    };
    let file_size = match f.seek(SeekFrom::End(0)) {
        Ok(n) => n,
        Err(_) => return HttpResponse::error(500, "seek_failed", None),
    };
    let (start, truncated) = if file_size > LOGS_DOWNLOAD_CAP {
        (file_size - LOGS_DOWNLOAD_CAP, true)
    } else {
        (0u64, false)
    };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return HttpResponse::error(500, "seek_failed", None);
    }
    let mut buf = Vec::with_capacity((file_size - start) as usize);
    if f.take(LOGS_DOWNLOAD_CAP).read_to_end(&mut buf).is_err() {
        return HttpResponse::error(500, "read_failed", None);
    }
    let body = String::from_utf8_lossy(&buf).into_owned();

    let mut resp = HttpResponse {
        status: 200,
        body,
        content_type: "text/plain; charset=utf-8".to_string(),
        extra_headers: Vec::new(),
    };
    resp.extra_headers.push((
        "Content-Disposition".into(),
        format!("attachment; filename=\"{}-odoo.log\"", instance),
    ));
    if truncated {
        resp.extra_headers.push((
            "X-NKR-Log-Truncated".into(),
            file_size.to_string(),
        ));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_url_credentials_strips_pat() {
        let input = "remote: https://x-access-token:ghp_abc123XYZ@github.com/org/repo.git failed";
        let out = scrub_url_credentials(input);
        assert!(!out.contains("ghp_abc123XYZ"), "PAT must not survive scrub: {}", out);
        assert!(out.contains("***:***@github.com/org/repo.git"));
    }

    #[test]
    fn scrub_url_credentials_strips_basic_auth() {
        let input = "fatal: cannot fetch from https://user:secret-pw@gitlab.example.com/x/y.git";
        let out = scrub_url_credentials(input);
        assert!(!out.contains("secret-pw"));
        assert!(out.contains("***:***@gitlab.example.com"));
    }

    #[test]
    fn scrub_url_credentials_leaves_clean_urls_alone() {
        let input = "remote: https://github.com/org/repo.git pulling main";
        assert_eq!(scrub_url_credentials(input), input);
    }

    #[test]
    fn scrub_url_credentials_preserves_unicode() {
        // Multi-byte UTF-8 in surrounding text must survive.
        let input = "ramo: éxito — https://u:p@host/r café";
        let out = scrub_url_credentials(input);
        assert!(out.contains("éxito"));
        assert!(out.contains("café"));
        assert!(out.contains("***:***@host/r"));
    }

    // ---- Submodule tree handling ----------------------------------------

    use std::fs;
    use std::path::PathBuf;

    fn tempdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("nkr-gh-{}-{}", label, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_manifest(dir: &std::path::Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("__manifest__.py"), b"{}").unwrap();
    }

    #[test]
    fn scan_finds_modules_at_arbitrary_depth() {
        let tmp = tempdir("scan-deep");
        // Layout:
        //   tmp/module-direct/__manifest__.py             (depth 1)
        //   tmp/group-frontend/module-x/__manifest__.py   (depth 2)
        //   tmp/group-backend/sub/module-z/__manifest__.py(depth 3)
        write_manifest(&tmp.join("module-direct"));
        write_manifest(&tmp.join("group-frontend/module-x"));
        write_manifest(&tmp.join("group-backend/sub/module-z"));

        let found = scan_modules_recursive(&tmp.to_string_lossy()).unwrap();
        let names: std::collections::HashSet<String> =
            found.iter().map(|(_, n)| n.clone()).collect();
        assert_eq!(names.len(), 3, "found = {:?}", found);
        assert!(names.contains("module-direct"));
        assert!(names.contains("module-x"));
        assert!(names.contains("module-z"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scan_skips_dotfile_dirs() {
        let tmp = tempdir("scan-skip-dot");
        // .git/ should NEVER be descended into — even if it contained a
        // bogus __manifest__.py we must not pick it up.
        fs::create_dir_all(tmp.join(".git/modules/foo")).unwrap();
        fs::write(tmp.join(".git/modules/foo/__manifest__.py"), b"{}").unwrap();
        write_manifest(&tmp.join("good-module"));

        let found = scan_modules_recursive(&tmp.to_string_lossy()).unwrap();
        let names: Vec<String> = found.iter().map(|(_, n)| n.clone()).collect();
        assert_eq!(names, vec!["good-module".to_string()]);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scan_does_not_descend_into_a_module() {
        // A module dir contains data/, models/, security/, wizard/ subdirs.
        // Those are NOT modules — they are subdirs of one module. Scan must
        // stop at the first __manifest__.py found in the chain.
        let tmp = tempdir("scan-no-descend");
        write_manifest(&tmp.join("module-a"));
        fs::create_dir_all(tmp.join("module-a/models")).unwrap();
        fs::write(tmp.join("module-a/models/__init__.py"), b"").unwrap();
        // Stray manifest deep inside (paranoid case): ignored because the
        // walk stops at module-a's manifest.
        fs::write(tmp.join("module-a/models/__manifest__.py"), b"{}").unwrap();

        let found = scan_modules_recursive(&tmp.to_string_lossy()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "module-a");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn detect_collisions_flags_duplicates() {
        let mods: Vec<(PathBuf, String)> = vec![
            (PathBuf::from("/tmp/a/module-x"), "module-x".to_string()),
            (PathBuf::from("/tmp/b/module-x"), "module-x".to_string()),
            (PathBuf::from("/tmp/a/module-y"), "module-y".to_string()),
        ];
        let cols = detect_name_collisions(&mods);
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].0, "module-x");
        assert_eq!(cols[0].1.len(), 2);
    }

    #[test]
    fn detect_collisions_returns_empty_when_unique() {
        let mods: Vec<(PathBuf, String)> = vec![
            (PathBuf::from("/tmp/a/m1"), "m1".to_string()),
            (PathBuf::from("/tmp/b/m2"), "m2".to_string()),
        ];
        let cols = detect_name_collisions(&mods);
        assert!(cols.is_empty());
    }

    #[test]
    fn validate_submodules_detects_empty_dir() {
        let tmp = tempdir("val-empty");
        fs::write(tmp.join(".gitmodules"), r#"
[submodule "module-a"]
    path = module-a
    url = https://github.com/acme/module-a.git
[submodule "module-b"]
    path = module-b
    url = https://github.com/acme/module-b.git
"#).unwrap();
        write_manifest(&tmp.join("module-a"));
        // module-b dir created but left empty (clone of that submodule failed)
        fs::create_dir_all(tmp.join("module-b")).unwrap();

        let res = validate_submodules_populated(&tmp.to_string_lossy());
        assert!(res.is_err());
        let empty = res.unwrap_err();
        assert!(empty.iter().any(|p| p.contains("module-b")),
            "got empty list: {:?}", empty);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_submodules_recurses_into_nested_gitmodules() {
        let tmp = tempdir("val-nested");
        fs::write(tmp.join(".gitmodules"), r#"
[submodule "group-frontend"]
    path = group-frontend
    url = https://github.com/acme/group-frontend.git
"#).unwrap();
        fs::create_dir_all(tmp.join("group-frontend")).unwrap();
        fs::write(tmp.join("group-frontend/.gitmodules"), r#"
[submodule "nested-module"]
    path = nested-module
    url = https://github.com/acme/nested.git
"#).unwrap();
        // nested-module dir created but empty (auth failed for the nested repo)
        fs::create_dir_all(tmp.join("group-frontend/nested-module")).unwrap();

        let res = validate_submodules_populated(&tmp.to_string_lossy());
        assert!(res.is_err());
        let empty = res.unwrap_err();
        assert!(empty.iter().any(|p| p.contains("nested-module")),
            "got empty list: {:?}", empty);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_submodules_passes_when_all_populated() {
        let tmp = tempdir("val-ok");
        fs::write(tmp.join(".gitmodules"), r#"
[submodule "module-a"]
    path = module-a
    url = https://github.com/acme/module-a.git
"#).unwrap();
        write_manifest(&tmp.join("module-a"));

        let res = validate_submodules_populated(&tmp.to_string_lossy());
        assert!(res.is_ok());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_submodules_returns_ok_when_no_gitmodules() {
        // Plain repo without submodules — validation should be a no-op.
        let tmp = tempdir("val-no-gm");
        write_manifest(&tmp.join("module-a"));

        let res = validate_submodules_populated(&tmp.to_string_lossy());
        assert!(res.is_ok());

        let _ = fs::remove_dir_all(&tmp);
    }
}

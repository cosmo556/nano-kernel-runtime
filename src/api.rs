// =============================================================================
// NKR API — endpoints HTTP que consume el panel de control externo
// =============================================================================
//
// Montado sobre el listener HTTP de `src/metrics.rs::start_prometheus_server`.
// Sin frameworks externos: router manual basado en (method, path) y parseo de
// body JSON con serde_json.
//
// Rutas:
//   POST   /api/v1/cells/{cell}/instances                        → crear (clone)
//   GET    /api/v1/cells/{cell}/instances/{nkr_name}             → info + nkr_status
//   DELETE /api/v1/cells/{cell}/instances/{nkr_name}?drop_db=1   → eliminar
//   POST   /api/v1/cells/{cell}/instances/{nkr_name}/actions     → start|stop|restart
//   GET    /api/v1/cells/{cell}/instances/{nkr_name}/logs?tail=N → tail odoo.log
//   GET    /api/v1/health                                        → { ok: true, version }
//
// Todas las mutaciones (POST/DELETE) requieren header
//   Authorization: Bearer <NKR_API_TOKEN>
// si la variable de entorno NKR_API_TOKEN está seteada. En dev/local podés
// omitirla y cualquier request pasa. En producción el operador la exporta
// antes de correr `nkr serve`.
// =============================================================================

use serde::Deserialize;
use std::io::Read;
use std::process::Command;

use crate::cell::{
    CloneOptions, Edition, InstanceMode,
    clone_instance_with_opts, delete_instance, get_instance_info,
    count_odoo_instances, default_source_in_cell, ensure_cell_prefix,
    lookup_cell_id, load_cell, select_cell_for_version, MAX_ODOOS_PER_CELL,
};

// =============================================================================
// Tipos de request
// =============================================================================

#[derive(Deserialize)]
pub struct CreateInstanceReq {
    /// Nombre de la instancia. Puede ser corto ("tst-1") o completo
    /// ("nazcatex-tst-1"); se auto-prefija con el cell name si falta.
    pub nkr_name: String,
    /// `dev` (clon con DB) o `production` (clon sin DB).
    pub mode: InstanceMode,
    /// **Requerido**: cada cell soporta una única versión de Odoo, el panel
    /// la envía y el backend valida que la cell coincida.
    pub odoo_version: String,
    /// Opcional: si se especifica, se valida versión y capacidad. Si se omite,
    /// el backend elige la cell menos llena con la versión coincidente.
    #[serde(default)]
    pub cell: Option<String>,
    /// Opcional: nkr_name del template. Si se omite, se usa la primera
    /// instancia alfabética de la cell seleccionada.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub dns: Option<String>,
    #[serde(default)]
    pub edition: Option<Edition>,
    #[serde(default)]
    pub pg_version: Option<String>,
    #[serde(default)]
    pub workers: Option<u32>,
    #[serde(default)]
    pub list_db: Option<bool>,
    /// Bytes. Mapea a `limit_memory_soft` en odoo.conf.
    #[serde(default)]
    pub limit_memory_soft: Option<u64>,
    /// Bytes. Mapea a `limit_memory_hard` en odoo.conf.
    #[serde(default)]
    pub limit_memory_hard: Option<u64>,
    #[serde(default)]
    pub addons_path: Option<String>,
    #[serde(default)]
    pub python_libs: Vec<String>,
}

#[derive(Deserialize)]
pub struct ActionReq {
    pub action: ActionKind,
}

#[derive(Deserialize, serde::Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind { Start, Stop, Restart }

// =============================================================================
// Request parsing — lee el body según Content-Length
// =============================================================================

#[allow(dead_code)]
pub struct HttpRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    pub content_type: &'static str,
}

impl HttpResponse {
    pub fn json(status: u16, body: impl serde::Serialize) -> Self {
        let body = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
        HttpResponse { status, body, content_type: "application/json" }
    }
    #[allow(dead_code)]
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        HttpResponse { status, body: body.into(), content_type: "text/plain; charset=utf-8" }
    }
    pub fn to_wire(&self) -> String {
        let reason = match self.status {
            200 => "OK", 201 => "Created", 202 => "Accepted", 204 => "No Content",
            400 => "Bad Request", 401 => "Unauthorized", 403 => "Forbidden",
            404 => "Not Found", 405 => "Method Not Allowed", 409 => "Conflict",
            500 => "Internal Server Error", _ => "OK",
        };
        format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.status, reason, self.content_type, self.body.len(), self.body
        )
    }
}

/// Lee request HTTP desde un stream. Incluye body según Content-Length.
pub fn read_request(mut stream: impl Read) -> Option<(String, Vec<u8>)> {
    // Lee hasta "\r\n\r\n" para headers, luego Content-Length bytes adicionales.
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 1024];
    let mut header_end = None;
    while header_end.is_none() {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 { return None; }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(idx) = find_double_crlf(&buf) {
            header_end = Some(idx);
            break;
        }
        if buf.len() > 64 * 1024 { return None; } // defensa contra request gigante
    }
    let hidx = header_end.unwrap();
    let headers = String::from_utf8_lossy(&buf[..hidx]).to_string();

    // Parsear Content-Length del header
    let cl = headers.lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let body_start = hidx + 4;
    let mut body: Vec<u8> = buf[body_start..].to_vec();
    while body.len() < cl {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 { break; }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(cl);
    Some((headers, body))
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

pub fn parse_request_line(headers: &str) -> Option<(&str, &str, &str)> {
    let first = headers.lines().next()?;
    let mut it = first.split_whitespace();
    let method = it.next()?;
    let full = it.next()?;
    let (path, query) = match full.split_once('?') {
        Some((p, q)) => (p, q),
        None => (full, ""),
    };
    Some((method, path, query))
}

pub fn parse_headers(headers: &str) -> Vec<(String, String)> {
    headers.lines().skip(1).filter_map(|l| {
        l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
    }).collect()
}

// =============================================================================
// Autenticación bearer token opcional
// =============================================================================

fn check_auth(headers: &[(String, String)]) -> Result<(), HttpResponse> {
    let expected = match std::env::var("NKR_API_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => return Ok(()), // sin token configurado → pasa (modo dev/local)
    };
    let got = headers.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Authorization"))
        .map(|(_, v)| v.trim_start_matches("Bearer ").trim().to_string())
        .unwrap_or_default();
    if got != expected {
        return Err(HttpResponse::json(401, serde_json::json!({
            "error": "unauthorized",
            "message": "Authorization: Bearer <token> requerido"
        })));
    }
    Ok(())
}

// =============================================================================
// Query string parser mínimo (k=v&k2=v2)
// =============================================================================

fn query_get(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key { return Some(v.to_string()); }
        }
    }
    None
}

// =============================================================================
// Dispatcher principal
// =============================================================================

pub fn dispatch(method: &str, path: &str, query: &str, headers: &[(String, String)], body: &[u8]) -> HttpResponse {
    // Health no requiere auth
    if method == "GET" && path == "/api/v1/health" {
        return HttpResponse::json(200, serde_json::json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
        }));
    }

    if let Err(resp) = check_auth(headers) {
        return resp;
    }

    // Rutas con parámetros: parseo manual por segmentos.
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    // Esperamos ["api", "v1", "cells", <cell>, "instances", ...]
    match (method, segs.as_slice()) {
        // GET /api/v1/cells — lista cells con capacity
        ("GET", ["api", "v1", "cells"]) => handle_list_cells(),

        // POST /api/v1/instances — auto-selección de cell por odoo_version
        ("POST", ["api", "v1", "instances"]) => handle_create(None, body),

        // POST /api/v1/cells/{cell}/instances — cell explícita
        ("POST", ["api", "v1", "cells", cell, "instances"]) => handle_create(Some(cell), body),

        // GET /api/v1/cells/{cell}/instances/{name}
        ("GET", ["api", "v1", "cells", _cell, "instances", name]) => {
            handle_get_info(name)
        }
        // DELETE /api/v1/cells/{cell}/instances/{name}
        ("DELETE", ["api", "v1", "cells", _cell, "instances", name]) => {
            handle_delete(name, query)
        }
        // POST /api/v1/cells/{cell}/instances/{name}/actions
        ("POST", ["api", "v1", "cells", _cell, "instances", name, "actions"]) => {
            handle_action(name, body)
        }
        // GET /api/v1/cells/{cell}/instances/{name}/logs?tail=N
        ("GET", ["api", "v1", "cells", _cell, "instances", name, "logs"]) => {
            handle_logs(name, query)
        }
        _ => HttpResponse::json(404, serde_json::json!({
            "error": "not_found",
            "method": method,
            "path": path,
        })),
    }
}

fn handle_list_cells() -> HttpResponse {
    let cells: Vec<serde_json::Value> = crate::cell::list_cells().into_iter().map(|c| {
        let used = crate::cell::count_odoo_instances(&c.name);
        let free = MAX_ODOOS_PER_CELL.saturating_sub(used);
        serde_json::json!({
            "name": c.name,
            "cell_id": c.cell_id,
            "odoo_version": c.odoo_version,
            "used_odoos": used,
            "max_odoos": MAX_ODOOS_PER_CELL,
            "free_slots": free,
        })
    }).collect();
    HttpResponse::json(200, serde_json::json!({
        "cells": cells,
        "max_odoos_per_cell": MAX_ODOOS_PER_CELL,
    }))
}

// =============================================================================
// Handlers
// =============================================================================

/// `cell_hint`: None cuando el request no trae cell en URL (ruta /instances auto).
///              Some(cell) cuando el panel forzó una cell específica en el URL.
fn handle_create(cell_hint: Option<&str>, body: &[u8]) -> HttpResponse {
    let req: CreateInstanceReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return HttpResponse::json(400, serde_json::json!({
            "error": "invalid_json",
            "message": e.to_string(),
        })),
    };

    // Reconciliar cell: body.cell > URL cell > auto por versión
    let cell_name_req: Option<String> = req.cell.clone().or_else(|| cell_hint.map(|s| s.to_string()));

    let resolved_cell = match cell_name_req {
        Some(name) => {
            // Cell explícita: validar existencia + versión coincidente.
            if lookup_cell_id(&name).is_none() {
                return HttpResponse::json(404, serde_json::json!({
                    "error": "cell_not_found",
                    "cell": name,
                }));
            }
            let cell = match load_cell(&name) {
                Ok(c) => c,
                Err(e) => return HttpResponse::json(404, serde_json::json!({
                    "error": "cell_load_failed", "message": e.to_string(),
                })),
            };
            match cell.odoo_version.as_deref() {
                Some(v) if v == req.odoo_version => {}
                Some(v) => return HttpResponse::json(409, serde_json::json!({
                    "error": "version_mismatch",
                    "message": format!("Cell '{}' está en odoo_version={}, panel pidió {}",
                        cell.name, v, req.odoo_version),
                    "cell": cell.name, "cell_version": v, "requested_version": req.odoo_version,
                })),
                None => return HttpResponse::json(409, serde_json::json!({
                    "error": "cell_version_unset",
                    "message": format!("Cell '{}' sin odoo_version en cell.yml", cell.name),
                    "cell": cell.name,
                })),
            }
            cell
        }
        None => {
            // Auto-selección: menor carga primero.
            match select_cell_for_version(&req.odoo_version) {
                Ok(c) => c,
                Err(e) => return HttpResponse::json(409, serde_json::json!({
                    "error": "no_cell_available",
                    "message": e.to_string(),
                    "requested_version": req.odoo_version,
                })),
            }
        }
    };

    // Validar capacidad explícita antes de tocar disco (clone_instance_with_opts
    // re-valida internamente, pero un 409 temprano con mensaje claro le sirve al panel).
    let used = count_odoo_instances(&resolved_cell.name);
    if used >= MAX_ODOOS_PER_CELL {
        return HttpResponse::json(409, serde_json::json!({
            "error": "cell_full",
            "cell": resolved_cell.name,
            "used": used, "max": MAX_ODOOS_PER_CELL,
        }));
    }

    // Resolver source: body.source o primer instance de la cell como template.
    let source = match req.source.clone() {
        Some(s) => s,
        None => match default_source_in_cell(&resolved_cell.name) {
            Ok(s) => s,
            Err(e) => return HttpResponse::json(409, serde_json::json!({
                "error": "no_template_source",
                "message": e.to_string(),
                "cell": resolved_cell.name,
            })),
        },
    };

    // Normalizar nkr_name (aceptar corto o completo).
    let dst_name = ensure_cell_prefix(&resolved_cell.name, &req.nkr_name);

    let opts = CloneOptions {
        mode: req.mode,
        no_compose: false,
        dns: req.dns,
        edition: req.edition,
        odoo_version: Some(req.odoo_version),
        pg_version: req.pg_version,
        workers: req.workers,
        list_db: req.list_db,
        limit_memory_soft: req.limit_memory_soft,
        limit_memory_hard: req.limit_memory_hard,
        addons_path: req.addons_path,
        python_libs: req.python_libs,
    };

    match clone_instance_with_opts(&source, &dst_name, &opts) {
        Ok(info) => HttpResponse::json(201, info),
        Err(e) => HttpResponse::json(500, serde_json::json!({
            "error": "clone_failed",
            "message": e.to_string(),
            "cell": resolved_cell.name,
            "source": source,
            "nkr_name": dst_name,
        })),
    }
}

fn handle_get_info(nkr_name: &str) -> HttpResponse {
    match get_instance_info(nkr_name) {
        Ok(info) => HttpResponse::json(200, info),
        Err(e) => HttpResponse::json(404, serde_json::json!({
            "error": "not_found",
            "message": e.to_string(),
        })),
    }
}

fn handle_delete(nkr_name: &str, query: &str) -> HttpResponse {
    let drop_db = query_get(query, "drop_db").as_deref() != Some("0");
    match delete_instance(nkr_name, drop_db) {
        Ok(cell) => HttpResponse::json(200, serde_json::json!({
            "deleted": true,
            "nkr_name": nkr_name,
            "cell": cell,
            "drop_db": drop_db,
        })),
        Err(e) => HttpResponse::json(500, serde_json::json!({
            "error": "delete_failed",
            "message": e.to_string(),
        })),
    }
}

fn handle_action(nkr_name: &str, body: &[u8]) -> HttpResponse {
    let req: ActionReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return HttpResponse::json(400, serde_json::json!({
            "error": "invalid_json",
            "message": e.to_string(),
        })),
    };

    // Obtenemos cell name via get_instance_info para poder invocar `nkr compose up/down`
    // en el directorio correcto. Si no encontramos info, seguimos con nkr run/stop directo.
    let info = get_instance_info(nkr_name);
    let cell_dir = info.as_ref().ok().map(|i| {
        std::path::PathBuf::from(&i.instance_dir)
            .parent().and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
    }).flatten();

    let result = match req.action {
        ActionKind::Start => start_instance(nkr_name, cell_dir.as_deref()),
        ActionKind::Stop => stop_instance(nkr_name),
        ActionKind::Restart => restart_instance(nkr_name, cell_dir.as_deref()),
    };

    match result {
        Ok(()) => {
            // Re-leer info después de la acción para devolver estado actualizado
            let info = get_instance_info(nkr_name).ok();
            HttpResponse::json(202, serde_json::json!({
                "action": serde_json::to_value(req.action).unwrap_or_default(),
                "nkr_name": nkr_name,
                "status": "accepted",
                "info": info,
            }))
        }
        Err(e) => HttpResponse::json(500, serde_json::json!({
            "error": "action_failed",
            "message": e.to_string(),
        })),
    }
}

fn start_instance(nkr_name: &str, cell_dir: Option<&std::path::Path>) -> Result<(), Box<dyn std::error::Error>> {
    // Si ya corre, no-op.
    if crate::state::list_vms().iter().any(|v| v.name == nkr_name) {
        return Ok(());
    }
    let dir = cell_dir.ok_or("no se pudo resolver cell dir para start")?;
    let status = Command::new("nkr")
        .current_dir(dir)
        .args(["compose", "up", "-d"])
        .status()?;
    if !status.success() { return Err("nkr compose up falló".into()); }
    Ok(())
}

fn stop_instance(nkr_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let vm = crate::state::list_vms().into_iter().find(|v| v.name == nkr_name);
    match vm {
        Some(v) => crate::state::stop_vm(v.vm_id),
        None => Ok(()), // ya detenida
    }
}

fn restart_instance(nkr_name: &str, cell_dir: Option<&std::path::Path>) -> Result<(), Box<dyn std::error::Error>> {
    stop_instance(nkr_name)?;
    // Esperar un momento para que el TAP se libere (el netlock cubre la race interna)
    std::thread::sleep(std::time::Duration::from_millis(200));
    start_instance(nkr_name, cell_dir)
}

fn handle_logs(nkr_name: &str, query: &str) -> HttpResponse {
    let info = match get_instance_info(nkr_name) {
        Ok(i) => i,
        Err(e) => return HttpResponse::json(404, serde_json::json!({
            "error": "not_found",
            "message": e.to_string(),
        })),
    };
    let tail: usize = query_get(query, "tail")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
        .min(10_000);

    let content = std::fs::read_to_string(&info.logs_path).unwrap_or_default();
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(tail);
    let tail_lines: Vec<&str> = lines[start..].to_vec();
    HttpResponse::json(200, serde_json::json!({
        "nkr_name": nkr_name,
        "logs_path": info.logs_path,
        "tail": tail,
        "lines": tail_lines,
    }))
}

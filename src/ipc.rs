#![allow(dead_code)] // Module is consumed by both bins; each sees a partial surface.

// =============================================================================
// NKR IPC — Privilege-separation protocol between nkr (root daemon) and
// nkr-api-server (unprivileged HTTP proxy)
// =============================================================================
//
// Wire format (over Unix Domain Socket at /var/run/nkr.sock):
//
//   ┌──────────────────┬──────────────────────────────┐
//   │ u32 LE length    │ JSON body (len bytes)        │
//   └──────────────────┴──────────────────────────────┘
//
// Both requests and responses use this framing. Frame cap: 8 MiB (log tails can
// return up to ~4 MiB in the current implementation).
//
// The unprivileged side connects, writes one IpcRequest frame, reads exactly
// one IpcResponse frame, closes. Short-lived connections — no multiplexing.
//
// Socket permissions are set by the daemon to 0660 root:nkr-api so that only
// the nkr-api group can reach it (enforced by Unix file perms; no
// cryptographic auth on the UDS itself).
//
// The IPC module is dep-free (no cell/vmm/state imports) so both binaries can
// link it cheaply. The CreateInstance body travels as a raw JSON string —
// the proxy validates a few key fields for basic sanity, the daemon parses it
// with its full typed schema.
// =============================================================================

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default UDS path. Override with env NKR_SOCKET_PATH.
pub const DEFAULT_SOCKET_PATH: &str = "/var/run/nkr.sock";

/// Max frame size (request or response). Reject anything bigger to prevent OOM.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

pub fn socket_path() -> PathBuf {
    PathBuf::from(
        std::env::var("NKR_SOCKET_PATH").unwrap_or_else(|_| DEFAULT_SOCKET_PATH.to_string()),
    )
}

// =============================================================================
// Request / Response types
// =============================================================================

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IpcRequest {
    Health,
    ListCells,
    RenderMetrics,
    /// Per-VM metrics snapshot as JSON (the panel's per-instance Metrics tab).
    /// Cached server-side ~30s; the disk `du` cached longer. 404 if unknown.
    MetricsForVm { nkr_name: String },
    /// Create instance. `body_json` is the full JSON body received from the
    /// panel — daemon parses it with its typed CreateInstanceReq schema.
    CreateInstance {
        cell_hint: Option<String>,
        body_json: String,
    },
    GetInfo {
        nkr_name: String,
    },
    DeleteInstance {
        nkr_name: String,
        drop_db: bool,
    },
    /// `action`: "start" | "stop" | "restart". Daemon rejects unknown values.
    Action {
        nkr_name: String,
        action: String,
    },
    GetLogs {
        nkr_name: String,
        /// Modo tail clásico: últimas N líneas del archivo. Si `from_offset`
        /// está presente, se ignora y se usa ese en su lugar.
        #[serde(default)]
        tail: Option<usize>,
        /// Modo cursor: leer desde este byte offset. El panel lo obtuvo en un
        /// response previo (`next_offset`). Resume-safe.
        #[serde(default)]
        from_offset: Option<u64>,
        /// Cap de líneas devueltas en modo cursor. Default 500, max 10000.
        #[serde(default)]
        max_lines: Option<usize>,
        /// Long-poll: si `from_offset == file_size`, bloquea hasta que el
        /// archivo crezca o hasta `wait_ms`. Max 25000 ms. Default 0 = no block.
        #[serde(default)]
        wait_ms: Option<u64>,
    },
    /// Ejecuta acción sobre módulos Odoo del tenant vía JSON-RPC. `op` in
    /// {"install", "upgrade", "uninstall"}. Bloquea hasta que Odoo responde
    /// (instalaciones grandes pueden tardar varios minutos).
    ModulesAction {
        nkr_name: String,
        op: String,
        modules: Vec<String>,
        admin_login: String,
        admin_password: String,
    },
    /// Provisiona (o re-provisiona) DNS para un tenant: emite cert Let's Encrypt,
    /// escribe vhost nginx y recarga. Idempotente — si ya hay cert y vhost, actualiza.
    CreateDns {
        nkr_name: String,
        dns: String,
        enable_websocket: bool,
    },
    /// Quita vhost nginx y opcionalmente el cert. Idempotente.
    DeleteDns {
        nkr_name: String,
        delete_cert: bool,
    },
    /// Crea la DB inicial del tenant vía Odoo /web/database/create (usa
    /// admin_passwd del odoo.conf del tenant). Bloquea hasta que Odoo
    /// responde o timeout.
    InitDb {
        nkr_name: String,
        db_name: Option<String>,
        admin_login: String,
        admin_password: String,
        demo: bool,
        lang: Option<String>,
        country_code: Option<String>,
        phone: Option<String>,
    },
    /// Upsert de keys de `odoo.conf`. Body JSON pre-validado por el proxy.
    /// Workers/memory requieren restart; SMTP aplica con SIGHUP o restart.
    PatchConfig {
        nkr_name: String,
        body_json: String,
    },
    /// Ejecuta un comando psql contra la DB del tenant. Sandbox: `-d db-<tenant>`
    /// fijo, timeout 30s, output CSV truncado. Audit-log obligatorio.
    Psql {
        nkr_name: String,
        query: String,
        max_rows: usize,
    },
    /// Borra todas las entries del cache nginx (`/var/cache/nginx/nkr_static/*`).
    /// Operación global — afecta a todos los tenants. Reconstrucción es orgánica
    /// (próxima request va a Odoo). Útil tras `POST /addons/git` con cambios en
    /// archivos `/web/static/` (logos, imgs, fonts), donde la URL es fija y la
    /// entry vieja serviría stale data por hasta 24h.
    PurgeCache,
    /// Reload de workers Odoo SIN reiniciar la VM. Daemon manda SIGUSR1 al
    /// proceso de la VM, vmm.rs inyecta "REL_OD\n" por hvc0, init guest hace
    /// pkill -HUP odoo → master mata workers → respawnean con código fresh
    /// del disco. ~3s, sin downtime de master ni VM. Usado tras addons/git
    /// (auto) y vía POST /reload (manual). Idempotente — múltiples reloads
    /// rápidos colapsan en uno solo.
    ReloadWorkers {
        nkr_name: String,
    },
    /// Marca la VM como ACTIVE en el ballooning dinámico (CLAUDE.md v2.2).
    /// Daemon manda SIGUSR2 al proceso de la VM; vmm.rs renueva el TS y
    /// el vcpu loop aplica el target ACTIVE en ≤5s. Si la VM tiene balloon
    /// estático (PROD por tier), la señal es no-op — la respuesta sigue
    /// siendo 202 para idempotencia desde el panel.
    BalloonActive {
        nkr_name: String,
    },
    /// Diagnóstico HOST-side: captura stacks/wchan/cpu de los threads del
    /// proceso `nkr` del tenant. Devuelve text/plain con dump multi-sección.
    /// Útil cuando el watchdog detecta cuelgue y queremos snapshot pre-restart.
    /// Idempotente, ~50ms. Ver `api::handle_diag` para el output.
    Diag {
        nkr_name: String,
    },
    /// SSO one-shot: NKR pre-autentica con el admin_passwd del tenant y
    /// devuelve una URL firmada (HMAC) para auto-login. Ver `handle_sso`
    /// para el flujo completo. El password jamás sale del host.
    Sso {
        nkr_name: String,
        user: String,
    },
    /// Estado del repo Odoo Enterprise descargado en una cell. El panel lo
    /// usa para decidir si puede aceptar `edition: "enterprise"` al crear
    /// tenants — si la cell no tiene el repo descargado, el tenant arrancaría
    /// con manifests faltantes y warnings en log.
    GetEnterpriseStatus {
        cell: String,
    },
    /// Estado de un create asíncrono lanzado por `POST /instances` (v1.6.4+).
    /// El panel pollea esto hasta `status` ∈ {`ready`, `failed`}. Lee el status
    /// file en `/mnt/nkr/cells/{cell}/.nkr-creates/{nkr_name}.json`.
    GetCreateStatus {
        cell: String,
        nkr_name: String,
    },
}

/// HTTP-shaped response. `body` is always valid UTF-8 (JSON or Prometheus text).
#[derive(Serialize, Deserialize, Debug)]
pub struct IpcResponse {
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

impl IpcResponse {
    pub fn json(status: u16, body: impl Serialize) -> Self {
        let body = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
        IpcResponse {
            status,
            content_type: "application/json".to_string(),
            body,
        }
    }
    pub fn text(status: u16, content_type: &str, body: impl Into<String>) -> Self {
        IpcResponse {
            status,
            content_type: content_type.to_string(),
            body: body.into(),
        }
    }
    pub fn error(status: u16, err: &str, msg: Option<&str>) -> Self {
        let v = match msg {
            Some(m) => serde_json::json!({"error": err, "message": m}),
            None => serde_json::json!({"error": err}),
        };
        Self::json(status, v)
    }
}

// =============================================================================
// Framing
// =============================================================================

/// Write a length-prefixed JSON frame to the stream.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> std::io::Result<()> {
    let body = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame {} > MAX_FRAME_BYTES {}", body.len(), MAX_FRAME_BYTES),
        ));
    }
    let len = (body.len() as u32).to_le_bytes();
    w.write_all(&len)?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// Read a length-prefixed JSON frame from the stream.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> std::io::Result<T> {
    let mut lenbuf = [0u8; 4];
    r.read_exact(&mut lenbuf)?;
    let len = u32::from_le_bytes(lenbuf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame {} > MAX_FRAME_BYTES {}", len, MAX_FRAME_BYTES),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

// =============================================================================
// Client helper (used by nkr-api-server)
// =============================================================================

/// Open a UDS connection, send one request, read one response, close.
/// Short-lived — no multiplexing.
pub fn call(req: &IpcRequest) -> std::io::Result<IpcResponse> {
    call_with_timeout(req, Duration::from_secs(120))
}

pub fn call_with_timeout(req: &IpcRequest, timeout: Duration) -> std::io::Result<IpcResponse> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write_frame(&mut stream, req)?;
    let resp: IpcResponse = read_frame(&mut stream)?;
    Ok(resp)
}

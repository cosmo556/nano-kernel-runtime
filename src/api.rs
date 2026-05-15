// =============================================================================
// NKR API — Daemon-side request handlers (invoked over IPC from nkr-api-server)
// =============================================================================
//
// Every `handle_*` function takes already-validated inputs (strings or parsed
// bodies), performs the actual work against cell/state/metrics modules, and
// returns an `IpcResponse`. The unprivileged proxy marshals HTTP ↔ IPC; all
// the privileged work (file I/O, cgroup writes, process spawns) happens here.
//
// Validation is repeated here on the daemon side (defense in depth): even
// though the proxy checks identifiers before sending, a rogue process with
// access to the UDS could skip the proxy. Every handler re-validates.
// =============================================================================
//
// Routes exposed by the proxy (resolved to IpcRequest variants):
//   POST   /api/v1/instances                                     → CreateInstance { cell_hint: None }
//   POST   /api/v1/cells/{cell}/instances                        → CreateInstance { cell_hint: Some(cell) }
//   GET    /api/v1/cells/{cell}/instances/{nkr_name}             → GetInfo
//   DELETE /api/v1/cells/{cell}/instances/{nkr_name}?drop_db=1   → DeleteInstance
//   POST   /api/v1/cells/{cell}/instances/{nkr_name}/actions     → Action
//   GET    /api/v1/cells/{cell}/instances/{nkr_name}/logs?tail=N → GetLogs
//   GET    /api/v1/health                                        → Health
//   GET    /api/v1/cells                                         → ListCells
//   GET    /metrics                                              → RenderMetrics
// =============================================================================

use serde::{Deserialize, Serialize};
use std::process::Command;

use crate::api_http::{is_safe_addons_path, is_safe_dns, is_safe_identifier};
use crate::cell::{
    clone_instance_with_opts, count_odoo_instances, delete_instance,
    ensure_cell_prefix, get_instance_info, load_cell, lookup_cell_id,
    patch_odoo_conf, select_cell_for_version, CloneOptions, Edition, InstanceMode, Tier,
    MAX_ODOOS_PER_CELL,
};
use crate::ipc::IpcResponse;

// =============================================================================
// Request body types (panel → proxy → daemon over IPC body_json)
// =============================================================================

#[derive(Serialize, Deserialize, Debug)]
pub struct CreateInstanceReq {
    /// Instance name. Can be short ("tst-1") or full ("company_client-tst-1");
    /// it auto-prefixes with the cell name if missing.
    pub nkr_name: String,
    /// `dev` (clone with DB) o `production` (clone without DB). **Opcional.**
    /// Default `production`. **Sólo aplica cuando `tier=production`** (legacy).
    /// Cuando se manda `tier=staging` o `tier=dev`, este campo es ignorado
    /// (el tier dicta la semántica internamente). Tip: el panel puede mandar
    /// solo `tier` y olvidarse de `mode`.
    #[serde(default)]
    pub mode: InstanceMode,
    /// Required: each cell supports a single Odoo version; the panel
    /// sends it and the backend validates the cell matches.
    pub odoo_version: String,
    /// Optional: if specified, version and capacity are validated. If omitted,
    /// the backend picks the least-full cell with the matching version.
    #[serde(default)]
    pub cell: Option<String>,
    /// Optional: template nkr_name. If omitted, the first alphabetical
    /// instance of the selected cell is used.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub dns: Option<String>,
    #[serde(default)]
    pub edition: Option<Edition>,
    /// **Atajo del panel (API v2, 2026-05-15+).** Si se manda, mapea a `edition`:
    /// `true` → Enterprise, `false` → Community. Tiene precedencia sobre
    /// `edition` si ambos vienen — el contrato nuevo es "el panel manda
    /// `enterprise:bool`" y se acabó. Si se omite, se respeta `edition` (o
    /// default Community).
    #[serde(default)]
    pub enterprise: Option<bool>,
    #[serde(default)]
    pub pg_version: Option<String>,
    /// Sole resource input: NKR deriva chrs, ram_mb (compose) y limit_memory_soft/hard
    /// (odoo.conf) a partir de este número. Default = 2. Rango válido 1..=16.
    /// Ver `derive_resources()`.
    #[serde(default)]
    pub workers: Option<u32>,
    #[serde(default)]
    pub list_db: Option<bool>,
    #[serde(default)]
    pub addons_path: Option<String>,
    #[serde(default)]
    pub python_libs: Vec<String>,
    /// Master admin password of the tenant's Odoo instance. Used by /web/database/*
    /// endpoints and stored encrypted on the panel side. MANDATORY — NKR nunca
    /// lo genera; el panel es single source of truth. Charset `[A-Za-z0-9._-]{16,128}`.
    pub admin_passwd: String,
    /// Password de login del user "admin" del tenant (login web). Si se manda,
    /// NKR la setea via JSON-RPC tras boot del tenant — antes del 201 al panel.
    /// Esto cierra la ventana donde admin/admin (heredada del template) seguía
    /// funcionando. Si se omite, queda admin/admin (backward compat). Charset
    /// `[A-Za-z0-9._-]{8,128}`.
    #[serde(default)]
    pub admin_user_password: Option<String>,
    /// True por default (las cells NKR siempre están detrás de nginx/Cloudflare).
    /// Opt-out pasando false para tests locales.
    #[serde(default)]
    pub proxy_mode: Option<bool>,
    /// MB to inflate in the VirtIO-Balloon at boot. If omitted (normal case),
    /// the clone inherits `balloon_mb` from the template (default 128 MB for
    /// Odoo). Passing 0 explicitly disables the balloon (not recommended for
    /// Odoo). The per-descriptor cap + dedup make high values safe, but
    /// inflating beyond half the guest RAM can stall the guest.
    #[serde(default)]
    pub balloon_mb: Option<u32>,
    /// Tier de la instancia: `production` (default), `staging` o `dev`.
    /// Determina sizing, dev_mode (hot-reload), rate-limit y cache nginx.
    /// Reglas de bootstrap:
    /// - `production`: comportamiento actual (source opcional, default = primer
    ///   instance alfabético de la cell como template).
    /// - `staging`: REQUIERE `source` (clona DB de un tenant production existente).
    ///   No se puede crear staging sin source. Workers=1 forzado, dev_mode on.
    /// - `dev`: NO acepta `source` (rechazado). Arranca con DB vacía via
    ///   /web/database/create. Workers=1 forzado, dev_mode on. Las instancias
    ///   dev no son clonables (no se pueden usar como source).
    /// Si se omite, default `production` (back-compat).
    #[serde(default)]
    pub tier: Tier,
    /// **Opcional.** Override de `chrs` (CPU quota cgroup; 1 chr = 20% de un
    /// core). Si se omite, NKR aplica el default por tier:
    /// - production: `(2N+1)` donde N=workers (5 chrs para workers=2)
    /// - staging/dev: 10 chrs (= 2 cores quota, dev iteration friendly)
    /// Rango válido: 1..=50. chrs es QUOTA, no reserva — los chrs no usados
    /// quedan disponibles para otros tenants.
    #[serde(default)]
    pub chrs: Option<u32>,
    /// **Opcional (v1.6.5+).** Si `true`, NKR arranca la VM al final del create
    /// (compose up + wait :8069). Si `false` o ausente, el create se queda en
    /// "cold-prepared" — la VM queda registrada pero no se levanta hasta que
    /// el panel haga `POST /actions {start}`. Default = `admin_user_password.is_some()`
    /// (back-compat: hasta v1.6.4 mandar pwd implicaba auto-start).
    ///
    /// Casos:
    /// - source es template del cell + `admin_user_password` provisto → rotate
    ///   admin/admin → arranca (auto_start implícito = true).
    /// - source es template sin `admin_user_password` → cold-prepared (admin/admin
    ///   queda; el panel debe rotar antes de exponer). Mandá `auto_start: true`
    ///   si querés arrancar igualmente sin rotar (no recomendado).
    /// - source es otro tenant (staging, mode=dev) → `admin_user_password`
    ///   PROHIBIDO (el clone hereda la pwd del source via TEMPLATE). Usá
    ///   `auto_start: true` para arrancar el clone tras crearlo.
    #[serde(default)]
    pub auto_start: Option<bool>,
}

/// Recursos derivados a partir del workers count. Single source of truth para
/// el sizing del tenant — el panel sólo pasa workers, NKR computa el resto.
///
/// Fórmulas (workers=N):
///   compose:    chrs = 2N + 1            ram_mb     = 1024·N         MB
///   odoo.conf:  workers = N              soft_bytes = 400·N · 1MiB
///                                        hard_bytes = 750·N · 1MiB
///   balloon:    balloon_mb = ram_mb - limit_memory_hard_mb - 256
///                            (floor 0 si < 64 MB; mejor desactivar que
///                            arrancar un device para devolver migajas)
///
/// Tabla de referencia:
///   N=1 → chrs=3,  ram=1024MB, soft=400MB,  hard=750MB,  balloon=0
///   N=2 → chrs=5,  ram=2048MB, soft=800MB,  hard=1500MB, balloon=292
///   N=4 → chrs=9,  ram=4096MB, soft=1600MB, hard=3000MB, balloon=840
///   N=8 → chrs=17, ram=8192MB, soft=3200MB, hard=6000MB, balloon=1936
pub struct DerivedResources {
    pub workers: u32,
    pub chrs: u32,
    pub ram_mb: u32,
    pub limit_memory_soft: u64, // bytes
    pub limit_memory_hard: u64, // bytes
    /// Target ACTIVE del balloon — VALOR DE BOOT (MB inflados al arrancar).
    /// La VM debe nacer en ACTIVE para que Odoo tenga RAM suficiente para
    /// bootstrap; si arrancara en IDLE (squeeze a 256 MB en DEV) el OOM
    /// killer del guest masacraría a Odoo durante el load de módulos.
    pub balloon_mb: u32,
    /// Target IDLE del balloon — al cual transiciona la VM tras
    /// `balloon_decay_secs` sin renovación SIGUSR2. Si == `balloon_mb`,
    /// la VM se queda estática (sin transición). PROD: == 0 (siempre ACTIVE,
    /// sin decay, doctrina anti-latencia). DEV/STAG: != balloon_mb (dinámico).
    pub balloon_idle_mb: u32,
}

pub fn derive_resources(workers: u32) -> DerivedResources {
    derive_resources_for_tier(workers, Tier::Production)
}

/// Tier-aware resource derivation. **Aplica SOLO a tenants Odoo creados via
/// API.** Las VMs de pg/pgbouncer (creadas vía `nkr compose up` desde YAML)
/// no pasan por este flow — siguen su propio sizing en cell.yml.
///
/// Tabla canónica (CLAUDE.md v2.2 + ajuste v1.6.2 post-OOM Odoo 19):
///
/// | Tier    | VM RAM  | Workers | Soft  | Hard   | Bal ACTIVE (boot) | Bal IDLE (post-decay) | Decay |
/// |---------|---------|---------|-------|--------|-------------------|------------------------|-------|
/// | DEV     | 1300 MB | 0       | 800   | 1000   | 0                 | 256                    | 600s  |
/// | STAGING | 1024 MB | 0       | 600   | 700    | 256               | 768                    | 600s  |
/// | PROD    | 2048 MB | 2       | 640×W | 768×W  | 0                 | 0                      | n/a   |
///
/// DEV se subió de 768→1300 MB y soft/hard 400/512→800/1000 (v1.6.2 tras
/// observar `Server memory limit reached` ciclando con Odoo 19 + 31 módulos
/// custom en threaded mode — el soft de 400 MB era inalcanzable bajo carga
/// normal de DEV).
///
/// Ballooning IDLE/ACTIVE (CLAUDE.md v2.2):
///   - **La VM nace ACTIVE**: balloon_mb es el target de boot. Para DEV es 0
///     (toda la RAM al guest), para STAG 256, para PROD 0.
///   - Tras `balloon_decay_secs` (600s = 10 min) sin renovación SIGUSR2 desde
///     el panel, vmm transiciona a IDLE: balloon = VM_RAM - 256 (la VM se
///     queda con el mínimo del kernel, máxima densidad para el host).
///   - PROD se queda en ACTIVE estáticamente (balloon_idle_mb == balloon_mb
///     == 0) — el state machine no se activa cuando ambos coinciden.
///     Doctrina: "PROD evita latencia de desinflado en picos de tráfico".
///   - **Por qué nacer en ACTIVE**: si la VM arrancara en IDLE (DEV con 256 MB
///     reales), Odoo no completaría bootstrap — kernel + initramfs + Odoo
///     necesitan >256 MB. El OOM killer del guest masacraría init antes de
///     que Odoo levante el puerto 8069. Doctrina cita explícita:
///     "La VM NUNCA puede bajar de 256 MB de RAM real disponible".
///
/// workers=0 = Odoo threaded mode (un solo proceso werkzeug multi-thread,
/// SIN master prefork). `dev_mode` queda VACÍO (v1.6.3 — `reload` agota
/// inotify en virtio-fs, `qweb,xml` recompila templates en cada request →
/// cuelgues; ver BUG_inotify_dev_mode.md). Iteración rápida vía REL_OD/HVC0:
/// `POST /reload` → SIGTERM al proceso → el supervisor loop de nkr-start.sh
/// lo respawnea con código fresh del disco.
///
/// PROD con workers≥4: ver `validate_workers_ram_budget()` para la fórmula
/// `VM_RAM >= 256 + 256 + W*768` y la regla del Grifo (balloon=0 si W>4).
pub fn derive_resources_for_tier(workers: u32, tier: Tier) -> DerivedResources {
    match tier {
        Tier::Dev => DerivedResources {
            workers: 0,
            chrs: 5,
            ram_mb: 1300,
            limit_memory_soft: 800 * 1024 * 1024,
            limit_memory_hard: 1000 * 1024 * 1024,
            balloon_mb: 0,            // ACTIVE (boot): toda la RAM al guest
            // IDLE post-decay conservador: 256 MB squeeze. Deja 1044 MB al
            // guest, suficiente para Odoo idle sin chocar con el hard limit
            // de 1000 MB. (La fórmula clásica ram-256 daría 1044 squeeze →
            // chocaría con el hard, OOM al primer pico de uso post-IDLE.)
            balloon_idle_mb: 256,
        },
        Tier::Staging => DerivedResources {
            workers: 0,
            chrs: 5,
            ram_mb: 1024,
            limit_memory_soft: 600 * 1024 * 1024,
            limit_memory_hard: 700 * 1024 * 1024,
            // v1.6.5+: ballooning suave igual que dev (boot=0, idle=256). El
            // perfil viejo (boot=256, idle=768) ahogaba a Odoo: con 1024 RAM −
            // 768 squeeze IDLE = 256 MB reales, muy por debajo del hard limit
            // de 700 MB → al primer pico post-IDLE el guest entraba en swap o
            // OOM. Ahora boot llega con toda la RAM, decay sólo recupera 256.
            balloon_mb: 0,
            balloon_idle_mb: 256,
        },
        Tier::Production => {
            let w = workers.max(1);
            // Fórmula canónica (per-worker × N + master reserve + OS tax):
            //   VM_RAM ≥ 256 (OS) + 256 (master) + 768 × W
            // Soft/Hard PER WORKER son los de la tabla.
            DerivedResources {
                workers: w,
                chrs: (2 * w) + 1,
                // Default: 2 workers → 2048MB. Si la API recibe override de
                // workers > 2, el caller debe haber pasado el chequeo de
                // validate_workers_ram_budget. Acá simplemente derivamos.
                ram_mb: (256 + 256 + 768 * w).max(1024),
                limit_memory_soft: 640 * (w as u64) * 1024 * 1024,
                limit_memory_hard: 768 * (w as u64) * 1024 * 1024,
                // PROD siempre ACTIVE: balloon=0 estático. Con
                // balloon_idle_mb == balloon_mb, el state machine no se
                // activa en vmm — sin decay, sin transitions. Doctrine:
                // "PROD evita latencia de desinflado en picos de tráfico".
                // También cubre regla del Grifo (W>4 → balloon=0).
                balloon_mb: 0,
                balloon_idle_mb: 0,
            }
        }
    }
}

/// Valida overrides de workers contra la fórmula de seguridad de CLAUDE.md
/// (Reglas del "Grifo" + RAM_INSUFFICIENT_FOR_WORKERS). Solo para tier=
/// production con override explícito (panel pasa workers > default).
///
/// `VM_RAM >= 256 + 256 + (workers × 768)`
///
/// Devuelve `Some(error_response)` si falla, `None` si pasa.
///
/// Hoy no se llama desde el create flow (ram_mb es derivado por la fórmula
/// → siempre cumple). Reservada para el día que la API exponga `ram_mb`
/// override explícito en el body, donde el panel podría violar la fórmula.
#[allow(dead_code)]
pub fn validate_workers_ram_budget(workers: u32, ram_mb: u32) -> Option<IpcResponse> {
    let required = 256u32 + 256u32 + workers.saturating_mul(768);
    if ram_mb < required {
        return Some(IpcResponse::json(400, serde_json::json!({
            "error": "ram_insufficient_for_workers",
            "message": format!(
                "VM_RAM {}MB < {} (= 256 OS + 256 master + workers×768). \
                 Subí ram_mb o bajá workers.",
                ram_mb, required
            ),
            "workers_requested": workers,
            "ram_mb_provided": ram_mb,
            "ram_mb_required": required,
        })));
    }
    None
}


#[derive(Deserialize, Serialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    Start,
    Stop,
    Restart,
}

impl ActionKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "start" => Some(ActionKind::Start),
            "stop" => Some(ActionKind::Stop),
            "restart" => Some(ActionKind::Restart),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub struct ActionReq {
    pub action: ActionKind,
}

// =============================================================================
// Handlers — each returns IpcResponse
// =============================================================================

pub fn handle_health() -> IpcResponse {
    IpcResponse::json(
        200,
        serde_json::json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
        }),
    )
}

pub fn handle_list_cells() -> IpcResponse {
    let cells: Vec<serde_json::Value> = crate::cell::list_cells()
        .into_iter()
        .map(|c| {
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
        })
        .collect();
    IpcResponse::json(
        200,
        serde_json::json!({
            "cells": cells,
            "max_odoos_per_cell": MAX_ODOOS_PER_CELL,
        }),
    )
}

/// `cell_hint`: None for POST /instances (auto-select). Some(cell) for POST /cells/{cell}/instances.
pub fn handle_create(cell_hint: Option<&str>, body_json: &str) -> IpcResponse {
    if body_json.len() > 64 * 1024 {
        return IpcResponse::error(413, "body_too_large", None);
    }
    let mut req: CreateInstanceReq = match serde_json::from_str(body_json) {
        Ok(r) => r,
        Err(_) => {
            return IpcResponse::error(
                400,
                "invalid_json",
                Some("request body is not valid JSON or missing required fields"),
            )
        }
    };

    // ── Normalización contrato v2 (2026-05-15) ──────────────────────────────
    // El panel ahora puede mandar `enterprise: bool` en lugar de `edition`.
    // Mapeamos a edition (precedencia: enterprise > edition).
    if let Some(ent) = req.enterprise {
        req.edition = Some(if ent { crate::cell::Edition::Enterprise } else { crate::cell::Edition::Community });
    }

    // Detección "contrato nuevo": ni URL trae cell ni body trae cell →
    // significa que el panel quiere auto-pick + tier-defaults. En ese caso,
    // ignoramos cualquier `workers` que venga (no rechazamos por compat;
    // sólo NO lo usamos). En contrato viejo (cell explícita) seguimos
    // respetando workers para no romper integraciones existentes.
    let is_v2_contract = cell_hint.is_none() && req.cell.is_none();
    if is_v2_contract && req.workers.is_some() {
        eprintln!("[API] create({}): contrato v2 — ignorando workers={:?} \
                   (los recursos se derivan de tier)",
            req.nkr_name, req.workers);
        req.workers = None;
    }
    let req = req; // re-shadow as immutable from here

    if !is_safe_identifier(&req.nkr_name) {
        return IpcResponse::error(
            400,
            "invalid_nkr_name",
            Some("nkr_name must match [A-Za-z0-9._-]{1,64}"),
        );
    }
    if !is_safe_identifier(&req.odoo_version) {
        return IpcResponse::error(400, "invalid_odoo_version", None);
    }
    if let Some(ref c) = req.cell {
        if !is_safe_identifier(c) {
            return IpcResponse::error(400, "invalid_cell", None);
        }
    }
    if let Some(h) = cell_hint {
        if !is_safe_identifier(h) {
            return IpcResponse::error(400, "invalid_cell_in_url", None);
        }
    }
    if let Some(ref s) = req.source {
        if !is_safe_identifier(s) {
            return IpcResponse::error(400, "invalid_source", None);
        }
    }
    if let Some(ref d) = req.dns {
        if !is_safe_dns(d) {
            return IpcResponse::error(400, "invalid_dns", None);
        }
    }
    if let Some(ref pg) = req.pg_version {
        if !is_safe_identifier(pg) {
            return IpcResponse::error(400, "invalid_pg_version", None);
        }
    }
    if let Some(ref ap) = req.addons_path {
        if !is_safe_addons_path(ap) {
            return IpcResponse::error(400, "invalid_addons_path", None);
        }
    }
    // admin_passwd: OBLIGATORIO. El panel es única fuente. Charset [A-Za-z0-9._-]{16,128}
    // para evitar inyección en odoo.conf y forzar entropía mínima razonable.
    {
        let p = req.admin_passwd.as_str();
        if p.is_empty() {
            return IpcResponse::error(400, "admin_passwd_required",
                Some("admin_passwd is mandatory (panel must generate and persist it)"));
        }
        let bytes = p.as_bytes();
        let ok = bytes.len() >= 16 && bytes.len() <= 128
            && bytes.iter().all(|b| b.is_ascii_alphanumeric()
                || matches!(b, b'.' | b'_' | b'-'));
        if !ok {
            return IpcResponse::error(400, "invalid_admin_passwd",
                Some("must be [A-Za-z0-9._-]{16,128}"));
        }
    }
    // admin_user_password: OPCIONAL. Si presente, NKR la setea en res_users.password
    // del user 'admin' tras boot. Mínimo 8 chars (Odoo no fuerza más, no queremos
    // bloquear casos legítimos). No log, no echo en respuesta.
    if let Some(ref p) = req.admin_user_password {
        let bytes = p.as_bytes();
        let ok = bytes.len() >= 8 && bytes.len() <= 128
            && bytes.iter().all(|b| b.is_ascii_alphanumeric()
                || matches!(b, b'.' | b'_' | b'-'));
        if !ok {
            return IpcResponse::error(400, "invalid_admin_user_password",
                Some("must be [A-Za-z0-9._-]{8,128}"));
        }
    }
    // workers: rango 1..=16 (la única input de sizing que acepta el API).
    if let Some(w) = req.workers {
        if !(1..=16).contains(&w) {
            return IpcResponse::error(400, "invalid_workers",
                Some("workers must be 1..=16"));
        }
    }

    // Reconcile cell: body.cell > URL cell > auto by version.
    let cell_name_req: Option<String> = req.cell.clone().or_else(|| cell_hint.map(|s| s.to_string()));

    let resolved_cell = match cell_name_req {
        Some(name) => {
            if lookup_cell_id(&name).is_none() {
                return IpcResponse::json(
                    404,
                    serde_json::json!({"error":"cell_not_found","cell":name}),
                );
            }
            let cell = match load_cell(&name) {
                Ok(c) => c,
                Err(e) => {
                    return IpcResponse::json(
                        404,
                        serde_json::json!({
                            "error":"cell_load_failed","message":e.to_string()
                        }),
                    )
                }
            };
            match cell.odoo_version.as_deref() {
                Some(v) if v == req.odoo_version => {}
                Some(v) => {
                    return IpcResponse::json(
                        409,
                        serde_json::json!({
                            "error": "version_mismatch",
                            "message": format!("Cell '{}' está en odoo_version={}, panel pidió {}",
                                cell.name, v, req.odoo_version),
                            "cell": cell.name,
                            "cell_version": v,
                            "requested_version": req.odoo_version,
                        }),
                    )
                }
                None => {
                    return IpcResponse::json(
                        409,
                        serde_json::json!({
                            "error": "cell_version_unset",
                            "message": format!("Cell '{}' sin odoo_version en cell.yml", cell.name),
                            "cell": cell.name,
                        }),
                    )
                }
            }
            cell
        }
        None => match select_cell_for_version(&req.odoo_version) {
            Ok(c) => c,
            Err(e) => {
                return IpcResponse::json(
                    409,
                    serde_json::json!({
                        "error": "no_cell_available",
                        "message": e.to_string(),
                        "requested_version": req.odoo_version,
                    }),
                )
            }
        },
    };

    let used = count_odoo_instances(&resolved_cell.name);
    if used >= MAX_ODOOS_PER_CELL {
        return IpcResponse::json(
            409,
            serde_json::json!({
                "error": "cell_full",
                "cell": resolved_cell.name,
                "used": used,
                "max": MAX_ODOOS_PER_CELL,
            }),
        );
    }

    // Source resolution: el tier determina las reglas. Ver la doc de `Tier`.
    //   - tier=production (default): comportamiento legacy según `mode`.
    //     · mode=production: source PROHIBIDO, NKR usa template de cell.
    //     · mode=dev: source REQUERIDO (clone de otro tenant).
    //   - tier=staging: source REQUERIDO + el source DEBE ser tier=production.
    //     Mantenemos staging clonable de prod para reproducir bugs con datos
    //     reales. Internamente clonamos como `mode=dev` (DB completa).
    //   - tier=dev: source PROHIBIDO. Internamente clonamos como
    //     `mode=production` (DB del template, vacía) — el dev hidrata módulos
    //     desde Apps. Bootstrap de DB completamente fresca queda para sesión
    //     futura (hoy reusamos template_DB del cell).
    //
    // El tier también fuerza `effective_mode` para mantener consistencia:
    // staging→Dev (clone full), dev→Production (cell template).
    let effective_mode = match req.tier {
        Tier::Production => req.mode,
        Tier::Staging => InstanceMode::Dev,
        Tier::Dev => InstanceMode::Production,
    };

    // Selección del template default según `edition` (v1.6.5+):
    //   community → <cell>-odoo-template
    //   enterprise → <cell>-odoo-template-enterprise (siembra del operador, una
    //                vez por cell con web_enterprise pre-instalado)
    // Reglas: cuando el `source` viene del template, NKR rota admin/admin después
    // del boot. Cuando el source viene de otro tenant (mode=dev / tier=staging),
    // NKR NO rota — el clone hereda res_users.password del source via TEMPLATE.
    let want_enterprise = matches!(req.edition, Some(crate::cell::Edition::Enterprise));
    let default_template = if want_enterprise {
        crate::cell::cell_template_enterprise_name(&resolved_cell.name)
    } else {
        crate::cell::cell_template_community_name(&resolved_cell.name)
    };
    let template_error_for = |cell: &str, tpl: &str| {
        if tpl.ends_with("-odoo-template-enterprise") {
            IpcResponse::json(409, serde_json::json!({
                "error": "enterprise_template_missing",
                "message": format!("La cell '{}' no tiene template enterprise '{}'. El operador debe sembrarlo una vez: clonar el template community, instalar `web_enterprise` desde la UI, marcarlo como template (disabled:true). Sin él, edition=enterprise no se puede crear via API en esta cell.", cell, tpl),
                "cell": cell,
                "expected_template": tpl,
                "hint": "Ver NKR_API.md §4.4 'Sembrar template enterprise' para el runbook completo.",
            }))
        } else {
            IpcResponse::json(409, serde_json::json!({
                "error": "cell_template_missing",
                "message": format!("La cell '{}' no tiene template '{}'. Crearlo o reseed la cell.", cell, tpl),
                "cell": cell,
                "expected_template": tpl,
            }))
        }
    };
    let check_template_exists = |tpl: &str| -> Option<IpcResponse> {
        let tpl_dir = crate::cell::cells_dir()
            .join(&resolved_cell.name)
            .join("instances")
            .join(tpl);
        if !tpl_dir.exists() {
            Some(template_error_for(&resolved_cell.name, tpl))
        } else {
            None
        }
    };

    let source = match req.tier {
        Tier::Production => match req.mode {
            InstanceMode::Production => {
                if req.source.is_some() {
                    return IpcResponse::json(409, serde_json::json!({
                        "error": "source_not_allowed_in_production",
                        "message": "mode=production siempre clona del template de la cell. Para clonar de otro tenant usá mode=dev con 'source' explícito o tier=staging.",
                        "cell": resolved_cell.name,
                    }));
                }
                if let Some(resp) = check_template_exists(&default_template) {
                    return resp;
                }
                default_template.clone()
            }
            InstanceMode::Dev => match req.source.clone() {
                Some(s) => s,
                None => {
                    return IpcResponse::error(400, "source_required",
                        Some("mode=dev requiere 'source' explícito (nkr_name del tenant fuente). Para crear un tenant fresh usá mode=production."));
                }
            },
        },
        Tier::Staging => {
            let s = match req.source.clone() {
                Some(s) => s,
                None => {
                    return IpcResponse::error(400, "source_required",
                        Some("tier=staging requiere 'source' explícito (nkr_name del tenant production a clonar)."));
                }
            };
            // Validar que el source sea tier=production. Leemos su meta.json.
            // Si el meta.json no existe (tenant sin metadata) o no tiene tier,
            // asumimos production (back-compat con instancias previas a esta
            // feature). Solo bloqueamos si meta.json existe Y tiene tier
            // explícito staging/dev.
            let src_meta_path = crate::cell::cells_dir()
                .join(&resolved_cell.name)
                .join("instances")
                .join(&s)
                .join("meta.json");
            if let Ok(buf) = std::fs::read(&src_meta_path) {
                if let Ok(src_meta) = serde_json::from_slice::<crate::cell::InstanceMeta>(&buf) {
                    if src_meta.tier != Tier::Production {
                        return IpcResponse::json(409, serde_json::json!({
                            "error": "source_must_be_production",
                            "message": format!("tier=staging solo puede clonar de un tenant tier=production. Source '{}' es tier={:?}.",
                                s, src_meta.tier),
                            "source": s,
                            "source_tier": src_meta.tier,
                        }));
                    }
                }
            }
            s
        }
        Tier::Dev => {
            if req.source.is_some() {
                return IpcResponse::json(409, serde_json::json!({
                    "error": "source_not_allowed_in_dev",
                    "message": "tier=dev no acepta 'source' (las instancias dev son standalone, no clonables). Para clonar de prod usá tier=staging.",
                    "cell": resolved_cell.name,
                }));
            }
            if let Some(resp) = check_template_exists(&default_template) {
                return resp;
            }
            default_template.clone()
        }
    };

    // v1.6.5+: `admin_user_password` está PROHIBIDO en clones-from-tenant. El
    // clone hereda la pwd del source via CREATE DATABASE TEMPLATE; intentar
    // change_password con admin/admin falla porque ya no es esa la pwd. El
    // panel debe omitir el campo cuando source != template.
    let is_template_clone = crate::cell::is_template_name(&resolved_cell.name, &source);
    if !is_template_clone && req.admin_user_password.is_some() {
        return IpcResponse::json(400, serde_json::json!({
            "error": "admin_user_password_not_applicable_for_clone",
            "message": format!("source='{}' es otro tenant (no un template del cell). Los clones heredan res_users.password del source via CREATE DATABASE TEMPLATE — NKR no puede rotar la pwd porque admin/admin ya no es válido. Omitir `admin_user_password` del body.", source),
            "source": source.clone(),
            "hint": "El panel ya conoce la pwd del source (la generó él mismo cuando lo creó). Usar esa misma pwd para autenticarse contra el clone, o cambiarla post-create vía JSON-RPC.",
        }));
    }

    let dst_name = ensure_cell_prefix(&resolved_cell.name, &req.nkr_name);

    // Derivar TODOS los recursos a partir de workers + tier. Para tier
    // staging/dev: se fuerza workers=1 + ram=2GB internamente (ver
    // derive_resources_for_tier). Si el panel envió workers=N para staging,
    // se ignora y se loguea un debug aviso (no error — el valor era una
    // sugerencia que tier=staging contradice por diseño).
    if req.tier.is_dev_like() && req.workers.is_some() && req.workers != Some(0) {
        eprintln!("[API] {}: tier={:?} fuerza workers=0 (threaded, panel pidió {}), ignorando.",
            req.nkr_name, req.tier, req.workers.unwrap());
    }
    // Validación per-CLAUDE.md (Reglas del Grifo) — solo aplica a tier=production
    // con override explícito de workers. Dev/Staging tienen perfil fijo.
    if matches!(req.tier, Tier::Production) {
        if let Some(w) = req.workers {
            if w > 16 {
                return IpcResponse::error(400, "invalid_workers",
                    Some("workers must be 1..=16"));
            }
            // VM RAM derivado automáticamente cumple la fórmula. Solo si el
            // panel pasara `ram_mb` explícito (no implementado en el body
            // actual) tendríamos que validarlo aparte. Por ahora skip.
        }
    }
    let r = derive_resources_for_tier(req.workers.unwrap_or(2), req.tier);

    // Override de chrs SOLO en tier=production. Para dev/staging el perfil
    // es fijo (2GB + chrs=5 forzado) y se ignora cualquier override del
    // panel — si el dev necesita más CPU, el operador promueve a production.
    let effective_chrs = if req.tier.is_dev_like() {
        if req.chrs.is_some() && req.chrs != Some(r.chrs) {
            eprintln!("[API] {}: tier={:?} fuerza chrs={} (panel pidió {}), ignorando.",
                req.nkr_name, req.tier, r.chrs, req.chrs.unwrap());
        }
        r.chrs
    } else {
        match req.chrs {
            Some(c) => {
                if !(1..=50).contains(&c) {
                    return IpcResponse::error(400, "invalid_chrs",
                        Some("chrs must be 1..=50 (1 chr = 20% de un core)"));
                }
                c
            }
            None => r.chrs,
        }
    };

    // Si el panel pide edition=enterprise, validar que la cell tenga el repo
    // enterprise descargado. Sin esto el tenant arrancaría con manifests
    // faltantes y warnings de addons_path inválido — peor: el usuario cree
    // que tiene enterprise cuando funcionalmente es community.
    if matches!(req.edition, Some(crate::cell::Edition::Enterprise)) {
        let ent_dir = format!("/mnt/nkr/enterprise/{}", resolved_cell.odoo_version
            .clone().unwrap_or_else(|| req.odoo_version.clone()));
        let mut has_modules = false;
        if let Ok(entries) = std::fs::read_dir(&ent_dir) {
            for e in entries.flatten() {
                let p = e.path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with('.') || name == "nkr-env" { continue; }
                if p.is_dir() && p.join("__manifest__.py").exists() {
                    has_modules = true;
                    break;
                }
            }
        }
        if !has_modules {
            return IpcResponse::json(409, serde_json::json!({
                "error": "enterprise_not_provisioned",
                "message": format!(
                    "edition=enterprise solicitada pero la cell '{}' no tiene el repo Odoo Enterprise descargado en {}. \
                     El panel debe llamar primero `POST /api/v1/cells/{}/enterprise/git` con el deploy_key_b64 (o github_token) del cliente.",
                    resolved_cell.name, ent_dir, resolved_cell.name
                ),
                "cell": resolved_cell.name,
                "odoo_version": resolved_cell.odoo_version.clone().unwrap_or_default(),
                "enterprise_path": ent_dir,
            }));
        }
    }

    let admin_passwd_to_persist = req.admin_passwd.clone();

    let opts = CloneOptions {
        mode: effective_mode,
        no_compose: false,
        dns: req.dns,
        edition: req.edition,
        odoo_version: Some(req.odoo_version.clone()),
        pg_version: req.pg_version,
        workers: Some(r.workers),
        list_db: req.list_db,
        limit_memory_soft: Some(r.limit_memory_soft),
        limit_memory_hard: Some(r.limit_memory_hard),
        addons_path: req.addons_path,
        python_libs: req.python_libs,
        admin_passwd: Some(admin_passwd_to_persist),
        proxy_mode: req.proxy_mode,
        ram_mb: Some(r.ram_mb),
        chrs: Some(effective_chrs),
        balloon_mb: Some(req.balloon_mb.unwrap_or(r.balloon_mb)),
        balloon_idle_mb: Some(r.balloon_idle_mb),
        balloon_decay_secs: Some(600),
        skip_db_clone: false,
        tier: req.tier,
        // Cold-prepared (v1.6.5+): bloque nace `disabled: true` cuando el
        // panel no pide auto-start. El POST /actions {start} flippea a `false`.
        // Default de auto_start: presencia de `admin_user_password` (back-compat
        // con v1.6.4: mandar pwd implicaba arrancar). Para clones-from-tenant
        // (no aceptan admin_user_password) el panel debe usar `auto_start: true`
        // explícito.
        start_disabled: !req.auto_start.unwrap_or(req.admin_user_password.is_some()),
    };

    // v1.6.5+: decisiones de arranque + rotación de pwd:
    //  - source==template + admin_user_password → rotate admin/admin (boot_and_set_admin_password).
    //  - source!=template (clone) + auto_start → solo compose_up_and_wait_ready (sin rotate).
    //  - cold-prepared (no auto_start) → ni se arranca.
    let admin_user_pwd = if is_template_clone {
        req.admin_user_password.clone()
    } else {
        None // validado arriba que es None cuando !is_template_clone
    };
    let auto_start = req.auto_start.unwrap_or(req.admin_user_password.is_some());
    let cell_name = resolved_cell.name.clone();

    // ── Async create (v1.6.4) ──────────────────────────────────────────────
    // El clone + boot del tenant tarda 30-200s: filesystem reflink + DB
    // TEMPLATE + `nkr compose up` con readiness wait (que para PROD prefork se
    // pega al borde de 140s) + opcional set de admin_user_password. Bloquear
    // el HTTP request hasta el final hacía que clientes con timeout corto
    // (panel, o Cloudflare ~100s) vieran 504 aunque el create terminara OK
    // (caso real 2026-05-11, tenant johao-y-richavo: 504 en el panel, tenant
    // perfectamente creado y corriendo). Ahora: TODA la validación es síncrona
    // (los 4xx se devuelven al toque), después el clone se despacha en un
    // thread y se devuelve 202 inmediato. El panel pollea
    //   GET /api/v1/cells/{cell}/instances/{name}/create-status
    // hasta status=ready|failed (o GET /instances/{name} hasta phase=ready).
    // Status file: /mnt/nkr/cells/{cell}/.nkr-creates/{name}.json — vive a
    // nivel cell (no instancia) para sobrevivir al rollback del clone si falla.

    // Colisión de nombre: si ya existe → 409, el panel no debe re-crear.
    if get_instance_info(&dst_name).is_ok() {
        return IpcResponse::json(409, serde_json::json!({
            "error": "instance_already_exists",
            "nkr_name": dst_name.clone(),
            "cell": cell_name.clone(),
            "message": "ya existe un tenant con ese nombre — GET /instances/{name} para ver su estado.",
        }));
    }

    // ¿Hay un create en curso para este nombre? (idempotencia ante reintentos)
    let status_path = create_status_path(&cell_name, &dst_name);
    if let Ok(buf) = std::fs::read_to_string(&status_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&buf) {
            let st = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            let started = v.get("started_at").and_then(|x| x.as_u64()).unwrap_or(0);
            // 'provisioning' reciente (< 15 min) → 202 idempotente. Si es más
            // viejo asumimos que el daemon murió a mitad → permitimos re-crear.
            if st == "provisioning" && now_unix().saturating_sub(started) < 900 {
                return IpcResponse::json(202, serde_json::json!({
                    "nkr_name": dst_name.clone(),
                    "cell": cell_name.clone(),
                    "status": "accepted",
                    "async": true,
                    "message": "ya hay un create en curso para este nombre; poll GET /api/v1/cells/{cell}/instances/{name}/create-status.",
                    "poll": format!("/api/v1/cells/{}/instances/{}/create-status", cell_name, dst_name),
                    "job": v,
                }));
            }
        }
    }

    // Guard atómico in-memory contra dos POST /instances concurrentes del mismo nombre.
    {
        let mut set = match inflight_creates().lock() { Ok(s) => s, Err(p) => p.into_inner() };
        if set.contains(&dst_name) {
            return IpcResponse::json(409, serde_json::json!({
                "error": "create_in_progress",
                "nkr_name": dst_name.clone(),
                "cell": cell_name.clone(),
                "message": "ya hay un POST /instances en curso para este nombre.",
            }));
        }
        set.insert(dst_name.clone());
    }

    // Persistir status 'provisioning' ANTES del spawn — un poll inmediato del
    // panel debe ver el job en curso, no un 404.
    let started_at = now_unix();
    if let Some(dir) = status_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&status_path, serde_json::json!({
        "nkr_name": dst_name.clone(),
        "cell": cell_name.clone(),
        "source": source.clone(),
        "status": "provisioning",
        "phase": "cloning",
        "started_at": started_at,
    }).to_string());

    let source_owned = source.clone();
    let dst_owned = dst_name.clone();
    let cell_owned = cell_name.clone();
    let status_owned = status_path.clone();

    let spawn_result = std::thread::Builder::new()
        .name(format!("nkr-create-{}", dst_name))
        .spawn(move || {
            let _guard = InflightCreateGuard(dst_owned.clone());
            let t0 = std::time::Instant::now();
            let write_status = |v: serde_json::Value| { let _ = std::fs::write(&status_owned, v.to_string()); };

            match clone_instance_with_opts(&source_owned, &dst_owned, &opts) {
                Ok(info) => {
                    // v1.6.5: dos paths post-clone:
                    //  (a) source es template + admin_user_password → rota admin/admin via
                    //      boot_and_set_admin_password (compose up + login + change_password).
                    //  (b) auto_start sin admin_user_password → arranca VM sin rotar pwd
                    //      (clone-from-tenant hereda res_users.password del source).
                    //  (c) cold-prepared → ni se arranca; el bloque tiene `disabled: true`.
                    let boot_phase = if let Some(new_pwd) = admin_user_pwd.as_ref() {
                        // Path (a): rotar pwd implica auto-start. Sólo entramos acá si
                        // is_template_clone (validado en sync layer).
                        write_status(serde_json::json!({
                            "nkr_name": dst_owned, "cell": cell_owned, "source": source_owned,
                            "status": "provisioning", "phase": "setting_admin_pwd",
                            "started_at": started_at,
                        }));
                        if let Err(e) = boot_and_set_admin_password(&info, new_pwd) {
                            eprintln!("[API] create({}) async: admin_password_setup_failed: {}", dst_owned, e);
                            write_status(serde_json::json!({
                                "nkr_name": dst_owned, "cell": cell_owned, "source": source_owned,
                                "status": "failed", "phase": "setting_admin_pwd",
                                "error": "admin_password_setup_failed", "message": e,
                                "started_at": started_at, "finished_at": now_unix(),
                                "elapsed_ms": t0.elapsed().as_millis() as u64,
                                "hint": "El tenant se clonó y arrancó, pero el password del user 'admin' sigue siendo el del template. Reintentar via JSON-RPC/PATCH, o borrar y re-crear.",
                            }));
                            return;
                        }
                        "setting_admin_pwd"
                    } else if auto_start {
                        // Path (b): boot sin rotar. Caso clone-from-tenant + auto_start.
                        write_status(serde_json::json!({
                            "nkr_name": dst_owned, "cell": cell_owned, "source": source_owned,
                            "status": "provisioning", "phase": "starting",
                            "started_at": started_at,
                        }));
                        if let Err(e) = compose_up_and_wait_ready(&info) {
                            eprintln!("[API] create({}) async: boot_failed: {}", dst_owned, e);
                            write_status(serde_json::json!({
                                "nkr_name": dst_owned, "cell": cell_owned, "source": source_owned,
                                "status": "failed", "phase": "starting",
                                "error": "boot_failed", "message": e,
                                "started_at": started_at, "finished_at": now_unix(),
                                "elapsed_ms": t0.elapsed().as_millis() as u64,
                                "hint": "El tenant se clonó pero su VM no respondió :8069 en el wait window. Reintentar via POST /actions {start}.",
                            }));
                            return;
                        }
                        "starting"
                    } else {
                        // Path (c): cold-prepared. No boot.
                        "cold"
                    };

                    eprintln!("[API] create({}) async ok ({}ms, phase={})",
                        dst_owned, t0.elapsed().as_millis(), boot_phase);

                    // Refresh info post-boot (v1.6.5): el `info` de clone_instance está
                    // stale (running:false, port_8069_up:false). Releer para reportar el
                    // estado real al panel.
                    let info_fresh = get_instance_info(&dst_owned).ok();
                    let (running, port_up) = match info_fresh.as_ref() {
                        Some(i) => (i.nkr_status.running, i.nkr_status.port_8069_up),
                        None => (info.nkr_status.running, info.nkr_status.port_8069_up),
                    };

                    write_status(serde_json::json!({
                        "nkr_name": dst_owned, "cell": cell_owned, "source": source_owned,
                        "status": "ready", "phase": "done",
                        "started_at": started_at, "finished_at": now_unix(),
                        "elapsed_ms": t0.elapsed().as_millis() as u64,
                        "running": running,
                        "port_8069_up": port_up,
                        "guest_ip": info.guest_ip,
                        "db_name": info.db_name,
                        "dns": info.dns,
                    }));
                }
                Err(e) => {
                    eprintln!("[API] create({}) async error ({}ms): {}", dst_owned, t0.elapsed().as_millis(), e);
                    write_status(serde_json::json!({
                        "nkr_name": dst_owned, "cell": cell_owned, "source": source_owned,
                        "status": "failed", "phase": "cloning",
                        "error": "clone_failed", "message": e.to_string(),
                        "started_at": started_at, "finished_at": now_unix(),
                        "elapsed_ms": t0.elapsed().as_millis() as u64,
                    }));
                }
            }
        });

    if let Err(e) = spawn_result {
        if let Ok(mut set) = inflight_creates().lock() { set.remove(&dst_name); }
        let _ = std::fs::remove_file(&status_path);
        eprintln!("[API] create({}) thread spawn fail: {}", dst_name, e);
        return IpcResponse::json(503, serde_json::json!({
            "error": "spawn_failed", "nkr_name": dst_name, "cell": cell_name,
        }));
    }

    let poll_path = format!("/api/v1/cells/{}/instances/{}/create-status", cell_name, dst_name);
    IpcResponse::json(202, serde_json::json!({
        "nkr_name": dst_name,
        "cell": cell_name,
        "source": source,
        "status": "accepted",
        "async": true,
        "message": "create despachado en background (10-20s típico, community y enterprise — el theme enterprise viene pre-instalado en el template). Poll GET /api/v1/cells/{cell}/instances/{name}/create-status hasta status=ready|failed.",
        "poll": poll_path,
        "started_at": started_at,
    }))
}

/// `GET /api/v1/cells/{cell}/instances/{name}/create-status` — estado de un
/// create asíncrono lanzado por `POST /instances`. Lee el status file en
/// `/mnt/nkr/cells/{cell}/.nkr-creates/{name}.json`. Si no hay registro pero
/// la instancia existe (creada con un NKR previo síncrono, o el status file ya
/// se purgó), devuelve `status: ready`. Si no hay ni registro ni instancia → 404.
pub fn handle_create_status(cell: &str, nkr_name: &str) -> IpcResponse {
    if !is_safe_identifier(cell) {
        return IpcResponse::error(400, "invalid_cell", None);
    }
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }
    let dst = ensure_cell_prefix(cell, nkr_name);
    let path = create_status_path(cell, &dst);
    match std::fs::read_to_string(&path) {
        Ok(buf) => match serde_json::from_str::<serde_json::Value>(&buf) {
            Ok(v) => IpcResponse::json(200, v),
            Err(_) => IpcResponse::json(200, serde_json::json!({
                "nkr_name": dst, "cell": cell, "status": "unknown",
                "note": "status file corrupto", "raw": buf,
            })),
        },
        Err(_) => {
            if let Ok(info) = get_instance_info(&dst) {
                IpcResponse::json(200, serde_json::json!({
                    "nkr_name": dst, "cell": cell,
                    "status": "ready",
                    "note": "sin registro de create async — la instancia ya existe (creada con NKR previo, o status file purgado)",
                    "running": info.nkr_status.running,
                }))
            } else {
                IpcResponse::json(404, serde_json::json!({
                    "error": "no_create_record",
                    "nkr_name": dst, "cell": cell,
                    "message": "no hay create en curso ni instancia con ese nombre",
                }))
            }
        }
    }
}

/// Arranca el tenant (`nkr compose up -d`) y bloquea hasta que el puerto
/// 8069 responda (max 120s). Idempotente: si la VM ya está arriba, el
/// compose up detecta y retorna inmediato. Usado tanto por
/// `boot_and_set_admin_password` (cuando hay admin password) como por
/// `handle_create` directamente cuando `auto_start=true` sin password.
fn compose_up_and_wait_ready(info: &crate::cell::InstanceInfo) -> Result<(), String> {
    use std::time::{Duration, Instant};

    // 1. Compose up — disparar el tenant. Idempotente: si ya estaba arriba,
    //    el daemon de compose detecta y sigue.
    let cell_dir = std::path::PathBuf::from(&info.instance_dir)
        .parent().and_then(|p| p.parent())
        .ok_or_else(|| "no se pudo resolver cell dir".to_string())?
        .to_path_buf();
    let _ = Command::new("nkr")
        .current_dir(&cell_dir)
        .args(["compose", "up", "-d"])
        .status()
        .map_err(|e| format!("compose up spawn: {}", e))?;

    // 2. Wait HTTP :8069 — polling cada 2s, max 120s. POST /web/database/list
    //    es buen probe: no requiere auth, responde 200 cuando Odoo está listo
    //    para servir requests.
    let host = &info.guest_ip;
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let r = http_post_json(host, 8069, "/web/database/list",
            "{\"jsonrpc\":\"2.0\",\"method\":\"call\",\"params\":{}}",
            None, 3);
        if let Ok((code, _, _)) = r {
            if (200..400).contains(&code) {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err(format!("tenant {} no respondió :8069 en 120s", host))
}

/// Para usuarios que pidieron `admin_user_password` en POST /instances:
/// (1) `nkr compose up -d` para arrancar el tenant, (2) polling TCP :8069 hasta
/// que responda, (3) JSON-RPC login admin/admin → res.users.change_password.
/// Bloquea hasta ~120s. Errores propagan al caller.
///
/// **Enterprise activation NO se hace acá** (v1.6.5): la instalación de
/// `web_enterprise` toma 2-5 min y rompía el target de creación en ≤60s. Ahora
/// vive en `enable_enterprise_async` que se dispara como fase post-ready desde
/// `handle_create`.
fn boot_and_set_admin_password(
    info: &crate::cell::InstanceInfo,
    new_pwd: &str,
) -> Result<(), String> {
    // 1+2: compose up + wait :8069 — extraído al helper para que el flow
    // `auto_start=true sin admin_user_password` también lo pueda usar.
    compose_up_and_wait_ready(info)?;
    let host = &info.guest_ip;

    // 3. JSON-RPC login con admin/admin (default heredada del template).
    let auth_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "call",
        "params": {
            "db": info.db_name,
            "login": "admin",
            "password": "admin",
        }
    }).to_string();
    let (auth_code, set_cookie, auth_resp) = http_post_json(
        host, 8069, "/web/session/authenticate",
        &auth_body, None, 30,
    ).map_err(|e| format!("auth http: {}", e))?;
    if auth_code != 200 {
        return Err(format!("auth status={}", auth_code));
    }
    // Validar que la respuesta tenga uid (login exitoso) — Odoo devuelve 200
    // incluso cuando creds son incorrectas, con result:null.
    let auth_json: serde_json::Value = serde_json::from_str(&auth_resp)
        .map_err(|e| format!("auth json: {}", e))?;
    let uid = auth_json.pointer("/result/uid").and_then(|v| v.as_i64());
    if uid.is_none() {
        return Err("login admin/admin falló (¿template ya tiene otra pwd?)".to_string());
    }
    let cookie = set_cookie
        .ok_or_else(|| "auth no devolvió Set-Cookie".to_string())?;

    // 4. JSON-RPC change_password. res.users.change_password(old, new) sólo
    //    funciona sobre el user autenticado (no admin sobre cualquiera).
    let chg_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "call",
        "params": {
            "model": "res.users",
            "method": "change_password",
            "args": ["admin", new_pwd],
            "kwargs": {},
        }
    }).to_string();
    let (chg_code, _, chg_resp) = http_post_json(
        host, 8069, "/web/dataset/call_kw",
        &chg_body, Some(&cookie), 30,
    ).map_err(|e| format!("change_password http: {}", e))?;
    if chg_code != 200 {
        return Err(format!("change_password status={}", chg_code));
    }
    let chg_json: serde_json::Value = serde_json::from_str(&chg_resp)
        .map_err(|e| format!("change_password json: {}", e))?;
    if chg_json.get("error").is_some() {
        return Err(format!("change_password error: {}",
            chg_json.get("error").unwrap()));
    }
    eprintln!("[API] admin user password set para tenant '{}' (uid={:?})",
        info.nkr_name, uid);
    Ok(())
}


pub fn handle_get_info(nkr_name: &str) -> IpcResponse {
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }
    match get_instance_info(nkr_name) {
        Ok(info) => IpcResponse::json(200, info),
        Err(e) => {
            eprintln!("[API] get_info({}) error: {}", nkr_name, e);
            IpcResponse::json(
                404,
                serde_json::json!({"error":"not_found","nkr_name":nkr_name}),
            )
        }
    }
}

pub fn handle_delete(nkr_name: &str, drop_db: bool) -> IpcResponse {
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }

    // Idempotency fast-path: if the instance is already gone, return 200
    // synchronously without spawning anything. Avoids reserving an in-flight
    // slot for a no-op when the panel retries a DELETE.
    if get_instance_info(nkr_name).is_err() {
        eprintln!("[API] delete({}) idempotente: ya no existía", nkr_name);
        return IpcResponse::json(
            200,
            serde_json::json!({
                "deleted": true,
                "already_deleted": true,
                "nkr_name": nkr_name,
                "drop_db": drop_db,
            }),
        );
    }

    // Snapshot of instance metadata BEFORE the async delete runs. The panel
    // gets cell name + dns + addons_path back in the 202 so it can update its
    // own UI state immediately, instead of trying GET /instances/{name}
    // (which will start returning 404 mid-delete).
    let info_snapshot = get_instance_info(nkr_name).ok();

    // Reserve the in-flight slot — sharing the action set with handle_action,
    // because a delete and a start/stop on the same name are mutually
    // exclusive (the delete races with the action's writes to the same dir).
    {
        let mut set = match inflight_actions().lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        if set.contains(nkr_name) {
            return IpcResponse::json(
                409,
                serde_json::json!({
                    "error": "action_in_progress",
                    "nkr_name": nkr_name,
                    "message": "ya hay un start/stop/restart/delete en curso para esta instancia",
                }),
            );
        }
        set.insert(nkr_name.to_string());
    }

    // Detach: delete_instance can take 60-90s (SIGTERM grace period + DROP
    // DATABASE + remove dir). Synchronous return forced panel HTTP timeouts
    // to fire before NKR finished, leaving the panel UI stuck on 500 while
    // the underlying delete had succeeded. Going async mirrors handle_action.
    // The panel polls GET /instances/{name} until it returns 404.
    let nkr_name_owned = nkr_name.to_string();
    let spawn_result = std::thread::Builder::new()
        .name(format!("nkr-delete-{}", nkr_name))
        .spawn(move || {
            let _guard = InflightActionGuard(nkr_name_owned.clone());
            let started = std::time::Instant::now();
            match delete_instance(&nkr_name_owned, drop_db) {
                Ok(cell) => eprintln!(
                    "[API] delete({},cell={}) async ok ({}ms)",
                    nkr_name_owned, cell, started.elapsed().as_millis()
                ),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("no encontrada") || msg.contains("not found") {
                        eprintln!("[API] delete({}) async: ya no existía ({}ms)",
                            nkr_name_owned, started.elapsed().as_millis());
                    } else {
                        eprintln!("[API] delete({}) async error ({}ms): {}",
                            nkr_name_owned, started.elapsed().as_millis(), msg);
                    }
                }
            }
        });

    if let Err(e) = spawn_result {
        // Spawn failed — release the slot manually since the guard never ran.
        if let Ok(mut set) = inflight_actions().lock() {
            set.remove(nkr_name);
        }
        eprintln!("[API] delete({}) thread spawn fail: {}", nkr_name, e);
        return IpcResponse::json(
            503,
            serde_json::json!({"error":"spawn_failed","nkr_name":nkr_name}),
        );
    }

    let mut payload = serde_json::json!({
        "deleted": "pending",
        "nkr_name": nkr_name,
        "drop_db": drop_db,
        "status": "accepted",
        "async": true,
        "message": "delete dispatched in background. Poll GET /instances/{name} until 404.",
    });
    if let Some(info) = info_snapshot {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("cell".to_string(), serde_json::Value::String(info.cell));
            if let Some(dns) = info.dns {
                obj.insert("dns".to_string(), serde_json::Value::String(dns));
            }
        }
    }

    IpcResponse::json(202, payload)
}

// In-flight set: una start/stop/restart en curso por nkr_name.
// Evita races en TAP/iptables/state cuando el panel dispara dos acciones
// seguidas (ej. webhook que pide restart 2× por reintento).
fn inflight_actions() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static INFLIGHT: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    INFLIGHT.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

struct InflightActionGuard(String);
impl Drop for InflightActionGuard {
    fn drop(&mut self) {
        let mut set = match inflight_actions().lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        set.remove(&self.0);
    }
}

// In-flight set para creates asíncronos (POST /instances). Evita que dos POST
// concurrentes del mismo nkr_name disparen dos clones a la vez.
fn inflight_creates() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static INFLIGHT: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    INFLIGHT.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

struct InflightCreateGuard(String);
impl Drop for InflightCreateGuard {
    fn drop(&mut self) {
        let mut set = match inflight_creates().lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        set.remove(&self.0);
    }
}

/// Unix epoch en segundos (best-effort: 0 si el reloj está antes de 1970).
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Path del status file de un create asíncrono. Vive a nivel CELL (no
/// instancia) para sobrevivir al rollback del clone si éste falla.
fn create_status_path(cell: &str, dst_name: &str) -> std::path::PathBuf {
    crate::cell::cells_dir()
        .join(cell)
        .join(".nkr-creates")
        .join(format!("{}.json", dst_name))
}

pub fn handle_action(nkr_name: &str, action_str: &str) -> IpcResponse {
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }
    let action = match ActionKind::parse(action_str) {
        Some(a) => a,
        None => {
            return IpcResponse::error(
                400,
                "invalid_action",
                Some("expected start|stop|restart"),
            )
        }
    };

    // Resolve cell dir for `nkr compose up` during start/restart.
    let info = get_instance_info(nkr_name);
    let cell_dir = info
        .as_ref()
        .ok()
        .and_then(|i| {
            std::path::PathBuf::from(&i.instance_dir)
                .parent()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
        });

    // Reserve the in-flight slot BEFORE spawning. Si ya hay una acción para
    // esta instancia → 409 (el panel debería polear nkr_status en lugar de
    // re-disparar).
    {
        let mut set = match inflight_actions().lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        if set.contains(nkr_name) {
            return IpcResponse::json(
                409,
                serde_json::json!({
                    "error": "action_in_progress",
                    "nkr_name": nkr_name,
                    "message": "ya hay un start/stop/restart en curso para esta instancia",
                }),
            );
        }
        set.insert(nkr_name.to_string());
    }

    // Snapshot del info ANTES (el panel ya lo necesita para addons_path/dns
    // del response inmediato; nkr_status se rescata después con GET).
    let info_snapshot = info.ok();

    // Detach: stop+start puede tardar 60–130s y la API HTTP es síncrona —
    // sin esto el panel cortaba por timeout y el restart quedaba colgado en
    // el medio. El proxy tiene MAX_INFLIGHT=64; bloquear un slot por minuto
    // y medio por VM era inviable. El panel debe polear
    // GET /api/v1/cells/{cell}/instances/{name} → nkr_status.port_8069_up
    // para detectar readiness.
    let nkr_name_owned = nkr_name.to_string();
    let spawn_result = std::thread::Builder::new()
        .name(format!("nkr-action-{}", nkr_name))
        .spawn(move || {
            let _guard = InflightActionGuard(nkr_name_owned.clone());
            let started = std::time::Instant::now();
            let result = match action {
                ActionKind::Start => start_instance(&nkr_name_owned, cell_dir.as_deref()),
                ActionKind::Stop => stop_instance(&nkr_name_owned),
                ActionKind::Restart => restart_instance(&nkr_name_owned, cell_dir.as_deref()),
            };
            let dur = started.elapsed();
            match result {
                Ok(()) => eprintln!(
                    "[API] action({},{:?}) async ok ({}ms)",
                    nkr_name_owned, action, dur.as_millis()
                ),
                Err(e) => eprintln!(
                    "[API] action({},{:?}) async error ({}ms): {}",
                    nkr_name_owned, action, dur.as_millis(), e
                ),
            }
        });

    if let Err(e) = spawn_result {
        // Spawn falló — liberar slot manualmente (el guard nunca corrió).
        if let Ok(mut set) = inflight_actions().lock() {
            set.remove(nkr_name);
        }
        eprintln!("[API] action({},{:?}) thread spawn fail: {}", nkr_name, action, e);
        return IpcResponse::json(
            503,
            serde_json::json!({"error":"spawn_failed","nkr_name":nkr_name}),
        );
    }

    IpcResponse::json(
        202,
        serde_json::json!({
            "action": action,
            "nkr_name": nkr_name,
            "status": "accepted",
            "async": true,
            "info": info_snapshot,
        }),
    )
}

fn start_instance(
    nkr_name: &str,
    cell_dir: Option<&std::path::Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    if crate::state::list_vms().iter().any(|v| v.name == nkr_name) {
        return Ok(());
    }
    let dir = cell_dir.ok_or("no se pudo resolver cell dir para start")?;

    // Cold-prepared (v1.6.5+): si el bloque del compose tiene `disabled: true`
    // — caso de tenants creados sin `admin_user_password` — flippearlo a
    // `false` antes del compose up; si no, el filtro de compose lo omitiría.
    // Idempotente: si ya estaba `false`, no escribe.
    let cell_name = dir.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if !cell_name.is_empty() {
        if let Err(e) = crate::cell::set_compose_block_disabled(cell_name, nkr_name, false) {
            eprintln!("[API] start({}) WARN: no pude flippear disabled→false: {} \
                (sigo con compose up, si el bloque ya estaba habilitado funciona)",
                nkr_name, e);
        }
    }

    let status = Command::new("nkr")
        .current_dir(dir)
        .args(["compose", "up", "-d"])
        .status()?;
    if !status.success() {
        return Err("nkr compose up falló".into());
    }
    Ok(())
}

fn stop_instance(nkr_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let vm = crate::state::list_vms().into_iter().find(|v| v.name == nkr_name);
    match vm {
        Some(v) => crate::state::stop_vm(v.cell_id, v.vm_id),
        None => Ok(()),
    }
}

fn restart_instance(
    nkr_name: &str,
    cell_dir: Option<&std::path::Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    stop_instance(nkr_name)?;
    std::thread::sleep(std::time::Duration::from_millis(200));
    start_instance(nkr_name, cell_dir)
}

pub fn handle_logs(
    nkr_name: &str,
    tail: Option<usize>,
    from_offset: Option<u64>,
    max_lines: Option<usize>,
    wait_ms: Option<u64>,
) -> IpcResponse {
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }
    let info = match get_instance_info(nkr_name) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[API] logs({}) error: {}", nkr_name, e);
            return IpcResponse::error(404, "not_found", None);
        }
    };

    // Modo cursor: from_offset presente → read from offset, long-poll opcional.
    // Modo tail: sin from_offset → últimas N líneas desde el final del archivo.
    if let Some(mut start) = from_offset {
        let max_lines = max_lines.unwrap_or(500).clamp(1, 10_000);
        let wait_ms = wait_ms.unwrap_or(0).min(25_000);

        let file_size = match std::fs::metadata(&info.logs_path) {
            Ok(m) => m.len(),
            Err(_) => return IpcResponse::error(404, "log_file_missing", None),
        };

        // Rotation detection: si el archivo es MÁS PEQUEÑO que from_offset
        // → fue truncado/rotado. El panel debe reiniciar desde 0.
        let rotated = start > file_size;
        if rotated {
            start = 0;
        }

        // Long-poll: si nada nuevo, esperar hasta wait_ms.
        let file_size = if !rotated && start == file_size && wait_ms > 0 {
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_millis(wait_ms);
            let mut cur = file_size;
            while std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(250));
                match std::fs::metadata(&info.logs_path) {
                    Ok(m) if m.len() > cur => { cur = m.len(); break; }
                    Ok(m) if m.len() < cur => { cur = m.len(); break; } // rotate mid-wait
                    _ => {}
                }
            }
            cur
        } else {
            file_size
        };

        // Rotation mid-wait: si el file_size es menor que start ahora, rotó.
        let (start, rotated) = if file_size < start { (0u64, true) } else { (start, rotated) };

        let (lines, next_offset) = read_forward_lines(
            &info.logs_path, start, max_lines, 4 * 1024 * 1024,
        );

        return IpcResponse::json(200, serde_json::json!({
            "nkr_name": nkr_name,
            "logs_path": info.logs_path,
            "mode": "cursor",
            "lines": lines,
            "from_offset": start,
            "next_offset": next_offset,
            "file_size": file_size,
            "rotated": rotated,
            "eof": next_offset >= file_size,
        }));
    }

    // Modo tail clásico (retrocompatible).
    let tail = tail.unwrap_or(200).min(10_000);
    let tail_lines = read_tail_lines(&info.logs_path, tail, 4 * 1024 * 1024);
    let file_size = std::fs::metadata(&info.logs_path).map(|m| m.len()).unwrap_or(0);
    IpcResponse::json(
        200,
        serde_json::json!({
            "nkr_name": nkr_name,
            "logs_path": info.logs_path,
            "mode": "tail",
            "tail": tail,
            "lines": tail_lines,
            "next_offset": file_size,
            "file_size": file_size,
        }),
    )
}

/// Forward read desde `start`. Devuelve (líneas, next_offset byte-accurate).
/// Cap: `max_bytes` en RAM y `max_lines` líneas.
fn read_forward_lines(path: &str, start: u64, max_lines: usize, max_bytes: usize)
    -> (Vec<String>, u64)
{
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return (Vec::new(), start),
    };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return (Vec::new(), start);
    }
    // Leer como máximo max_bytes; cortar en el último \n que entra.
    let mut buf = vec![0u8; max_bytes];
    let n = match f.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return (Vec::new(), start),
    };
    buf.truncate(n);

    // Buscar el último \n para no devolver una línea parcial.
    let last_nl = buf.iter().rposition(|&b| b == b'\n');
    let (consumed_bytes, slice): (usize, &[u8]) = match last_nl {
        Some(p) => (p + 1, &buf[..=p]),
        None if n == 0 => (0, &[][..]),
        None => {
            // Archivo sin '\n' hasta ahora — devolver partial-less y no avanzar.
            return (Vec::new(), start);
        }
    };

    let text = String::from_utf8_lossy(slice);
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();

    // Si exceden max_lines, truncar Y calcular el offset real hasta esa línea
    // para que el panel pueda seguir desde ahí sin saltarse nada.
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        // Re-encontrar el offset byte-accurate de la line max_lines-ésima.
        let mut consumed = 0usize;
        let mut count = 0usize;
        for (i, &b) in slice.iter().enumerate() {
            if b == b'\n' {
                count += 1;
                if count == max_lines {
                    consumed = i + 1;
                    break;
                }
            }
        }
        return (lines, start + consumed as u64);
    }

    (lines, start + consumed_bytes as u64)
}

// =============================================================================
// Module install / upgrade / uninstall — vía Odoo JSON-RPC
// =============================================================================

pub fn handle_modules_action(
    nkr_name: &str,
    op: &str,
    modules: &[String],
    admin_login: &str,
    admin_password: &str,
) -> IpcResponse {
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }
    let odoo_method = match op {
        "install" => "button_immediate_install",
        "upgrade" => "button_immediate_upgrade",
        "uninstall" => "button_immediate_uninstall",
        _ => return IpcResponse::error(400, "invalid_op",
            Some("expected install|upgrade|uninstall")),
    };
    if modules.is_empty() {
        return IpcResponse::error(400, "missing_modules",
            Some("provide at least one module name"));
    }
    if modules.len() > 64 {
        return IpcResponse::error(400, "too_many_modules",
            Some("max 64 per call"));
    }
    for m in modules {
        // Odoo module names: ASCII alphanum + underscore, 1..64.
        let ok = !m.is_empty() && m.len() <= 64
            && m.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !ok {
            return IpcResponse::error(400, "invalid_module_name",
                Some(&format!("'{}': expected [A-Za-z0-9_]{{1,64}}", m)));
        }
    }
    if admin_login.is_empty() || admin_login.len() > 128
        || admin_login.bytes().any(|b| matches!(b, b'\n' | b'\r' | 0)) {
        return IpcResponse::error(400, "invalid_admin_login", None);
    }
    if admin_password.len() < 4 || admin_password.len() > 512
        || admin_password.bytes().any(|b| matches!(b, b'\n' | b'\r' | 0)) {
        return IpcResponse::error(400, "invalid_admin_password", None);
    }

    let info = match get_instance_info(nkr_name) {
        Ok(i) => i,
        Err(_) => return IpcResponse::error(404, "instance_not_found", None),
    };
    if !info.nkr_status.running || !info.nkr_status.port_8069_up {
        return IpcResponse::error(502, "odoo_not_ready",
            Some("VM must be running and :8069 up"));
    }

    // 1) Autenticar — /web/session/authenticate devuelve uid + session cookie.
    let auth_body = serde_json::json!({
        "jsonrpc": "2.0",
        "params": {
            "db": info.db_name,
            "login": admin_login,
            "password": admin_password,
        }
    }).to_string();
    let (auth_code, auth_cookie, auth_resp) = match http_post_json(
        &info.guest_ip, 8069, "/web/session/authenticate",
        &auth_body, None, 30,
    ) {
        Ok(t) => t,
        Err(e) => return IpcResponse::error(502, "odoo_auth_failed",
            Some(&e)),
    };
    if auth_code != 200 {
        return IpcResponse::error(502, "odoo_auth_http_error",
            Some(&format!("code={}", auth_code)));
    }
    let uid = match serde_json::from_str::<serde_json::Value>(&auth_resp)
        .ok()
        .and_then(|v| v.get("result").and_then(|r| r.get("uid").cloned()))
        .and_then(|u| u.as_i64())
    {
        Some(u) if u > 0 => u,
        _ => return IpcResponse::error(401, "odoo_auth_rejected",
            Some("wrong admin_login/admin_password, or no DB")),
    };
    let cookie = match auth_cookie {
        Some(c) => c,
        None => return IpcResponse::error(502, "odoo_no_session_cookie", None),
    };

    // 2) Buscar los ir.module.module por nombre. execute_kw via /jsonrpc.
    let search_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "call",
        "params": {
            "service": "object",
            "method": "execute_kw",
            "args": [
                info.db_name, uid, admin_password,
                "ir.module.module", "search_read",
                [[["name", "in", modules]]],
                {"fields": ["id", "name", "state"]}
            ]
        }
    }).to_string();
    let (search_code, _, search_resp) = match http_post_json(
        &info.guest_ip, 8069, "/jsonrpc", &search_body, Some(&cookie), 30,
    ) {
        Ok(t) => t,
        Err(e) => return IpcResponse::error(502, "odoo_search_failed", Some(&e)),
    };
    if search_code != 200 {
        return IpcResponse::error(502, "odoo_search_http_error",
            Some(&format!("code={}", search_code)));
    }
    let search_val: serde_json::Value = match serde_json::from_str(&search_resp) {
        Ok(v) => v,
        Err(_) => return IpcResponse::error(502, "odoo_search_parse_error", None),
    };
    if let Some(err) = search_val.get("error") {
        return IpcResponse::json(502, serde_json::json!({
            "error": "odoo_search_rpc_error",
            "detail": err,
        }));
    }
    let found = search_val.get("result").and_then(|r| r.as_array())
        .cloned().unwrap_or_default();

    // Armar { name → (id, state) } y detectar módulos faltantes.
    let mut module_map: std::collections::HashMap<String, (i64, String)> =
        std::collections::HashMap::new();
    for m in &found {
        let id = m.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let name = m.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let state = m.get("state").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if id > 0 && !name.is_empty() {
            module_map.insert(name, (id, state));
        }
    }
    let missing: Vec<String> = modules.iter()
        .filter(|m| !module_map.contains_key(*m))
        .cloned().collect();
    if !missing.is_empty() {
        return IpcResponse::json(404, serde_json::json!({
            "error": "modules_not_found",
            "missing": missing,
            "hint": "module must exist in the database (addons_path + update apps list)",
        }));
    }

    // 3) Ejecutar la acción sobre los IDs. button_immediate_* takes `[ids]` y
    // bloquea hasta completar (instala deps, carga XML, recomputa views).
    let ids: Vec<i64> = modules.iter()
        .filter_map(|m| module_map.get(m).map(|(id, _)| *id))
        .collect();
    let action_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "call",
        "params": {
            "service": "object",
            "method": "execute_kw",
            "args": [
                info.db_name, uid, admin_password,
                "ir.module.module", odoo_method,
                [ids],
                {}
            ]
        }
    }).to_string();
    let start = std::time::Instant::now();
    // Timeout generoso: instalación con many2many tables puede tomar 2-5 min.
    let (action_code, _, action_resp) = match http_post_json(
        &info.guest_ip, 8069, "/jsonrpc", &action_body, Some(&cookie), 600,
    ) {
        Ok(t) => t,
        Err(e) => return IpcResponse::error(502, "odoo_action_failed", Some(&e)),
    };
    let elapsed_ms = start.elapsed().as_millis() as u64;

    if action_code != 200 {
        return IpcResponse::error(502, "odoo_action_http_error",
            Some(&format!("code={}", action_code)));
    }
    let action_val: serde_json::Value = match serde_json::from_str(&action_resp) {
        Ok(v) => v,
        Err(_) => return IpcResponse::error(502, "odoo_action_parse_error", None),
    };
    if let Some(err) = action_val.get("error") {
        return IpcResponse::json(500, serde_json::json!({
            "error": format!("odoo_{}_failed", op),
            "elapsed_ms": elapsed_ms,
            "detail": err,
            "modules": modules,
        }));
    }

    IpcResponse::json(200, serde_json::json!({
        "nkr_name": nkr_name,
        "op": op,
        "modules": modules,
        "elapsed_ms": elapsed_ms,
        "status": "ok",
    }))
}

/// HTTP POST con JSON body y manejo mínimo de cookies. Retorna
/// (status_code, primera Set-Cookie, body). `cookie_in` se envía como Cookie: header.
fn http_post_json(
    host: &str, port: u16, path: &str,
    body: &str, cookie_in: Option<&str>, timeout_secs: u64,
) -> Result<(u16, Option<String>, String), String> {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
    use std::time::Duration;

    let addr: SocketAddr = format!("{}:{}", host, port).to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next().ok_or_else(|| "no address resolved".to_string())?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| format!("connect: {}", e))?;
    stream.set_read_timeout(Some(Duration::from_secs(timeout_secs))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(timeout_secs))).ok();

    let cookie_hdr = match cookie_in {
        Some(c) => format!("Cookie: {}\r\n", c),
        None => String::new(),
    };
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: nkr-daemon/1.5\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n{}Connection: close\r\n\r\n{}",
        path, host, port, body.len(), cookie_hdr, body
    );
    stream.write_all(req.as_bytes()).map_err(|e| format!("write: {}", e))?;

    let mut resp = Vec::with_capacity(4096);
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                resp.extend_from_slice(&buf[..n]);
                if resp.len() > 4 * 1024 * 1024 { break; } // cap 4 MiB
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&resp).to_string();
    let code: u16 = text.split_whitespace().nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let header_end = text.find("\r\n\r\n").unwrap_or(text.len());
    let headers_s = &text[..header_end];
    // Primera Set-Cookie (Odoo sólo manda session_id — basta con eso).
    let mut cookie_out: Option<String> = None;
    for line in headers_s.lines() {
        if let Some(rest) = line.strip_prefix("Set-Cookie: ")
            .or_else(|| line.strip_prefix("set-cookie: "))
        {
            // Tomar la parte NAME=VALUE antes del primer ';'.
            let pair = rest.split(';').next().unwrap_or("").trim().to_string();
            if !pair.is_empty() {
                cookie_out = Some(pair);
                break;
            }
        }
    }
    let body_part = if header_end + 4 <= text.len() {
        text[header_end + 4..].to_string()
    } else {
        String::new()
    };
    Ok((code, cookie_out, body_part))
}

/// Reads the last `max_lines` lines of the file without loading > `max_bytes` in RAM.
/// Strategy: seek to end, read 64 KiB chunks backwards, count '\n' until we
/// have max_lines+1 or reach max_bytes.
fn read_tail_lines(path: &str, max_lines: usize, max_bytes: usize) -> Vec<String> {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    const CHUNK: usize = 64 * 1024;
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let file_size = match f.seek(SeekFrom::End(0)) {
        Ok(n) => n as usize,
        Err(_) => return Vec::new(),
    };
    let read_cap = file_size.min(max_bytes);
    let start_off = file_size.saturating_sub(read_cap);
    let mut buf = Vec::with_capacity(read_cap);
    let mut pos = file_size;
    while pos > start_off {
        let chunk_start = pos.saturating_sub(CHUNK).max(start_off);
        let chunk_size = pos - chunk_start;
        if f.seek(SeekFrom::Start(chunk_start as u64)).is_err() {
            break;
        }
        let mut tmp = vec![0u8; chunk_size];
        if f.read_exact(&mut tmp).is_err() {
            break;
        }
        tmp.extend_from_slice(&buf);
        buf = tmp;
        pos = chunk_start;
        let newlines = buf.iter().filter(|&&b| b == b'\n').count();
        if newlines > max_lines {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].iter().map(|s| s.to_string()).collect()
}

// =============================================================================
// DNS provisioning — nginx vhost + Let's Encrypt cert por tenant
// =============================================================================
//
// El panel remoto manda `POST /cells/<cell>/instances/<nkr_name>/dns { dns }` y
// el daemon:
//   1. Encuentra la IP del tenant (guest_ip) desde meta.json.
//   2. Emite (o renueva) cert Let's Encrypt para el dns vía `certbot --webroot`.
//   3. Escribe /etc/nginx/sites-available/<nkr_name> con el vhost generado.
//   4. Symlink en sites-enabled/.
//   5. `nginx -t` → `systemctl reload nginx`.
//
// Idempotente: volver a llamar con el mismo dns es un no-op efectivo.
// Cambiar el dns (llamar con uno distinto) reemplaza el vhost y emite cert nuevo.

const NGINX_SITES_AVAILABLE: &str = "/etc/nginx/sites-available";
const NGINX_SITES_ENABLED: &str = "/etc/nginx/sites-enabled";
const LETSENCRYPT_LIVE: &str = "/etc/letsencrypt/live";
const ACME_WEBROOT: &str = "/var/www/html";
const CERTBOT_EMAIL: &str = "antovargas556@gmail.com";

pub fn handle_create_dns(nkr_name: &str, dns: &str, enable_ws: bool) -> IpcResponse {
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }
    if !is_safe_dns(dns) {
        return IpcResponse::error(400, "invalid_dns", None);
    }

    let info = match get_instance_info(nkr_name) {
        Ok(i) => i,
        Err(_) => return IpcResponse::error(404, "instance_not_found", None),
    };
    let guest_ip = info.guest_ip.clone();

    // 1. Emit cert. Usa webroot (no modifica nginx), sirve ACME por el vhost
    //    default en :80 con `location /.well-known/acme-challenge/ { root ... }`.
    //    --expand=false significa que cert por dns aparte; si ya existe, certbot
    //    es idempotente.
    let _ = std::fs::create_dir_all(ACME_WEBROOT);
    let cert_out = Command::new("certbot")
        .args([
            "certonly", "--non-interactive", "--agree-tos",
            "--email", CERTBOT_EMAIL,
            "--no-eff-email",
            "--webroot", "--webroot-path", ACME_WEBROOT,
            "-d", dns,
        ])
        .output();
    match cert_out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).to_string();
            let out = String::from_utf8_lossy(&o.stdout).to_string();
            eprintln!("[API-DNS] certbot falló para {}: {}\n{}", dns, err, out);
            // Fallback: si el cert ya existe con otra firma, no bloqueemos;
            // sólo fallar si el dir no está.
            let cert_dir = format!("{}/{}", LETSENCRYPT_LIVE, dns);
            if !std::path::Path::new(&cert_dir).exists() {
                return IpcResponse::json(422, serde_json::json!({
                    "error": "cert_issue_failed",
                    "dns": dns,
                    "log_tail": tail_str(&format!("{}{}", out, err), 40),
                }));
            }
        }
        Err(e) => {
            return IpcResponse::error(500, "certbot_spawn_failed", Some(&e.to_string()));
        }
    }

    // 2. Generar vhost. El tier de la instancia (production / staging / dev)
    // determina si se aplica rate-limit en /web/login + cache nginx en
    // /web/static y /web/assets. Para tier dev-like, todo eso se desactiva
    // para que el dev iterando no se topa con caches/throttling.
    let vhost_body = render_tenant_vhost(nkr_name, dns, &guest_ip, enable_ws, info.meta.tier);
    let vhost_path = format!("{}/{}", NGINX_SITES_AVAILABLE, nkr_name);
    if let Err(e) = std::fs::write(&vhost_path, &vhost_body) {
        return IpcResponse::error(500, "vhost_write_failed", Some(&e.to_string()));
    }

    // 3. Symlink en sites-enabled (idempotente — si existe y apunta bien, skip).
    let link_path = format!("{}/{}", NGINX_SITES_ENABLED, nkr_name);
    if !std::path::Path::new(&link_path).exists() {
        use std::os::unix::fs::symlink;
        let _ = std::fs::remove_file(&link_path);
        if let Err(e) = symlink(&vhost_path, &link_path) {
            return IpcResponse::error(500, "vhost_enable_failed", Some(&e.to_string()));
        }
    }

    // 4. nginx -t + reload. Si -t falla, rollback del symlink para no romper nginx.
    let test = Command::new("nginx").arg("-t").output();
    let test_ok = matches!(&test, Ok(o) if o.status.success());
    if !test_ok {
        let log = test.as_ref().map(|o| {
            format!("{}\n{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr))
        }).unwrap_or_else(|_| "nginx -t spawn failed".to_string());
        let _ = std::fs::remove_file(&link_path);
        return IpcResponse::json(422, serde_json::json!({
            "error": "nginx_config_invalid",
            "log_tail": tail_str(&log, 30),
        }));
    }
    let reload = Command::new("systemctl").args(["reload", "nginx"]).status();
    if !reload.map(|s| s.success()).unwrap_or(false) {
        return IpcResponse::error(500, "nginx_reload_failed", None);
    }

    // 5. Best-effort: setear web.base.url=https://<dns> + freeze en la DB del
    //    tenant. Si la DB no existe todavía (panel llamó /dns antes de /init-db),
    //    no es error — el tenant arrancará con el placeholder `http://...`
    //    y el panel debería re-llamar /dns post init-db.
    //
    //    SIN este step: Odoo carga /web/login con URLs http:// internas, el
    //    browser bloquea por mixed-content cuando la página se sirve por HTTPS,
    //    el JS de OWL no hidrata el form (queda con clase `d-none` heredada del
    //    template), y el cliente ve un login en blanco.
    let https_url = format!("https://{}", dns);
    let base_url_status = match update_tenant_base_url(&info.cell, &info.db_name, &https_url) {
        Ok(()) => {
            eprintln!("[API-DNS] web.base.url={} sealed para tenant {}", https_url, nkr_name);
            "updated"
        }
        Err(e) if e == "db_not_present" => {
            eprintln!("[API-DNS] DB '{}' aún no existe; web.base.url se aplicará al re-llamar /dns tras init-db", info.db_name);
            "skipped_db_missing"
        }
        Err(e) => {
            eprintln!("[API-DNS] WARN: update_tenant_base_url falló: {}", e);
            "failed_nonblocking"
        }
    };

    IpcResponse::json(200, serde_json::json!({
        "nkr_name": nkr_name,
        "dns": dns,
        "guest_ip": guest_ip,
        "https_url": https_url,
        "vhost_path": vhost_path,
        "cert_path": format!("{}/{}/fullchain.pem", LETSENCRYPT_LIVE, dns),
        "websocket_enabled": enable_ws,
        "base_url_update": base_url_status,
    }))
}

/// Auto-seal de `web.base.url` post-init-db (recomendación #4 del panel).
/// Se invoca al final del job background de `handle_init_db` si la DB se creó
/// exitosamente. Best-effort: errores se loguean pero no fallan el init-db
/// (la DB ya está creada y usable).
///
/// Si `dns` es None, el tenant no tiene vhost provisionado todavía → no hay
/// nada que sellar (el panel debe llamar `POST /dns` después, lo cual TAMBIÉN
/// intenta el seal — el código `base_url_update` del response cubre eso).
fn auto_seal_base_url(nkr_name: &str, cell: &str, db_name: &str, dns: Option<&str>) {
    let Some(dns) = dns else {
        eprintln!("[API] init-db({}) auto-seal skipped: no DNS provisioned yet",
            nkr_name);
        return;
    };
    let https_url = format!("https://{}", dns);
    match update_tenant_base_url(cell, db_name, &https_url) {
        Ok(()) => {
            eprintln!("[API] init-db({}) auto-sealed web.base.url={}",
                nkr_name, https_url);
        }
        Err(e) => {
            eprintln!("[API] init-db({}) auto-seal failed (best-effort): {}",
                nkr_name, e);
        }
    }
}

/// Setea `web.base.url=<https_url>` y `web.base.url.freeze=True` en la DB del
/// tenant. Si la DB no existe, retorna Err("db_not_present") (caller decide).
///
/// El freeze evita que Odoo auto-overwrite el valor con el host del primer
/// request HTTP entrante (ese es el comportamiento default de Odoo y la causa
/// de que la URL quede `http://...` después de init-db inicial).
fn update_tenant_base_url(cell_name: &str, db_name: &str, https_url: &str)
    -> Result<(), String>
{
    use crate::cell::lookup_cell_id;
    let cell_id = lookup_cell_id(cell_name)
        .ok_or_else(|| "cell_not_found".to_string())?;
    let pg_ip = format!("10.0.{}.2", cell_id);

    // Sanity: db existe?
    let chk = Command::new("psql")
        .args(["-h", &pg_ip, "-U", "odoo", "-d", "postgres", "-tAc",
               &format!("SELECT 1 FROM pg_database WHERE datname='{}';", db_name)])
        .env("PGPASSWORD", "odoo")
        .env("PGCONNECT_TIMEOUT", "5")
        .output()
        .map_err(|e| format!("psql spawn (probe): {}", e))?;
    if !chk.status.success() {
        return Err(format!("psql probe failed: {}",
            String::from_utf8_lossy(&chk.stderr)));
    }
    let present = String::from_utf8_lossy(&chk.stdout).trim() == "1";
    if !present {
        return Err("db_not_present".into());
    }

    // UPSERT de las dos keys. https_url ya pasó is_safe_dns + es-construido por
    // NKR (no input directo), así que no hay riesgo de SQL injection.
    let sql = format!(
        "INSERT INTO ir_config_parameter (key, value, create_date, write_date, create_uid, write_uid) \
         VALUES ('web.base.url','{url}',NOW(),NOW(),1,1) \
         ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value, write_date=NOW(); \
         INSERT INTO ir_config_parameter (key, value, create_date, write_date, create_uid, write_uid) \
         VALUES ('web.base.url.freeze','True',NOW(),NOW(),1,1) \
         ON CONFLICT (key) DO UPDATE SET value='True', write_date=NOW();",
        url = https_url,
    );

    let out = Command::new("psql")
        .args(["-h", &pg_ip, "-U", "odoo", "-d", db_name, "-v", "ON_ERROR_STOP=1", "-c", &sql])
        .env("PGPASSWORD", "odoo")
        .env("PGCONNECT_TIMEOUT", "5")
        .output()
        .map_err(|e| format!("psql spawn (upsert): {}", e))?;
    if !out.status.success() {
        return Err(format!("psql upsert: {}",
            String::from_utf8_lossy(&out.stderr).chars().take(500).collect::<String>()));
    }
    Ok(())
}

pub fn handle_delete_dns(nkr_name: &str, delete_cert: bool) -> IpcResponse {
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }

    let vhost_path = format!("{}/{}", NGINX_SITES_AVAILABLE, nkr_name);
    let link_path = format!("{}/{}", NGINX_SITES_ENABLED, nkr_name);

    // Leer el dns del vhost antes de borrarlo (para certbot delete).
    let dns_maybe = std::fs::read_to_string(&vhost_path)
        .ok()
        .and_then(|content| {
            content.lines()
                .find(|l| l.trim_start().starts_with("server_name "))
                .and_then(|l| l.split_whitespace().nth(1).map(|s| s.trim_end_matches(';').to_string()))
        });

    let _ = std::fs::remove_file(&link_path);
    let _ = std::fs::remove_file(&vhost_path);

    let _ = Command::new("systemctl").args(["reload", "nginx"]).status();

    if delete_cert {
        if let Some(dns) = &dns_maybe {
            // Re-validate the dns read from the vhost before passing it to
            // certbot. Although the initial validation already blocks malformed
            // dns inputs, the vhost file lives on disk and could be modified
            // outside this path; treat it as untrusted input.
            if is_safe_dns(dns) {
                let _ = Command::new("certbot")
                    .args(["delete", "--non-interactive", "--cert-name", dns])
                    .output();
            } else {
                eprintln!("[API-DNS] skipping certbot delete: invalid dns '{}'", dns);
            }
        }
    }

    IpcResponse::json(200, serde_json::json!({
        "deleted": true,
        "nkr_name": nkr_name,
        "dns": dns_maybe,
        "cert_deleted": delete_cert,
    }))
}

fn tail_str(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// =============================================================================
// InitDb — llama a Odoo /web/database/create en el guest
// =============================================================================

pub fn handle_init_db(
    nkr_name: &str,
    db_name_override: Option<&str>,
    admin_login: &str,
    admin_password: &str,
    demo: bool,
    lang: Option<&str>,
    country_code: Option<&str>,
    phone: Option<&str>,
) -> IpcResponse {
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }
    if admin_login.is_empty() || admin_login.len() > 128
        || admin_login.bytes().any(|b| matches!(b, b'\n' | b'\r' | 0)) {
        return IpcResponse::error(400, "invalid_admin_login", None);
    }
    if admin_password.len() < 4 || admin_password.len() > 512
        || admin_password.bytes().any(|b| matches!(b, b'\n' | b'\r' | 0)) {
        return IpcResponse::error(400, "invalid_admin_password", None);
    }

    let info = match get_instance_info(nkr_name) {
        Ok(i) => i,
        Err(_) => return IpcResponse::error(404, "instance_not_found", None),
    };
    if !info.nkr_status.running {
        return IpcResponse::error(409, "instance_not_running",
            Some("call POST /actions {\"action\":\"start\"} first"));
    }
    if !info.nkr_status.port_8069_up {
        // 503 + Retry-After semántico: el panel sabe que es transitorio y
        // puede reintentar sin marcar la creación como failed.
        return IpcResponse::json(503, serde_json::json!({
            "error": "odoo_not_ready_yet",
            "message": "VM running pero Odoo :8069 aún no responde. Reintentá en 5s.",
            "retry_after_s": 5,
        }));
    }

    let db_name = db_name_override.map(|s| s.to_string())
        .unwrap_or_else(|| info.db_name.clone());
    if !is_safe_identifier(&db_name) {
        return IpcResponse::error(400, "invalid_db_name", None);
    }

    // Idempotencia 1: DB ya existe → 200.
    if info.nkr_status.db_present {
        return IpcResponse::json(200, serde_json::json!({
            "nkr_name": nkr_name,
            "db_name": db_name,
            "status": "already_present",
            "message": "DB ya existe — no-op idempotente",
        }));
    }

    // Idempotencia 2: hay un job en curso → 202 con el estado actual.
    let status_path = format!("{}/.nkr-init-db.json", info.instance_dir);
    if let Ok(existing) = std::fs::read_to_string(&status_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&existing) {
            if v.get("status").and_then(|s| s.as_str()) == Some("running") {
                return IpcResponse::json(202, serde_json::json!({
                    "nkr_name": nkr_name,
                    "db_name": db_name,
                    "status": "running",
                    "message": "init-db ya en curso; poll GET /instances/{name} → nkr_status.init_db",
                    "job": v,
                }));
            }
        }
    }

    let master_pwd = read_admin_passwd_from_conf(&info.config_path)
        .unwrap_or_else(|| "admin".to_string());

    let form = build_form(&[
        ("master_pwd", &master_pwd),
        ("name", &db_name),
        ("login", admin_login),
        ("password", admin_password),
        ("phone", phone.unwrap_or("")),
        ("lang", lang.unwrap_or("en_US")),
        ("country_code", country_code.unwrap_or("")),
        ("demo", if demo { "true" } else { "false" }),
    ]);

    // Persist status "running" ANTES de spawn para que un poll inmediato del
    // panel vea el job en curso (evita race "202 recibido pero init_db=null").
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let _ = std::fs::write(&status_path, serde_json::json!({
        "status": "running",
        "db_name": db_name,
        "admin_login": admin_login,
        "started_at": started_at,
    }).to_string());

    // Spawn background worker. El daemon retorna 202 al toque, el panel hace
    // poll de GET /instances/{name} y espera a db_present=true o init_db.status=failed.
    let nkr_name_owned = nkr_name.to_string();
    let db_name_owned = db_name.clone();
    let admin_login_owned = admin_login.to_string();
    let guest_ip = info.guest_ip.clone();
    let status_path_owned = status_path.clone();
    // Auto-seal de web.base.url (recomendación #4 del panel): si ya hay vhost
    // provisionado para este tenant, tras el CREATE DATABASE exitoso sellamos
    // `web.base.url=https://<dns>` + freeze. Esto cierra la ventana donde
    // Odoo 19 auto-asignaba el host del primer request (http://...) y rompía
    // el form de login por mixed-content. Sin esto el panel debía recordar
    // re-llamar /dns post-init-db.
    let cell_for_seal = info.cell.clone();
    let dns_for_seal: Option<String> = {
        let vhost_path = format!("/etc/nginx/sites-available/{}", nkr_name_owned);
        if std::path::Path::new(&vhost_path).exists() {
            info.dns.clone()
        } else {
            None
        }
    };
    eprintln!("[API] init-db({}) arrancando job background (db={})",
        nkr_name_owned, db_name_owned);

    std::thread::spawn(move || {
        let call_start = std::time::Instant::now();
        let result = http_post_form(
            &guest_ip, 8069, "/web/database/create", &form, 600,
        );
        let elapsed_ms = call_start.elapsed().as_millis() as u64;
        let finished_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

        let status_val = match result {
            Ok((code, _body_snippet)) if (200..400).contains(&code) => {
                eprintln!("[API] init-db({}) OK en {}ms (code={})",
                    nkr_name_owned, elapsed_ms, code);
                // Auto-seal web.base.url si hay DNS provisionado.
                auto_seal_base_url(&nkr_name_owned, &cell_for_seal,
                    &db_name_owned, dns_for_seal.as_deref());
                serde_json::json!({
                    "status": "success",
                    "db_name": db_name_owned,
                    "admin_login": admin_login_owned,
                    "started_at": started_at,
                    "finished_at": finished_at,
                    "elapsed_ms": elapsed_ms,
                    "odoo_response_code": code,
                })
            }
            Ok((code, body_snippet)) => {
                let lowered = body_snippet.to_lowercase();
                if lowered.contains("already exists") || lowered.contains("database already") {
                    eprintln!("[API] init-db({}) ya existía (idempotente)", nkr_name_owned);
                    // Idempotente: la DB ya estaba, igual sellamos por si el
                    // vhost se creó después.
                    auto_seal_base_url(&nkr_name_owned, &cell_for_seal,
                        &db_name_owned, dns_for_seal.as_deref());
                    serde_json::json!({
                        "status": "success",
                        "db_name": db_name_owned,
                        "note": "db_already_exists",
                        "started_at": started_at,
                        "finished_at": finished_at,
                        "elapsed_ms": elapsed_ms,
                    })
                } else {
                    eprintln!("[API] init-db({}) FAIL odoo_rejected code={} ({}ms)",
                        nkr_name_owned, code, elapsed_ms);
                    serde_json::json!({
                        "status": "failed",
                        "error": "odoo_rejected",
                        "odoo_response_code": code,
                        "body_snippet": body_snippet.chars().take(500).collect::<String>(),
                        "started_at": started_at,
                        "finished_at": finished_at,
                        "elapsed_ms": elapsed_ms,
                    })
                }
            }
            Err(e) => {
                eprintln!("[API] init-db({}) FAIL odoo_unreachable: {} ({}ms)",
                    nkr_name_owned, e, elapsed_ms);
                serde_json::json!({
                    "status": "failed",
                    "error": "odoo_unreachable",
                    "message": e,
                    "started_at": started_at,
                    "finished_at": finished_at,
                    "elapsed_ms": elapsed_ms,
                })
            }
        };
        let _ = std::fs::write(&status_path_owned, status_val.to_string());
    });

    // 202 inmediato. El panel espera db_present=true via GET /instances/{name}.
    IpcResponse::json(202, serde_json::json!({
        "nkr_name": nkr_name,
        "db_name": db_name,
        "admin_login": admin_login,
        "status": "accepted",
        "message": "init-db corriendo en background (30-90s típico). Poll GET /instances/{name} y espera nkr_status.db_present=true. Errores aparecen en nkr_status.init_db.",
    }))
}

fn read_admin_passwd_from_conf(conf_path: &str) -> Option<String> {
    let content = std::fs::read_to_string(conf_path).ok()?;
    for line in content.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("admin_passwd") {
            let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=').trim();
            if !rest.is_empty() && rest != "admin" {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// URL-encode form body.
fn build_form(pairs: &[(&str, &str)]) -> String {
    fn pct(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char);
                }
                _ => out.push_str(&format!("%{:02X}", b)),
            }
        }
        out
    }
    let mut s = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 { s.push('&'); }
        s.push_str(&pct(k));
        s.push('=');
        s.push_str(&pct(v));
    }
    s
}

/// Mini-HTTP-client bloqueante. Usa TcpStream plano (guest es HTTP, no HTTPS).
/// Sigue redirects 303 (típica respuesta de Odoo tras create OK).
/// Retorna (status_code, primera parte del body como String).
fn http_post_form(host: &str, port: u16, path: &str, body: &str, timeout_secs: u64)
    -> Result<(u16, String), String>
{
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
    use std::time::Duration;

    let addr: SocketAddr = format!("{}:{}", host, port).to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next().ok_or_else(|| "no address resolved".to_string())?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| format!("connect: {}", e))?;
    stream.set_read_timeout(Some(Duration::from_secs(timeout_secs))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(timeout_secs))).ok();

    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: nkr-daemon/1.5\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        path, host, port, body.len(), body
    );
    stream.write_all(req.as_bytes()).map_err(|e| format!("write: {}", e))?;

    let mut resp = Vec::with_capacity(4096);
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                resp.extend_from_slice(&buf[..n]);
                if resp.len() > 256 * 1024 { break; } // cap a 256 KiB
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&resp).to_string();
    // Parse status line: "HTTP/1.1 303 See Other\r\n..."
    let code: u16 = text.split_whitespace().nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // Body: everything after \r\n\r\n.
    let body_part = text.find("\r\n\r\n").map(|p| text[p+4..].to_string())
        .unwrap_or_default();
    Ok((code, body_part))
}

fn render_tenant_vhost(nkr_name: &str, dns: &str, guest_ip: &str, enable_ws: bool, tier: Tier) -> String {
    let http_upstream = format!("up_{}", nkr_name.replace('-', "_"));
    let ws_upstream = format!("{}_ws", http_upstream);
    let dev_like = tier.is_dev_like();

    // Puerto del WebSocket / longpolling según modo Odoo:
    //   - threaded (workers=0, DEV/STAG): NO existe gevent separado en :8072.
    //     El mismo werkzeug del :8069 maneja WebSocket upgrades. Apuntar
    //     nginx a :8072 daría connection refused → cliente ve "se perdió la
    //     conexión en tiempo real" (chat, bus, longpolling muertos).
    //   - prefork (workers>0, PROD): gevent corre como proceso separado en
    //     :8072 para longpolling/WebSocket — sin él, el master werkzeug se
    //     bloquearía cada longpoll y mataría la concurrencia.
    let ws_port = if dev_like { 8069 } else { 8072 };

    let mut out = String::new();
    out.push_str(&format!(
"# Auto-generated by NKR for tenant '{nkr_name}'. DO NOT EDIT MANUALLY.
# Regenerate via POST /api/v1/cells/<cell>/instances/{nkr_name}/dns.

upstream {http_upstream} {{ server {guest_ip}:8069; keepalive 16; }}
"));
    if enable_ws {
        out.push_str(&format!(
"upstream {ws_upstream} {{ server {guest_ip}:{ws_port}; keepalive 16; }}
"));
    }
    out.push_str(&format!(
"
server {{
    listen 80;
    listen [::]:80;
    server_name {dns};
    location /.well-known/acme-challenge/ {{ root /var/www/html; }}
    location / {{ return 301 https://$host$request_uri; }}
}}

server {{
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name {dns};

    ssl_certificate     /etc/letsencrypt/live/{dns}/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/{dns}/privkey.pem;
    include /etc/nginx/snippets/nkr-ssl.conf;
    # Hardening universal: bloquea .php/.env/.git/.zip + paths CMS (444 close).
    include /etc/nginx/snippets/nkr-hardening.conf;
    # Edge dual: real-IP rewriting cuando CF está proxied. En modo direct
    # (DNS-only) este snippet es no-op porque ningún IP del cliente cae en
    # los rangos CF. Switch entre modos se hace en CF (panel flipea
    # `proxied: true|false`), NKR no requiere cambios. Ver NKR_API.md §7.1.
    include /etc/nginx/snippets/nkr-real-ip.conf;

    client_max_body_size 200M;
    proxy_read_timeout 720s;
    proxy_send_timeout 720s;
    proxy_buffering off;
    proxy_set_header Host $host;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;

"));
    // Rate limit /web/login: SOLO en tier=production. Para dev/staging, el
    // operador hace muchos logins probando código y el throttle confunde.
    if !dev_like {
        out.push_str(&format!(
"    # Rate limit anti brute-force sobre login + database manager.
    # Burst 5 nodelay: 5 reqs simultáneos pasan; 6to en adelante 429.
    # $binary_remote_addr funciona en AMBOS modos de edge (direct/CF proxied)
    # gracias al snippet nkr-real-ip.conf incluido arriba.
    location ~ ^/web/(login|database/selector|database/manager) {{
        limit_req zone=nkr_login_limit burst=5 nodelay;
        limit_req_status 429;
        proxy_pass http://{http_upstream};
    }}

"));
    } else {
        out.push_str(&format!(
"    # tier={tier:?}: sin rate-limit en /web/login (iteración dev sin throttle).
    location ~ ^/web/(login|database/selector|database/manager) {{
        proxy_pass http://{http_upstream};
    }}

"));
    }
    if enable_ws {
        out.push_str(&format!(
"    # WebSocket / long-polling → :{ws_port}
    #   - DEV/STAG (threaded, workers=0): mismo werkzeug del :8069.
    #   - PROD (prefork, workers>0): gevent separado en :8072.
    location /websocket {{
        proxy_pass http://{ws_upstream};
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection \"upgrade\";
    }}
    location /longpolling {{
        proxy_pass http://{ws_upstream};
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection \"upgrade\";
    }}

"));
    }
    // Cache nginx en /web/static/* y /web/assets/<hash>/*: SOLO en
    // tier=production. Para dev/staging, los assets recién compilados pueden
    // venir cacheados del request anterior y confunden ("toqué el archivo y
    // el browser sigue mostrando el viejo"). Mejor sin cache para iteración.
    if !dev_like {
        out.push_str(&format!(
"    # /web/static/* — assets de módulos (img, fonts, css de módulos).
    # Cache server-side 24h. proxy_buffering on local: el server padre
    # tiene buffering off (necesario para SSE/long-polling), pero cache
    # REQUIERE buffering on porque nginx necesita capturar la respuesta
    # entera antes de escribirla al disco.
    location /web/static/ {{
        proxy_pass http://{http_upstream};
        proxy_buffering on;
        proxy_cache nkr_static;
        proxy_cache_key \"$host$request_uri\";
        proxy_cache_valid 200 24h;
        proxy_cache_use_stale error timeout updating;
        proxy_cache_lock on;
        add_header X-Cache-Status $upstream_cache_status always;
        expires 24h;
    }}

    # /web/assets/<hash>/* — bundles compilados (frontend.min.css, web.assets.js).
    # El <hash> en URL invalida automáticamente cuando el contenido cambia, así
    # que cache long-term (30 días) es seguro. Cache-Control: immutable evita
    # que el browser re-valide. Crítico para densidad: el bundle del login pesa
    # ~850 KB y se sirve en cada cold-load.
    location ~ ^/web/assets/[a-f0-9]+/ {{
        proxy_pass http://{http_upstream};
        proxy_buffering on;
        proxy_cache nkr_static;
        proxy_cache_key \"$host$request_uri\";
        proxy_cache_valid 200 30d;
        proxy_cache_use_stale error timeout updating;
        proxy_cache_lock on;
        add_header X-Cache-Status $upstream_cache_status always;
        add_header Cache-Control \"public, immutable, max-age=2592000\" always;
    }}

"));
    } else {
        out.push_str(&format!(
"    # tier={tier:?}: cache nginx desactivado, no-cache headers para forzar
    # al browser a re-bajar assets en cada request (iteración dev limpia).
    location /web/static/ {{
        proxy_pass http://{http_upstream};
        add_header Cache-Control \"no-cache, no-store, must-revalidate\" always;
    }}
    location ~ ^/web/assets/[a-f0-9]+/ {{
        proxy_pass http://{http_upstream};
        add_header Cache-Control \"no-cache, no-store, must-revalidate\" always;
    }}

"));
    }
    out.push_str(&format!(
"    location / {{
        proxy_pass http://{http_upstream};
    }}
}}
"));
    out
}

// =============================================================================
// PATCH /instances/{name}/config — upsert workers/memory/SMTP
// =============================================================================

#[derive(Serialize, Deserialize, Debug)]
pub struct PatchConfigReq {
    /// Único input de sizing. Si se pasa, NKR re-deriva chrs, ram_mb (compose)
    /// y limit_memory_soft/hard (odoo.conf) vía `derive_resources()` y los
    /// reescribe atómicamente. Rango 1..=16.
    #[serde(default)]
    pub workers: Option<u32>,
    /// Override de chrs (CPU quota cgroup). Si se manda, NKR escribe ese
    /// valor en el compose y aplica al próximo restart. Si se omite junto a
    /// `workers`, mantiene el chrs actual del compose. Rango 1..=50.
    /// Útil para upgradear momentáneamente la CPU de un tenant en horarios
    /// pico sin tocar workers (ej. fin de mes contable).
    #[serde(default)]
    pub chrs: Option<u32>,

    // ─── SMTP saliente del tenant ───────────────────────────────────────────
    // El panel manda estos campos cuando quiere configurar el servidor de mail
    // saliente. NKR upsertea un registro `ir.mail_server` (name='NKR-managed')
    // en la DB del tenant — esto es lo que Odoo usa en runtime y muestra en
    // "Settings → Technical → Email → Outgoing Mail Servers". También escribe
    // los `smtp_*` en odoo.conf como fallback (Odoo cae a ellos si no hay
    // ningún ir.mail_server activo, raro pero defensivo).
    #[serde(default)]
    pub smtp_server: Option<String>,
    #[serde(default)]
    pub smtp_port: Option<u16>,
    #[serde(default)]
    pub smtp_user: Option<String>,
    #[serde(default)]
    pub smtp_password: Option<String>,
    /// "none" | "ssl" | "starttls". Mapea directo al field `smtp_encryption`
    /// del modelo Odoo. Si se omite y `smtp_ssl=true` legacy → "ssl";
    /// si `smtp_ssl=false` y port=587 → "starttls"; otherwise "none".
    #[serde(default)]
    pub smtp_encryption: Option<String>,
    /// Legacy: bool simple. Reemplazado por `smtp_encryption`. Si ambos vienen,
    /// `smtp_encryption` gana. Mantenido para compat backwards.
    #[serde(default)]
    pub smtp_ssl: Option<bool>,
    /// "From:" header default (cuando un mail no tiene from explícito). Suele
    /// ser igual a `smtp_user`.
    #[serde(default)]
    pub email_from: Option<String>,
    /// Si true, NKR reinicia la VM tras escribir el conf. Workers lo NECESITA;
    /// SMTP toma efecto al instante via ir.mail_server, restart innecesario.
    #[serde(default)]
    pub restart: Option<bool>,
}

/// Rechaza cualquier char que rompa el INI parser de Odoo o permita inyección
/// de líneas (newlines). Permite espacios y casi cualquier ASCII imprimible
/// pero no CR/LF/NUL.
fn is_safe_conf_value(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 512
        && !s.bytes().any(|b| matches!(b, 0 | b'\n' | b'\r'))
}

/// Host/server: más estricto (solo [A-Za-z0-9._-:]) — evita inyección en el
/// arranque de Odoo si alguien usa smtp_server en shell concat más adelante.
fn is_safe_smtp_host(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
}

pub fn handle_patch_config(nkr_name: &str, body_json: &str) -> IpcResponse {
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }
    if body_json.len() > 16 * 1024 {
        return IpcResponse::error(413, "body_too_large", None);
    }
    let req: PatchConfigReq = match serde_json::from_str(body_json) {
        Ok(r) => r,
        Err(_) => return IpcResponse::error(400, "invalid_json", None),
    };

    // workers es input de sizing; el resto se deriva. chrs es override opcional.
    if let Some(w) = req.workers {
        if !(1..=16).contains(&w) {
            return IpcResponse::error(400, "invalid_workers",
                Some("workers must be 1..=16"));
        }
    }
    if let Some(c) = req.chrs {
        if !(1..=50).contains(&c) {
            return IpcResponse::error(400, "invalid_chrs",
                Some("chrs must be 1..=50 (1 chr = 20% de un core)"));
        }
    }
    if let Some(ref host) = req.smtp_server {
        if !is_safe_smtp_host(host) {
            return IpcResponse::error(400, "invalid_smtp_server", None);
        }
    }
    if let Some(p) = req.smtp_port {
        if p == 0 {
            return IpcResponse::error(400, "invalid_smtp_port", None);
        }
    }
    if let Some(ref u) = req.smtp_user {
        if !is_safe_conf_value(u) || u.len() > 256 {
            return IpcResponse::error(400, "invalid_smtp_user", None);
        }
    }
    if let Some(ref p) = req.smtp_password {
        if !is_safe_conf_value(p) {
            return IpcResponse::error(400, "invalid_smtp_password", None);
        }
    }
    if let Some(ref e) = req.email_from {
        if !is_safe_conf_value(e) || e.len() > 256 {
            return IpcResponse::error(400, "invalid_email_from", None);
        }
    }
    if let Some(ref enc) = req.smtp_encryption {
        if !matches!(enc.as_str(), "none" | "ssl" | "starttls") {
            return IpcResponse::error(400, "invalid_smtp_encryption",
                Some("expected: none | ssl | starttls"));
        }
    }

    let info = match get_instance_info(nkr_name) {
        Ok(i) => i,
        Err(_) => return IpcResponse::error(404, "instance_not_found", None),
    };

    // Tenants dev/staging tienen perfil fijo (2GB + workers=1 + chrs=5).
    // Si el panel intenta overridear workers o chrs en una instancia dev-like,
    // 409: tienen que promover a production primero (cambiar tier en meta.json
    // — endpoint PATCH /tier no implementado todavía, requiere edit manual).
    if info.meta.tier.is_dev_like() && (req.workers.is_some() || req.chrs.is_some()) {
        return IpcResponse::json(409, serde_json::json!({
            "error": "sizing_locked_for_tier",
            "message": format!("tier={:?} tiene perfil fijo (workers=1, ram=2GB, chrs=5). \
                                Para overridear sizing, promover el tenant a production primero.",
                info.meta.tier),
            "tier": info.meta.tier,
            "locked_fields": ["workers", "chrs"],
            "remediation": "Editar meta.json a mano (tier=production) + restart, o esperar a que se exponga PATCH /tier.",
        }));
    }

    // Construir set de upserts. Solo entran los campos que el panel mandó.
    let mut upserts: Vec<(String, String)> = Vec::new();
    let mut applied: Vec<&str> = Vec::new();
    let mut restart_required = false;

    // workers → recomputa workers + limit_memory_soft + limit_memory_hard (odoo.conf)
    //         + ram + chrs (compose). Single input, multi-output coherente.
    let workers_resources: Option<DerivedResources> = req.workers.map(derive_resources);
    if let Some(ref r) = workers_resources {
        upserts.push(("workers".into(), r.workers.to_string()));
        upserts.push(("limit_memory_soft".into(), r.limit_memory_soft.to_string()));
        upserts.push(("limit_memory_hard".into(), r.limit_memory_hard.to_string()));
        applied.push("workers");
        applied.push("limit_memory_soft");
        applied.push("limit_memory_hard");
        applied.push("ram_mb");
        applied.push("chrs");
        restart_required = true;
    }

    // SMTP: detectar si el panel mandó alguno de los campos. Si sí, hacemos
    // dos cosas:
    //   (a) Escribir keys en odoo.conf (fallback que Odoo usa SOLO si no hay
    //       ningún ir.mail_server activo — caso edge en boots fríos).
    //   (b) UPSERT en ir.mail_server (lo que la UI de Odoo muestra y lo que
    //       Odoo realmente usa en runtime).
    let any_smtp = req.smtp_server.is_some() || req.smtp_port.is_some()
        || req.smtp_user.is_some() || req.smtp_password.is_some()
        || req.smtp_encryption.is_some() || req.smtp_ssl.is_some()
        || req.email_from.is_some();

    if let Some(ref s) = req.smtp_server {
        upserts.push(("smtp_server".into(), s.clone()));
        applied.push("smtp_server");
    }
    if let Some(p) = req.smtp_port {
        upserts.push(("smtp_port".into(), p.to_string()));
        applied.push("smtp_port");
    }
    if let Some(ref u) = req.smtp_user {
        upserts.push(("smtp_user".into(), u.clone()));
        applied.push("smtp_user");
    }
    if let Some(ref p) = req.smtp_password {
        upserts.push(("smtp_password".into(), p.clone()));
        applied.push("smtp_password");
    }
    if let Some(ssl) = req.smtp_ssl {
        upserts.push(("smtp_ssl".into(), if ssl { "True".into() } else { "False".into() }));
        applied.push("smtp_ssl");
    }
    if let Some(ref e) = req.email_from {
        upserts.push(("email_from".into(), e.clone()));
        applied.push("email_from");
    }

    if upserts.is_empty() && workers_resources.is_none() {
        return IpcResponse::error(400, "no_fields",
            Some("at least one field must be provided (workers / smtp_*)"));
    }

    if !upserts.is_empty() {
        if let Err(e) = patch_odoo_conf(&info.config_path, &upserts) {
            eprintln!("[API] patch_config({}) failed: {}", nkr_name, e);
            return IpcResponse::error(500, "patch_failed", Some(&e.to_string()));
        }
    }
    if let Some(ref r) = workers_resources {
        // When workers changes we re-derive ALL the compose-level resources
        // that depend on it: ram, chrs (override del panel gana), balloon_mb.
        // Keeping balloon in sync with ram is critical — leaving the old
        // balloon when ram grows wastes density; leaving it when ram shrinks
        // can starve Odoo.
        let final_chrs = req.chrs.unwrap_or(r.chrs);
        if let Err(e) = crate::cell::patch_compose_block_resources(
            nkr_name, Some(r.ram_mb), Some(final_chrs), Some(r.balloon_mb))
        {
            eprintln!("[API] patch_config({}) compose failed: {}", nkr_name, e);
            return IpcResponse::error(500, "patch_compose_failed", Some(&e.to_string()));
        }
        restart_required = true;
    } else if let Some(c) = req.chrs {
        // chrs solo (sin workers): patch puntual del compose, no re-deriva
        // ram/balloon. Útil para boost de CPU sin reescribir todo el sizing.
        if let Err(e) = crate::cell::patch_compose_block_resources(
            nkr_name, None, Some(c), None)
        {
            eprintln!("[API] patch_config({}) compose chrs-only failed: {}", nkr_name, e);
            return IpcResponse::error(500, "patch_compose_failed", Some(&e.to_string()));
        }
        applied.push("chrs");
        restart_required = true;
    }

    // SMTP runtime: UPSERT en ir.mail_server. Esto es lo que la UI de Odoo
    // muestra y lo que el motor de mail.thread realmente usa al hacer SEND.
    let mut mail_server_status: &'static str = "not_requested";
    if any_smtp {
        // Resolver smtp_encryption efectivo:
        //   - Si vino explícito en req.smtp_encryption → lo usamos.
        //   - Si no, derivamos de smtp_ssl + smtp_port (heurística).
        //   - Si nada, "none".
        let encryption = match req.smtp_encryption.as_deref() {
            Some(v) => v.to_string(),
            None => match (req.smtp_ssl, req.smtp_port) {
                (Some(true), _) => "ssl".to_string(),
                (Some(false), Some(587)) => "starttls".to_string(),
                _ => "none".to_string(),
            },
        };
        match upsert_tenant_mail_server(
            &info.cell, &info.db_name,
            req.smtp_server.as_deref().unwrap_or(""),
            req.smtp_port.unwrap_or(0),
            req.smtp_user.as_deref().unwrap_or(""),
            req.smtp_password.as_deref().unwrap_or(""),
            &encryption,
            req.email_from.as_deref().unwrap_or(""),
        ) {
            Ok(()) => {
                applied.push("ir.mail_server");
                mail_server_status = "updated";
                eprintln!("[API] patch_config({}) ir.mail_server upsertado (encryption={})",
                    nkr_name, encryption);
            }
            Err(e) if e == "db_not_present" => {
                mail_server_status = "skipped_db_missing";
                eprintln!("[API] patch_config({}) ir.mail_server omitido: DB aún no existe", nkr_name);
            }
            Err(e) => {
                mail_server_status = "failed_nonblocking";
                eprintln!("[API] patch_config({}) ir.mail_server FAIL: {}", nkr_name, e);
            }
        }
        // SMTP via ir.mail_server NO requiere restart — Odoo lo lee fresh
        // cada vez que `mail.mail.send()` corre. Sólo si quedó SOLO en
        // odoo.conf (fallback) habría que restart.
    }

    // Restart opcional. Si el panel lo pide (default para workers/memory: sí
    // a menos que explícitamente mande restart=false).
    let want_restart = req.restart.unwrap_or(restart_required);
    let mut restarted = false;
    if want_restart {
        match handle_action(nkr_name, "restart") {
            r if r.status == 202 || r.status == 200 => { restarted = true; }
            r => {
                eprintln!("[API] patch_config({}): restart falló code={}", nkr_name, r.status);
            }
        }
    }

    IpcResponse::json(200, serde_json::json!({
        "nkr_name": nkr_name,
        "applied": applied,
        "restart_required": restart_required,
        "restarted": restarted,
        "mail_server_update": mail_server_status,
    }))
}

/// UPSERT del registro `ir.mail_server` con name='NKR-managed' en la DB del
/// tenant. Es lo que la UI de Odoo ("Settings → Technical → Email → Outgoing
/// Mail Servers") muestra y lo que el motor de mail usa al enviar.
///
/// Si la DB no existe → Err("db_not_present"). Resto de errores propagan stderr
/// truncado.
fn upsert_tenant_mail_server(
    cell_name: &str,
    db_name: &str,
    smtp_host: &str,
    smtp_port: u16,
    smtp_user: &str,
    smtp_pass: &str,
    smtp_encryption: &str, // "none" | "ssl" | "starttls"
    from_filter: &str,
) -> Result<(), String> {
    use crate::cell::lookup_cell_id;
    let cell_id = lookup_cell_id(cell_name)
        .ok_or_else(|| "cell_not_found".to_string())?;
    let pg_ip = format!("10.0.{}.2", cell_id);

    // Probe DB.
    let chk = Command::new("psql")
        .args(["-h", &pg_ip, "-U", "odoo", "-d", "postgres", "-tAc",
               &format!("SELECT 1 FROM pg_database WHERE datname='{}';", db_name)])
        .env("PGPASSWORD", "odoo").env("PGCONNECT_TIMEOUT", "5")
        .output().map_err(|e| format!("psql probe spawn: {}", e))?;
    if !chk.status.success() {
        return Err(format!("psql probe: {}", String::from_utf8_lossy(&chk.stderr)));
    }
    if String::from_utf8_lossy(&chk.stdout).trim() != "1" {
        return Err("db_not_present".into());
    }

    // Escape simple: las comillas simples se duplican (ANSI SQL). El charset
    // ya pasó is_safe_conf_value (no \n / \r / NUL).
    let esc = |s: &str| s.replace('\'', "''");
    let host_e = esc(smtp_host);
    let user_e = esc(smtp_user);
    let pass_e = esc(smtp_pass);
    let enc_e = esc(smtp_encryption);
    let from_e = esc(from_filter);

    // UPSERT por name único 'NKR-managed'. Si Odoo no tiene la columna
    // smtp_authentication (versión antigua), el INSERT cae con error claro
    // (la sentencia es atómica via ON_ERROR_STOP=1).
    //
    // Uso DELETE+INSERT en lugar de ON CONFLICT porque `name` no tiene unique
    // index en ir.mail_server. Sí hay potencial race con varios PATCH paralelos
    // pero el endpoint corre con `restart` que ya serializa naturalmente.
    let sql = format!(
        "DELETE FROM ir_mail_server WHERE name='NKR-managed';
         INSERT INTO ir_mail_server
            (name, sequence, smtp_host, smtp_port, smtp_authentication,
             smtp_user, smtp_pass, smtp_encryption, from_filter, active,
             create_uid, write_uid, create_date, write_date)
         VALUES
            ('NKR-managed', 10, '{host}', {port}, 'login',
             NULLIF('{user}',''), NULLIF('{pass}',''), '{enc}',
             NULLIF('{from}',''), TRUE,
             1, 1, NOW(), NOW());",
        host = host_e, port = smtp_port,
        user = user_e, pass = pass_e, enc = enc_e, from = from_e,
    );

    let out = Command::new("psql")
        .args(["-h", &pg_ip, "-U", "odoo", "-d", db_name, "-v", "ON_ERROR_STOP=1", "-c", &sql])
        .env("PGPASSWORD", "odoo").env("PGCONNECT_TIMEOUT", "5")
        .output().map_err(|e| format!("psql upsert spawn: {}", e))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).chars().take(500).collect::<String>());
    }
    Ok(())
}

// =============================================================================
// POST /instances/{name}/psql — sandboxed psql vs tenant DB
// =============================================================================

/// Rechaza queries peligrosas para el scope del endpoint. No es un parser SQL;
/// sólo un filtro grueso para bloquear los casos obvios de escape/cross-tenant.
fn reject_psql_query(q: &str) -> Option<&'static str> {
    // Límite duro al body.
    if q.is_empty() { return Some("empty_query"); }
    if q.len() > 16 * 1024 { return Some("query_too_large"); }
    // Nul bytes → shell/exec injection.
    if q.bytes().any(|b| b == 0) { return Some("null_byte"); }
    let lower = q.to_ascii_lowercase();
    // \c / \connect meta-commands de psql → cambian de DB.
    if lower.contains("\\c ") || lower.contains("\\connect") {
        return Some("meta_connect_forbidden");
    }
    // \! ejecuta shell dentro de psql.
    if lower.contains("\\!") {
        return Some("shell_escape_forbidden");
    }
    // COPY ... TO/FROM PROGRAM — ejecución arbitraria.
    if lower.contains("copy") && lower.contains("program") {
        return Some("copy_program_forbidden");
    }
    // DROP/CREATE DATABASE — el panel tiene /init-db para create; nunca
    // debería crear/borrar DBs via este endpoint.
    if lower.contains("drop database") || lower.contains("create database") {
        return Some("database_ddl_forbidden");
    }
    None
}

fn audit_psql(nkr_name: &str, db_name: &str, query: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0);
    let truncated: String = query.chars().take(1024).collect();
    let line = format!("{t}\t{nkr_name}\t{db_name}\t{}\n",
        truncated.replace('\n', " ").replace('\t', " "));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true).append(true)
        .mode(0o640)
        .open("/var/log/nkr-psql-audit.log")
    {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn handle_psql(nkr_name: &str, query: &str, max_rows: usize) -> IpcResponse {
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }
    if let Some(reason) = reject_psql_query(query) {
        return IpcResponse::error(400, reason, None);
    }
    // Cap de filas: default 1000, max 10000.
    let max_rows = max_rows.clamp(1, 10_000);

    let info = match get_instance_info(nkr_name) {
        Ok(i) => i,
        Err(_) => return IpcResponse::error(404, "instance_not_found", None),
    };

    let cell_id = match lookup_cell_id(&info.cell) {
        Some(c) => c,
        None => return IpcResponse::error(404, "cell_not_found", None),
    };
    let pg_ip = format!("10.0.{}.2", cell_id);

    audit_psql(nkr_name, &info.db_name, query);

    // Ejecutar psql con:
    //   -h <pg_ip> -U odoo -d db-<tenant>
    //   --csv   → CSV parseable
    //   -P pager=off
    //   ON_ERROR_STOP=1
    //   statement_timeout=30s vía -v y SET
    //
    // La query se pasa por stdin para evitar límites de argv y inyección
    // en la línea de comandos.
    let mut cmd = Command::new("psql");
    cmd.arg("-h").arg(&pg_ip)
       .arg("-U").arg("odoo")
       .arg("-d").arg(&info.db_name)
       .arg("--csv")
       .arg("-P").arg("pager=off")
       .arg("-v").arg("ON_ERROR_STOP=1")
       .arg("-X"); // no leer ~/.psqlrc
    cmd.env("PGPASSWORD", "odoo"); // credencial interna del cluster
    cmd.env("PGCONNECT_TIMEOUT", "5");
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return IpcResponse::error(500, "psql_spawn_failed",
            Some(&e.to_string())),
    };

    // Prepender timeouts y setear search_path a public para evitar sorpresas.
    let full_query = format!(
        "SET statement_timeout = '30s';\nSET idle_in_transaction_session_timeout = '30s';\n{}\n",
        query
    );
    use std::io::Write;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(full_query.as_bytes());
    }

    // Timeout del proceso total: 35s (5s grace sobre el statement_timeout).
    let start = std::time::Instant::now();
    let deadline = std::time::Duration::from_secs(35);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return IpcResponse::error(504, "psql_timeout", None);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return IpcResponse::error(500, "psql_wait_failed",
                Some(&e.to_string())),
        }
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => return IpcResponse::error(500, "psql_output_failed",
            Some(&e.to_string())),
    };

    let stdout_s = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_s = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);

    // Truncar por filas. La primera línea es header.
    let mut lines: Vec<&str> = stdout_s.lines().collect();
    let total_rows = lines.len().saturating_sub(1);
    let truncated = total_rows > max_rows;
    if truncated {
        lines.truncate(max_rows + 1); // +1 header
    }
    let body_out = lines.join("\n");

    // Cap absoluto defensivo: 4 MiB.
    let body_out = if body_out.len() > 4 * 1024 * 1024 {
        body_out.chars().take(4 * 1024 * 1024).collect()
    } else {
        body_out
    };

    if code == 0 {
        IpcResponse::json(200, serde_json::json!({
            "nkr_name": nkr_name,
            "db_name": info.db_name,
            "exit_code": 0,
            "rows_returned": total_rows.min(max_rows),
            "truncated": truncated,
            "csv": body_out,
        }))
    } else {
        IpcResponse::json(400, serde_json::json!({
            "error": "psql_error",
            "nkr_name": nkr_name,
            "db_name": info.db_name,
            "exit_code": code,
            "stderr": stderr_s.chars().take(4096).collect::<String>(),
        }))
    }
}

// =============================================================================
// POST /admin/cache/purge — vaciar cache nginx (global, todos los tenants)
// =============================================================================

const NGINX_CACHE_DIR: &str = "/var/cache/nginx/nkr_static";

/// Borra todas las entries del cache nginx en disco. Reconstrucción orgánica:
/// la próxima request a un asset cacheado va a Odoo y vuelve a poblar la entry.
/// Aplicación típica: tras `POST /addons/git` que toca `/web/static/*` (logos,
/// imgs, fonts) — sin purgar, el cache server-side serviría stale por hasta 24h.
pub fn handle_purge_cache() -> IpcResponse {
    let dir = std::path::Path::new(NGINX_CACHE_DIR);
    if !dir.exists() {
        return IpcResponse::json(200, serde_json::json!({
            "purged": 0,
            "size_bytes_freed": 0,
            "note": "cache dir no existe — nada para purgar",
        }));
    }

    // Walk: cada entry hijo del cache dir es un subdir hash o archivo.
    // rm -rf todo el contenido (preservar el dir top porque nginx lo necesita).
    let mut purged = 0u64;
    let mut bytes_freed = 0u64;
    let mut errors: Vec<String> = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => return IpcResponse::error(500, "read_cache_dir_failed",
            Some(&e.to_string())),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let metadata_size = walk_size(&path);
        match if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        } {
            Ok(()) => {
                purged += 1;
                bytes_freed += metadata_size;
            }
            Err(e) => errors.push(format!("{}: {}", path.display(), e)),
        }
    }

    eprintln!("[API] cache purge: {} entries, {} bytes liberados", purged, bytes_freed);

    if !errors.is_empty() {
        return IpcResponse::json(207, serde_json::json!({
            "purged": purged,
            "size_bytes_freed": bytes_freed,
            "errors": errors,
        }));
    }
    IpcResponse::json(200, serde_json::json!({
        "purged": purged,
        "size_bytes_freed": bytes_freed,
    }))
}

/// SSO one-shot (Plan E): genera URL firmada HMAC para auto-login en el
/// tenant sin necesidad de conocer el password del usuario.
///
/// Flujo:
/// 1. Lee `nkr_sso_secret` del odoo.conf del tenant.
/// 2. Construye payload `<user>|<expires_at>` (TTL 30s).
/// 3. Firma con HMAC-SHA256 usando `nkr_sso_secret`.
/// 4. Devuelve URL `https://<dns>/nkr-sso?u=<user>&exp=<ts>&sig=<hex>`.
///
/// El módulo `nkr_sso` del tenant valida la firma con el MISMO secret
/// (leído del odoo.conf vía `tools.config.get('nkr_sso_secret')`), busca
/// el user en `res.users` y crea sesión sudo — sin pedir password.
///
/// Por qué es seguro:
/// - El `nkr_sso_secret` (256 bits) vive sólo en odoo.conf del host.
/// - El password de los users del tenant **jamás entra al flujo** — NKR
///   no lo necesita ni lo conoce.
/// - HMAC garantiza que sólo quien tiene el secret puede emitir URLs.
/// - TTL 30s limita la ventana de uso.
/// - Funciona para CUALQUIER user del tenant (no sólo admin).
///
/// Compromiso de `nkr_sso_secret`: permite login arbitrario al tenant
/// hasta rotar. Rotación = editar odoo.conf + REL_OD (≤5s, el módulo
/// re-lee la conf al respawn).
pub fn handle_sso(nkr_name: &str, user: &str) -> IpcResponse {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }
    // Login válido: alfanum + _-.@ (acepta emails). Max 128.
    if user.is_empty() || user.len() > 128
        || user.bytes().any(|b| !(b.is_ascii_alphanumeric()
            || matches!(b, b'_' | b'-' | b'.' | b'@'))) {
        return IpcResponse::error(400, "invalid_user", None);
    }

    let info = match get_instance_info(nkr_name) {
        Ok(i) => i,
        Err(_) => return IpcResponse::error(404, "instance_not_found", None),
    };
    if !info.nkr_status.running {
        return IpcResponse::error(409, "not_running",
            Some("Tenant apagado — start primero"));
    }
    let dns = match info.dns.as_deref() {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => return IpcResponse::error(409, "no_dns_provisioned",
            Some("Tenant sin DNS. Llamar POST /dns primero.")),
    };

    // Lee la HMAC key del odoo.conf. Ubicación preferida: sección `[nkr_sso]`
    // clave `secret` (no genera el WARNING "unknown option" de Odoo). Legacy:
    // clave `nkr_sso_secret` en `[options]` (tenants no migrados). admin_passwd
    // ya no se usa. Debe matchear cómo lo lee el módulo Odoo `nkr_sso`.
    let conf = match std::fs::read_to_string(&info.config_path) {
        Ok(c) => c,
        Err(e) => return IpcResponse::error(500, "conf_read_failed",
            Some(&format!("{}: {}", info.config_path, e))),
    };
    let sso_secret = parse_conf_value(&conf, "secret")
        .or_else(|| parse_conf_value(&conf, "nkr_sso_secret"))
        .unwrap_or_default();
    if sso_secret.is_empty() {
        return IpcResponse::error(500, "sso_secret_missing",
            Some("Tenant sin HMAC key en odoo.conf ([nkr_sso] secret = ... , o \
                  legacy nkr_sso_secret en [options]). Aplicable sólo a tenants \
                  creados con NKR ≥1.6.3. Setear manualmente y recargar Odoo \
                  (REL_OD vía POST /reload)."));
    }

    // Payload firmado: <user>|<exp>. NKR no toca el guest — solo firma.
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0) + 30;
    let payload = format!("{}|{}", user, exp);

    type HmacSha256 = Hmac<Sha256>;
    let mut mac = match HmacSha256::new_from_slice(sso_secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return IpcResponse::error(500, "hmac_key_invalid", None),
    };
    mac.update(payload.as_bytes());
    let sig_bytes = mac.finalize().into_bytes();
    let sig_hex: String = sig_bytes.iter().map(|b| format!("{:02x}", b)).collect();

    let url = format!(
        "https://{}/nkr-sso?u={}&exp={}&sig={}",
        dns,
        url_encode(user),
        exp,
        sig_hex
    );

    eprintln!("[API] sso({}, user={}): URL emitida (TTL 30s, HMAC-only)",
        nkr_name, user);
    IpcResponse::json(200, serde_json::json!({
        "url": url,
        "user": user,
        "expires_in": 30,
        "nkr_name": nkr_name,
        "dns": dns,
    }))
}

fn parse_conf_value(conf: &str, key: &str) -> Option<String> {
    for line in conf.lines() {
        let t = line.trim_start();
        if t.starts_with('#') || t.starts_with(';') {
            continue;
        }
        if let Some(rest) = t.strip_prefix(key) {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let v = rest.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

#[allow(dead_code)]
fn extract_session_id(set_cookie: &str) -> Option<String> {
    // `Set-Cookie: session_id=<sid>; HttpOnly; Path=/; ...` — extraer <sid>.
    // Si vienen múltiples cookies (separadas por coma o newline), buscar
    // específicamente la session_id.
    for part in set_cookie.split([',', '\n']) {
        let t = part.trim_start();
        if let Some(rest) = t.strip_prefix("session_id=") {
            // hasta el primer ; o whitespace
            let end = rest.find(|c: char| c == ';' || c.is_whitespace())
                .unwrap_or(rest.len());
            let sid = &rest[..end];
            if !sid.is_empty() {
                return Some(sid.to_string());
            }
        }
    }
    None
}

/// Minimal percent-encoding para query string. Codifica solo lo que importa
/// para URLs (espacios, `&`, `=`, `+`, `?`, `#`). Resto se deja literal —
/// el session_id de Odoo es base64-url-safe igual.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.bytes() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'~' => out.push(c as char),
            _ => out.push_str(&format!("%{:02X}", c)),
        }
    }
    out
}

/// Diagnóstico HOST-side de un tenant: captura stacks de kernel + wchan +
/// CPU% + estado TCP por cada thread del proceso `nkr` de la VM.
///
/// Útil para diagnosticar cuelgues donde el HOST está en busy loop o un
/// virtio handler se quedó stuck. NO requiere shell al guest — toda la info
/// viene de `/proc/<pid>/{task,stat,...}` que el daemon (root) puede leer.
///
/// Output: text/plain con un dump multi-sección. Idempotente, ~50ms.
/// Lo invoca el operador (curl o `nkr diag`) cuando el watchdog detectó
/// `port_8069_up=false` o el panel ve latencia anómala.
///
/// Fase 1: solo HOST. Si el cuelgue es del guest (no del nkr), esta info
/// muestra `vcpu_thread blocked en kvm_run` que ya es una pista. Fase 2
/// agregará inyección de DIAG por hvc0 + script guest.
pub fn handle_diag(nkr_name: &str) -> IpcResponse {
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }
    let info = match get_instance_info(nkr_name) {
        Ok(i) => i,
        Err(_) => return IpcResponse::error(404, "instance_not_found", None),
    };
    if !info.nkr_status.running {
        return IpcResponse::error(409, "not_running",
            Some("Tenant apagado — no hay proceso vivo que diagnosticar"));
    }
    let pid = match info.nkr_status.pid {
        Some(p) if p > 0 => p,
        _ => return IpcResponse::error(409, "pid_unknown", None),
    };
    let mut out = String::new();
    use std::fmt::Write;
    let _ = writeln!(out, "=== nkr diag: {} (PID {}, cell {}) ===",
        nkr_name, pid, info.cell);
    let _ = writeln!(out, "captured_at: {}", chrono_like_now());
    let _ = writeln!(out, "guest_ip:    {}", info.guest_ip);
    let _ = writeln!(out, "port_8069_up: {}", info.nkr_status.port_8069_up);
    let _ = writeln!(out, "ram_mb (RSS): {:?}", info.nkr_status.ram_mb);
    let _ = writeln!(out, "uptime_s:    {:?}", info.nkr_status.uptime_s);
    let _ = writeln!(out);

    // /proc/<pid>/stat — overall state
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", pid)) {
        let _ = writeln!(out, "=== /proc/{}/stat ===", pid);
        let _ = writeln!(out, "{}", stat.trim());
        let _ = writeln!(out);
    }
    // /proc/<pid>/status — VmRSS, Threads, State, voluntary_ctxt_switches
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", pid)) {
        let _ = writeln!(out, "=== /proc/{}/status (resumen) ===", pid);
        for line in status.lines() {
            if line.starts_with("State:") || line.starts_with("Threads:")
                || line.starts_with("VmRSS:") || line.starts_with("VmSize:")
                || line.starts_with("voluntary") || line.starts_with("nonvoluntary") {
                let _ = writeln!(out, "{}", line);
            }
        }
        let _ = writeln!(out);
    }

    // Per-thread: stack, wchan, comm, state
    let task_dir = format!("/proc/{}/task", pid);
    let _ = writeln!(out, "=== threads (/proc/{}/task/) ===", pid);
    if let Ok(entries) = std::fs::read_dir(&task_dir) {
        let mut tids: Vec<String> = entries.flatten()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();
        tids.sort();
        for tid in tids {
            let base = format!("{}/{}", task_dir, tid);
            let comm = std::fs::read_to_string(format!("{}/comm", base))
                .unwrap_or_default().trim().to_string();
            let wchan = std::fs::read_to_string(format!("{}/wchan", base))
                .unwrap_or_default().trim().to_string();
            let state = std::fs::read_to_string(format!("{}/status", base))
                .ok()
                .and_then(|s| s.lines().find(|l| l.starts_with("State:"))
                    .map(|l| l.trim_start_matches("State:").trim().to_string()))
                .unwrap_or_default();
            let _ = writeln!(out, "--- task {} comm={} state={} wchan={}",
                tid, comm, state, wchan);
            // Stack: solo si el thread está en kernel space (no R userspace)
            // — para R, /proc/.../stack devuelve vacío. Aún así lo leemos
            // por completitud.
            if let Ok(stack) = std::fs::read_to_string(format!("{}/stack", base)) {
                let trimmed: String = stack.lines().take(20).collect::<Vec<_>>().join("\n");
                if !trimmed.is_empty() {
                    let _ = writeln!(out, "{}", trimmed);
                }
            }
        }
    }
    let _ = writeln!(out);

    // TCP del host hacia el guest
    let _ = writeln!(out, "=== ss -tn al guest_ip {} ===", info.guest_ip);
    if let Ok(o) = Command::new("ss").args(["-tn"]).output() {
        let s = String::from_utf8_lossy(&o.stdout);
        for line in s.lines() {
            if line.contains(&info.guest_ip) {
                let _ = writeln!(out, "{}", line);
            }
        }
    }
    let _ = writeln!(out);

    // dmesg tail (host) — para ver si hubo OOM-kill / hung tasks / etc
    let _ = writeln!(out, "=== dmesg tail (últimas 15 líneas del HOST) ===");
    if let Ok(o) = Command::new("dmesg").args(["-T", "--", "-time-format=iso"]).output() {
        let s = String::from_utf8_lossy(&o.stdout);
        let lines: Vec<&str> = s.lines().collect();
        for line in lines.iter().rev().take(15).rev() {
            let _ = writeln!(out, "{}", line);
        }
    }

    IpcResponse::text(200, "text/plain; charset=utf-8", out)
}

/// Timestamp legible para output `nkr diag`. Minimal — no necesitamos
/// precisión sub-segundo ni timezone.
fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0);
    format!("unix={} ({}s ago boot)", secs, uptime_secs())
}

fn uptime_secs() -> u64 {
    std::fs::read_to_string("/proc/uptime").ok()
        .and_then(|s| s.split('.').next().and_then(|n| n.parse().ok()))
        .unwrap_or(0)
}

/// Reload de workers Odoo SIN reiniciar la VM. Daemon manda SIGUSR1 al PID
/// del proceso de la VM (state.json), vmm.rs catch SIGUSR1 → setea flag
/// RELOAD_REQUESTED → vcpu loop la consume → inyecta "REL_OD\n" por hvc0 →
/// el watcher del initramfs recarga Odoo según el modo: workers>0 (prefork)
/// → SIGHUP al master (respawnea workers, master vivo); workers=0 (threaded)
/// → SIGTERM al proceso (el supervisor loop de nkr-start.sh lo relanza). En
/// ambos casos código fresh del disco, ~3s, sin downtime de la VM. Idempotente.
///
/// Caso de uso primario: post `addons/git`. virtio-fs+inotify NO propagan
/// eventos del host al guest (limitación FUSE), entonces `dev_mode=reload`
/// del Odoo no detecta cambios. Trigger explícito desde el host vía SIGUSR1
/// es la solución arquitectónicamente correcta.

/// Verifica que `pid` siga siendo un proceso `nkr run` (VMM). Cierra la
/// race donde el PID registrado en state file fue reusado por un proceso
/// completamente ajeno tras la muerte de la VM original. Si PID inválido
/// o cmdline no matchea, retorna false → el caller debe NO mandar la señal.
/// Lee /proc/<pid>/comm (no cmdline porque comm es el nombre del exec,
/// ~16 bytes max → match cheap y robusto).
/// Audit 2026-05-15.
fn pid_is_nkr_vmm(pid: u32) -> bool {
    let comm_path = format!("/proc/{}/comm", pid);
    match std::fs::read_to_string(&comm_path) {
        Ok(s) => s.trim() == "nkr",
        Err(_) => false, // PID muerto o /proc no accesible
    }
}

pub fn handle_reload_workers(nkr_name: &str) -> IpcResponse {
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }
    // Resolver PID desde state.json. Si no está corriendo → 409, no tiene
    // sentido reload de algo apagado (el panel debería START primero).
    let info = match get_instance_info(nkr_name) {
        Ok(i) => i,
        Err(_) => return IpcResponse::error(404, "instance_not_found", None),
    };
    if !info.nkr_status.running {
        return IpcResponse::error(409, "not_running",
            Some("Tenant está apagado — start primero"));
    }
    let pid = match info.nkr_status.pid {
        Some(p) if p > 0 => p,
        _ => return IpcResponse::error(409, "pid_unknown",
            Some("PID de la VM no disponible en state — VM en provisioning?")),
    };

    // SIGUSR1 al PID. El handler de vmm.rs setea RELOAD_REQUESTED, el vcpu
    // loop consume la flag e inyecta "REL_OD\n" por hvc0 al guest.
    // Guard pre-kill: leer /proc/<pid>/comm y verificar que sea "nkr". Si el
    // PID fue reusado por otro proceso (rare race entre state read y kill),
    // SIGUSR1 a un proceso ajeno = comportamiento default (terminate). Esta
    // verificación cierra el race (audit 2026-05-15).
    if !pid_is_nkr_vmm(pid) {
        eprintln!("[API] reload_workers({}): PID {} ya no es un nkr run — skip",
            nkr_name, pid);
        return IpcResponse::error(409, "pid_reused",
            Some("El PID registrado ya no corresponde al VMM (probablemente murió y otro proceso lo reusó). Pollee GET /instances/{name} para estado actual."));
    }
    let r = unsafe { libc::kill(pid as i32, libc::SIGUSR1) };
    if r != 0 {
        let err = std::io::Error::last_os_error();
        eprintln!("[API] reload_workers({}): kill SIGUSR1 PID {} falló: {}",
            nkr_name, pid, err);
        return IpcResponse::error(500, "signal_failed",
            Some(&format!("kill PID {}: {}", pid, err)));
    }
    eprintln!("[API] reload_workers({}): SIGUSR1 → PID {} OK", nkr_name, pid);
    // Avisar al watchdog: durante los próximos RELOAD_GRACE_SECS no contar
    // :8069 down como cuelgue real (un reload con muchos módulos custom
    // puede tener el puerto down 90-150s legítimamente).
    crate::watchdog::note_reload(nkr_name);
    // El guest (watcher hvc0 del initramfs) diferencia por modo Odoo:
    //   workers>0 (prefork)  → SIGHUP al master → respawnea workers (master vivo)
    //   workers=0 (threaded) → SIGTERM al proceso → supervisor loop lo relanza
    // En ambos casos: código fresh del disco, sin downtime de la VM, ~3s.
    IpcResponse::json(202, serde_json::json!({
        "nkr_name": nkr_name,
        "status": "accepted",
        "mechanism": "SIGUSR1 → vmm → hvc0 REL_OD → guest watcher (SIGKILL PID exacto si threaded ~5–8s, SIGHUP master PID exacto si prefork ~1–2s)",
        "estimated_seconds": 3,
        "note": "Odoo recarga con código fresh del disco. Sin downtime de la VM."
    }))
}

/// Marca la VM como ACTIVE en el ballooning dinámico. Sólo tiene efecto si
/// la VM se levantó con `balloon_idle_mb != balloon_mb` (típicamente DEV/
/// STAG por tier). Comportamiento (CLAUDE.md v2.2):
///   - SIGUSR2 al PID de la VM
///   - vmm setea BALLOON_ACTIVE_REQUESTED_TS = now() y el vcpu loop, en su
///     próximo iter (≤5s), aplica `set_target_mb(active_mb) + IRQ config_change`
///   - Tras `balloon_decay_secs` (default 600s) sin renovación, vmm aplica
///     IDLE automáticamente.
///
/// Si la VM tiene balloon estático (PROD = 0/0), la señal se entrega pero
/// el state machine ignora (BALLOON_ACTIVE_MB == 0 en vmm) — fire-and-forget,
/// idempotente. Devolvemos 202 igual: el panel no necesita conocer el tier.
pub fn handle_balloon_active(nkr_name: &str) -> IpcResponse {
    if !is_safe_identifier(nkr_name) {
        return IpcResponse::error(400, "invalid_nkr_name", None);
    }
    let info = match get_instance_info(nkr_name) {
        Ok(i) => i,
        Err(_) => return IpcResponse::error(404, "instance_not_found", None),
    };
    if !info.nkr_status.running {
        return IpcResponse::error(409, "not_running",
            Some("Tenant está apagado — start primero"));
    }
    let pid = match info.nkr_status.pid {
        Some(p) if p > 0 => p,
        _ => return IpcResponse::error(409, "pid_unknown",
            Some("PID de la VM no disponible en state")),
    };

    // Safety check: la VM debe haber sido lanzada con el daemon ≥1.6.2
    // (que registra el handler SIGUSR2). VMs lanzadas con daemons viejos
    // tratan SIGUSR2 con la disposición default = TERMINATE → mataríamos
    // al tenant.
    //
    // Heurística: leer la cmdline del proceso de la VM y verificar que
    // contiene `--balloon-idle-mb`. Si no, la VM corre con balloon
    // estático (PROD por tier, o instancia legacy pre-1.6.2) y mandar
    // SIGUSR2 sería peligroso o no haría nada. Devolvemos 202 igual
    // (idempotente desde el panel) pero marcamos `applied=false`.
    let cmdline_path = format!("/proc/{}/cmdline", pid);
    let cmdline = std::fs::read(&cmdline_path).unwrap_or_default();
    // /proc/<pid>/cmdline está NUL-separated; convertimos a string lossy.
    let cmdline_str = String::from_utf8_lossy(&cmdline);
    if !cmdline_str.contains("balloon-idle-mb") {
        eprintln!("[API] balloon_active({}): VM PID {} sin --balloon-idle-mb \
                   en cmdline (daemon viejo o tier=production sin balloon \
                   dinámico). No mando SIGUSR2 — riesgo de matar el tenant.",
            nkr_name, pid);
        return IpcResponse::json(202, serde_json::json!({
            "nkr_name": nkr_name,
            "status": "accepted",
            "applied": false,
            "reason": "vm_static_balloon_or_legacy",
            "note": "La VM no tiene ballooning dinámico configurado (tier=production o lanzada antes del upgrade del daemon). SIGUSR2 no enviado para evitar terminate por handler ausente. Restart la VM para activar ballooning dinámico si su tier (dev/staging) lo justifica.",
        }));
    }

    // Guard pre-kill (audit 2026-05-15): SIGUSR2 a un proceso ajeno con
    // disposición default = terminate. Si PID fue reusado, podríamos matar
    // un servicio random del host.
    if !pid_is_nkr_vmm(pid) {
        eprintln!("[API] balloon_active({}): PID {} ya no es un nkr run — skip",
            nkr_name, pid);
        return IpcResponse::error(409, "pid_reused",
            Some("El PID registrado ya no corresponde al VMM."));
    }
    let r = unsafe { libc::kill(pid as i32, libc::SIGUSR2) };
    if r != 0 {
        let err = std::io::Error::last_os_error();
        eprintln!("[API] balloon_active({}): kill SIGUSR2 PID {} falló: {}",
            nkr_name, pid, err);
        return IpcResponse::error(500, "signal_failed",
            Some(&format!("kill PID {}: {}", pid, err)));
    }
    eprintln!("[API] balloon_active({}): SIGUSR2 → PID {} OK", nkr_name, pid);
    IpcResponse::json(202, serde_json::json!({
        "nkr_name": nkr_name,
        "status": "accepted",
        "mechanism": "SIGUSR2 → vmm BALLOON_ACTIVE_TS=now → set_target_mb(active) + IRQ config_change",
        "note": "Renueva el TS active. Tras balloon_decay_secs sin nueva señal, decae a IDLE. Si la VM tiene balloon estático (tier=production), la señal se ignora silenciosamente.",
    }))
}

/// Estado del repo Odoo Enterprise descargado en una cell.
/// Lo usa el panel para chequear si puede aceptar `edition: "enterprise"`
/// al crear tenants — si no hay repo descargado, mejor rechazar antes de
/// que el tenant arranque con manifests faltantes.
pub fn handle_enterprise_status(cell: &str) -> IpcResponse {
    if !is_safe_identifier(cell) {
        return IpcResponse::error(400, "invalid_cell", None);
    }
    // Leer odoo_version desde cell.yml. Si no existe la cell o no tiene
    // odoo_version, devolver 404.
    let cell_yml = format!("/mnt/nkr/cells/{}/cell.yml", cell);
    let content = match std::fs::read_to_string(&cell_yml) {
        Ok(c) => c,
        Err(_) => return IpcResponse::error(404, "cell_not_found", None),
    };
    let odoo_version = content.lines()
        .find_map(|l| l.trim().strip_prefix("odoo_version:"))
        .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
        .unwrap_or_default();
    if odoo_version.is_empty() {
        return IpcResponse::error(404, "cell_has_no_odoo_version", None);
    }

    let ent_dir = format!("/mnt/nkr/enterprise/{}", odoo_version);
    let path = std::path::Path::new(&ent_dir);
    let mut available = false;
    let mut module_count: u64 = 0;
    let mut size_bytes: u64 = 0;
    let mut sha = String::new();

    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for e in entries.flatten() {
                let p = e.path();
                let name = match p.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                // Modules son subdirs con __manifest__.py. Filtrar dotfiles
                // y el `nkr-env` artifact que generamos en otro flujo.
                if name.starts_with('.') || name == "nkr-env" { continue; }
                if !p.is_dir() { continue; }
                if p.join("__manifest__.py").exists() {
                    module_count += 1;
                }
            }
        }
        // El repo está "available" si tiene al menos 1 manifest enterprise.
        available = module_count > 0;
        size_bytes = walk_size(path);

        // SHA del HEAD del repo si existe .git
        // safe.directory=<dir> scoped to this path (NOT `*`) so we don't
        // disable the CVE-2022-24765 protection globally.
        if let Ok(out) = std::process::Command::new("git")
            .args(["-c", &format!("safe.directory={}", ent_dir),
                   "-c", "core.hooksPath=/dev/null",
                   "-C", &ent_dir, "rev-parse", "HEAD"])
            .output()
        {
            if out.status.success() {
                sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
        }
    }

    IpcResponse::json(200, serde_json::json!({
        "cell": cell,
        "odoo_version": odoo_version,
        "available": available,
        "module_count": module_count,
        "path": ent_dir,
        "size_bytes": size_bytes,
        "sha": sha,
    }))
}

/// Suma recursiva de tamaños de archivos. Tolera errores (skipea archivos no
/// legibles). Usado solo para reportar bytes liberados — no es crítico.
fn walk_size(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(md) = std::fs::metadata(path) {
        if md.is_file() {
            return md.len();
        }
        if md.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for e in entries.flatten() {
                    total += walk_size(&e.path());
                }
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_resources_production_table() {
        // Tabla canónica CLAUDE.md v2.2 (production):
        //   VM_RAM = 256 + 256 + W*768
        //   Soft = W*640 MB, Hard = W*768 MB (per worker)
        //   Balloon: PROD siempre ACTIVE = 0 (sin transición IDLE/ACTIVE).
        //   Doctrine: "PROD evita latencia de desinflado en picos de tráfico".
        let r1 = derive_resources(1);
        assert_eq!(r1.ram_mb, 1280);
        assert_eq!(r1.limit_memory_soft, 640 * 1024 * 1024);
        assert_eq!(r1.limit_memory_hard, 768 * 1024 * 1024);
        assert_eq!(r1.balloon_mb, 0);
        assert_eq!(r1.balloon_idle_mb, 0);

        let r2 = derive_resources(2);
        assert_eq!(r2.ram_mb, 2048);  // 256 + 256 + 768*2
        assert_eq!(r2.limit_memory_soft, 1280 * 1024 * 1024);
        assert_eq!(r2.limit_memory_hard, 1536 * 1024 * 1024);
        assert_eq!(r2.balloon_mb, 0);

        let r4 = derive_resources(4);
        assert_eq!(r4.ram_mb, 3584);  // 256 + 256 + 768*4
        assert_eq!(r4.balloon_mb, 0);

        let r8 = derive_resources(8);
        assert_eq!(r8.ram_mb, 6656);  // 256 + 256 + 768*8
        assert_eq!(r8.balloon_mb, 0);  // Regla del Grifo además
    }

    #[test]
    fn derive_resources_dev_tier_fixed_profile() {
        // Tabla v1.6.2 (dev): 1300 MB RAM, workers=0, soft=800/hard=1000.
        // Subido de 768/400/512 tras observar `Server memory limit reached`
        // ciclando con Odoo 19 + 31 módulos custom — el soft de 400 MB era
        // inalcanzable bajo carga normal de DEV en threaded mode.
        // Balloon dinámico: ACTIVE=0 (boot, toda la RAM), IDLE=256 (squeeze
        // suave; deja 1044 al guest, suficiente para Odoo idle sin chocar
        // con el hard de 1000).
        let r = derive_resources_for_tier(99, Tier::Dev);  // workers ignorado
        assert_eq!(r.workers, 0);
        assert_eq!(r.ram_mb, 1300);
        assert_eq!(r.limit_memory_soft, 800 * 1024 * 1024);
        assert_eq!(r.limit_memory_hard, 1000 * 1024 * 1024);
        assert_eq!(r.balloon_mb, 0,
            "DEV debe arrancar ACTIVE (balloon=0 al boot) para evitar OOM");
        assert_eq!(r.balloon_idle_mb, 256);
        assert_ne!(r.balloon_mb, r.balloon_idle_mb,
            "DEV debe tener balloon dinámico (active != idle)");
    }

    #[test]
    fn derive_resources_staging_tier_fixed_profile() {
        // STAGING v1.6.5+: ram=1024, balloon suave igual que DEV (boot=0,
        // idle=256). El perfil viejo (boot=256, idle=768) ahogaba al guest
        // post-IDLE en picos de carga.
        let r = derive_resources_for_tier(99, Tier::Staging);
        assert_eq!(r.workers, 0);
        assert_eq!(r.ram_mb, 1024);
        assert_eq!(r.limit_memory_soft, 600 * 1024 * 1024);
        assert_eq!(r.limit_memory_hard, 700 * 1024 * 1024);
        assert_eq!(r.balloon_mb, 0);
        assert_eq!(r.balloon_idle_mb, 256);
    }

    #[test]
    fn validate_workers_ram_budget_rejects_insufficient_ram() {
        // 10 workers × 768 + 512 = 8192 MB requeridos
        let res = validate_workers_ram_budget(10, 2048);
        assert!(res.is_some());
        // RAM suficiente pasa
        assert!(validate_workers_ram_budget(2, 2048).is_none());
    }
}

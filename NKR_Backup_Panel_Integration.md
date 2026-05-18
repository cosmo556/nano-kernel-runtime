# NKR Backup — Guía de integración para el panel

**Audiencia:** equipo del panel (frontend + backend).
**Fecha:** 2026-05-18
**Versión NKR de referencia:** 1.6.9 + sprint backups.
**Scope:** cómo el panel pide un backup de un tenant y lo entrega para descarga al usuario final, **sin exponer el hostname de NKR**.

---

## TL;DR

1. **Usuario clic "Descargar backup"** en la UI del panel.
2. **Panel backend** llama a NKR vía 3 endpoints HTTP + Bearer token (esto sucede server-side, invisible al usuario).
3. **Panel re-stream** los bytes del ZIP al browser del usuario.
4. **El usuario ve solo URL del panel** (ej. `panel.systemouts.com/backups/<id>/download`). El hostname de NKR jamás aparece en el browser del usuario.

Formato: **`backup_odoo`** únicamente. Es un ZIP estándar Odoo (idéntico al que produce `/web/database/backup` nativo), restorable en cualquier Odoo del mundo vía `/web/database/restore`.

---

## 1. Arquitectura: Panel como proxy de streaming

```
┌──────────────┐                              ┌────────────────┐
│   Usuario    │                              │ NKR backend    │
│   (browser)  │                              │ (privado, IP   │
│              │                              │  interna)      │
└──────┬───────┘                              └────────┬───────┘
       │                                               │
       │ 1. clic "Descargar"                           │
       ▼                                               │
┌──────────────────────────────┐                       │
│   Panel backend              │                       │
│                              │                       │
│   - Verifica perms user      │                       │
│   - Llama NKR API ───────────┼──── 2. POST /backup ──┤
│       (Bearer NKR_TOKEN)     │                       │
│                              │◄─── 202 {job_id} ─────┤
│                              │                       │
│   - Polling cada 2s ─────────┼──── GET /status ──────┤
│                              │◄─── ready ────────────┤
│                              │                       │
│   - Streaming proxy ─────────┼──── GET /download ────┤
│       (resp.body.pipe(res))  │     (streaming bytes) │
│                              │◄═══════════════════════│
│                              │     stream chunks     │
│                              │                       │
│                              │                       │
└──────┬───────────────────────┘                       │
       │                                               │
       │ stream bytes del ZIP                          │
       ▼                                               │
┌──────────────┐                                       │
│   Usuario    │                                       │
│   descarga   │                                       │
│   ZIP        │                                       │
└──────────────┘                                       │
```

**Lo que el usuario ve:**
- URL del browser: `https://panel.systemouts.com/api/tenants/<id>/backup/download`
- Hostname/IP de NKR: **invisible** (la conexión NKR↔panel es server-to-server).

**Lo que el panel hace:**
- Server-side auth con su propio Bearer token NKR (NUNCA expuesto al cliente).
- Auth user-side con sus propias credenciales (sesión panel).
- Audit: log de "quién descargó qué tenant cuándo" en la DB del panel.
- Streaming: NO carga el archivo entero a RAM. Pipe directo del response NKR al response del browser.

---

## 2. Los 3 endpoints NKR que el panel consume

| Endpoint | Método | Propósito |
|---|---|---|
| `POST /api/v1/cells/{cell}/instances/{name}/backup` | POST | Iniciar backup (asíncrono, devuelve `job_id`) |
| `GET /api/v1/backups/{job_id}/status` | GET | Polling — devuelve `in_progress` / `ready` / `failed` |
| `GET /api/v1/backups/{job_id}/download` | GET | Streaming del ZIP final |

**Auth:** Bearer token NKR en `Authorization` header. Mismo token que el panel ya usa para los demás endpoints NKR.

**Detalles completos del contrato:** ver `NKR_API.md §4.20`.

---

## 3. Flujo paso a paso (pseudocódigo Node.js + Express)

### 3.1 Panel backend — endpoint que el frontend llama

```javascript
// PUT panel-backend/routes/backups.js
import fetch from 'node-fetch';

const NKR_API = process.env.NKR_API_URL;   // ej: http://10.0.0.1:9090
const NKR_TOKEN = process.env.NKR_API_TOKEN; // Bearer, server-side only

async function fetchNKR(path, opts = {}) {
    return fetch(`${NKR_API}${path}`, {
        ...opts,
        headers: {
            ...opts.headers,
            'Authorization': `Bearer ${NKR_TOKEN}`,
            ...(opts.body && { 'Content-Type': 'application/json' }),
        },
    });
}

// 1. Endpoint que dispara el backup (llamado por la UI cuando el user clic "Descargar")
router.post('/api/tenants/:tenantId/backup/start', requireAuth, async (req, res) => {
    const { tenantId } = req.params;
    const user = req.user;

    // Verificar que el user tiene permisos sobre este tenant en el panel
    const tenant = await db.tenants.findById(tenantId);
    if (!tenant || !user.canDownloadBackup(tenant)) {
        return res.status(403).json({ error: 'forbidden' });
    }

    // Llamar NKR
    const nkrResp = await fetchNKR(
        `/api/v1/cells/${tenant.cell}/instances/${tenant.nkr_name}/backup`,
        { method: 'POST', body: JSON.stringify({ format: 'odoo' }) }
    );
    if (!nkrResp.ok) {
        const err = await nkrResp.json();
        return res.status(nkrResp.status).json(err);
    }
    const { job_id } = await nkrResp.json();

    // Guardar el job en la DB del panel (para audit + retry + UI state)
    await db.backupJobs.create({
        id: job_id,
        tenant_id: tenantId,
        user_id: user.id,
        status: 'in_progress',
        created_at: new Date(),
    });

    // Devolver al frontend el job_id (NO el URL de NKR)
    res.json({ job_id, panel_status_url: `/api/tenants/${tenantId}/backup/${job_id}/status` });
});

// 2. Endpoint de polling (la UI consulta este, NO el de NKR directo)
router.get('/api/tenants/:tenantId/backup/:jobId/status', requireAuth, async (req, res) => {
    const { tenantId, jobId } = req.params;

    // Re-verify perms
    const tenant = await db.tenants.findById(tenantId);
    if (!req.user.canDownloadBackup(tenant)) {
        return res.status(403).json({ error: 'forbidden' });
    }

    // Forward a NKR
    const nkrResp = await fetchNKR(`/api/v1/backups/${jobId}/status`);
    const body = await nkrResp.json();

    // Update audit en panel DB
    await db.backupJobs.update(jobId, { status: body.status });

    // Filtrar info sensible antes de devolver al frontend
    res.json({
        status: body.status,
        size_bytes: body.size_bytes,
        filename: body.filename,
        // NO devolver job_id de NKR ni internal_paths
    });
});

// 3. Endpoint de descarga (la UI hace window.location = este URL)
router.get('/api/tenants/:tenantId/backup/:jobId/download', requireAuth, async (req, res) => {
    const { tenantId, jobId } = req.params;

    // Re-verify perms (importante — sino el user podría hijack URLs)
    const tenant = await db.tenants.findById(tenantId);
    if (!req.user.canDownloadBackup(tenant)) {
        return res.status(403).json({ error: 'forbidden' });
    }

    // Llamar NKR + stream proxy
    const nkrResp = await fetchNKR(`/api/v1/backups/${jobId}/download`);

    if (!nkrResp.ok) {
        const err = await nkrResp.json();
        return res.status(nkrResp.status).json(err);
    }

    // Reenviar headers de NKR al user
    res.setHeader('Content-Type', nkrResp.headers.get('content-type') || 'application/zip');
    res.setHeader('Content-Length', nkrResp.headers.get('content-length'));
    res.setHeader('Content-Disposition', nkrResp.headers.get('content-disposition'));
    // NO forwardear Authorization ni X-NKR-* internos

    // Audit final
    await db.backupJobs.update(jobId, { downloaded_at: new Date() });

    // Pipe directo (NO load to buffer)
    nkrResp.body.pipe(res);
});
```

### 3.2 Frontend (UI Vue/React/Angular)

```javascript
// async function en el componente "Tenant detail"
async function downloadBackup(tenantId) {
    // 1. Iniciar backup
    const startResp = await fetch(`/api/tenants/${tenantId}/backup/start`, {
        method: 'POST',
        credentials: 'include',
    });
    if (!startResp.ok) {
        showError("No se pudo iniciar el backup");
        return;
    }
    const { job_id } = await startResp.json();

    // 2. Polling con UI de progreso
    showSpinner("Generando backup...");
    let status;
    do {
        await new Promise(r => setTimeout(r, 2000));
        const sresp = await fetch(`/api/tenants/${tenantId}/backup/${job_id}/status`, {
            credentials: 'include',
        });
        status = await sresp.json();
        updateSpinnerMessage(`Estado: ${status.status}`);
    } while (status.status === 'in_progress');

    if (status.status !== 'ready') {
        showError(`Backup falló: ${status.error || 'unknown'}`);
        return;
    }

    // 3. Trigger descarga
    hideSpinner();
    // Method A — redirect:
    window.location = `/api/tenants/${tenantId}/backup/${job_id}/download`;
    // Method B — fetch + Blob (mejor UX, permite progress + cancel):
    /*
    const dlResp = await fetch(`/api/tenants/${tenantId}/backup/${job_id}/download`, {
        credentials: 'include',
    });
    const blob = await dlResp.blob();
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = status.filename;
    a.click();
    URL.revokeObjectURL(url);
    */
}
```

---

## 4. Decisiones de seguridad — qué hacer y qué NO

### ✅ Hacer

- **Server-side Bearer token de NKR**. El frontend NUNCA debe llamar NKR directamente.
- **Re-verify perms en CADA request** (start, status, download). Sin esto, un user con job_id ajeno podría descargar backup de otro tenant.
- **Audit log en la DB del panel** — quién bajó qué y cuándo. Crítico para forense.
- **Streaming proxy** — pipe directo del response NKR al response browser. Usar `response.body.pipe(res)` en Node, equivalent en otros lenguajes.
- **Validar `job_id` pertenece al tenant** — el panel guarda el mapping `job_id → tenant_id` en su DB y lo verifica en cada request.
- **Mostrar progreso al user** durante el polling — los backups de prod tardan 1–5 min, sin UI feedback el user va a recargar.

### ❌ NO hacer

- **NO exponer el endpoint NKR directamente al browser**. Aunque NKR requiera Bearer token, el hostname de NKR aparecería en DevTools y el token podría leakearse.
- **NO usar redirect 302 hacia NKR** (`res.redirect('http://nkr/...')`) — eso expone el hostname.
- **NO confiar en que el `job_id` recibido del frontend sea legítimo** — siempre cross-check contra la DB del panel.
- **NO cachear backups en el panel** sin TTL explícito — los backups tienen retention 24 h en NKR; cachear más allá puede dejar URLs muertas o entregar datos viejos.
- **NO incluir el Bearer token NKR en el `Cookie` o `localStorage`** del browser. Vive solo en variables de entorno del backend.

---

## 5. Manejo de errores y edge cases

| Error de NKR | Status | Significado | Qué debe hacer el panel |
|---|---|---|---|
| `404 instance_not_found` | 404 | El tenant fue borrado | Mostrar "tenant no existe" al user, refrescar lista |
| `409 action_in_progress` | 409 | Hay un delete/restart en curso en ese tenant | Mostrar "operación en curso, reintenta en X seg" |
| `400 invalid_format` | 400 | El panel mandó format != odoo (bug del panel) | Loggear bug; nunca debería pasar en panel-Claude |
| `503 spawn_failed` | 503 | NKR no pudo spawnear el thread del backup | Reintentar tras 5s, escalar al operador si persiste |
| `404 job_not_found` | 404 | El job fue cleanup (>24 h) o id inventado | Mostrar "backup expirado, generar uno nuevo" |
| `409 not_ready` (en /download) | 409 | El user navegó al download antes que status=ready | Poll status hasta ready, después reintentar download |
| `409 failed` | 409 | El backup falló al generarse | Mostrar el `error`/`message` al operador del panel |
| `500 file_disappeared` | 500 | Race entre el download y el cleanup 1 AM | Mostrar "backup expirado, generar uno nuevo" + alarma interna |

---

## 6. Performance esperada

### Tiempos baseline medidos (intech-devp: DB 77MB, filestore 108MB)

| Fase | Tiempo |
|---|---:|
| `POST /backup` → 202 | <200 ms |
| Polling (4× cada 2s) hasta `ready` | ~8 s |
| `GET /download` headers (streaming start) | <100 ms |
| Descarga real del ZIP (24 MB en LAN) | <2 s |
| **Total UX usuario (clic → descarga completa)** | **~10–12 s** |

### Proyección por tamaño de tenant

| Tipo tenant | DB | Backup time | Download time (LAN) | Total UX |
|---|---:|---:|---:|---:|
| dev (intech-devp) | 80 MB | ~6 s | ~2 s | **~10 s** |
| staging | 1 GB | ~30 s | ~5 s | **~40 s** |
| prod 1 año | 5 GB | ~2 min | ~30 s (LAN gigabit) | **~2.5 min** |
| prod 3 años | 15 GB | ~5 min | ~2 min | **~7 min** |

**Para backups prod grandes (5+ GB), el panel debe:**
- Permitir cancelar el polling (user puede cerrar la tab y reintentar)
- Mostrar progreso real (size_bytes durante el polling)
- Streaming con keep-alive del panel ↔ NKR (sino socket timeout)

### Warning de tamaño grande (UX) — propuesta de límite blando 1 GB

NKR **NO impone límite de tamaño** server-side — sirve archivos del tamaño que sean. Pero el panel **debería mostrar un warning de UX al user** cuando el backup pesa más de 1 GB, porque:
- Browsers en mobile / cellular pueden cortar downloads grandes mid-transfer
- El user puede no querer gastar 5+ minutos descargando si solo quería revisar datos
- Si el user está en una conexión metered, ver el tamaño le permite decidir

**Implementación sugerida en el frontend:**

```javascript
const SIZE_WARN_THRESHOLD = 1024 * 1024 * 1024; // 1 GB

// Cuando llega status=ready y antes de disparar la descarga:
if (status.size_bytes > SIZE_WARN_THRESHOLD) {
    const sizeGB = (status.size_bytes / 1024 / 1024 / 1024).toFixed(2);
    const confirm = await showModal({
        title: 'Backup grande',
        message: `Este backup pesa ${sizeGB} GB. La descarga puede tardar varios minutos
                  según tu conexión. ¿Continuar?`,
        buttons: ['Cancelar', 'Descargar igual'],
    });
    if (!confirm) return;
}
// ... proceder con la descarga
```

No es un hard limit — el user puede continuar si confirma. Solo evita que se inicie una descarga gigante por accidente.

### Para limit hard server-side (si en el futuro hace falta)

Si surge necesidad operativa de tope absoluto (ej. evitar saturar el panel proxy con un backup de 50 GB que mata el ancho de banda), agregar en NKR daemon:
- Variable env `NKR_MAX_BACKUP_DOWNLOAD_BYTES` (default sin tope)
- Si el archivo del backup excede el tope → `GET /download` devuelve `413 Payload Too Large` con mensaje "use CLI access"
- El operador puede acceder directo via SSH al host con el path en el status response

**No implementado en esta etapa** — esperar a que surja el caso real.

---

## 7. ¿Por qué NO direct-link (link directo firmado)?

Esa fue la primera opción analizada y rechazada. Tres razones:

1. **El hostname de NKR queda visible en el browser del user** (en la URL bar, en DevTools Network tab, en el clipboard si copia el link). Eso revela infraestructura interna que NKR está diseñada para mantener detrás del panel.

2. **URLs firmadas son reenviables dentro del TTL**. Aunque expiren en 30s, ese intervalo es suficiente para que alguien con DevTools la copie y la mande a otro. No hay forma robusta de prevenirlo sin IP-locking que rompe en mobile (NAT).

3. **No hay logging/audit propio del panel**. Con links firmados, NKR ve la descarga pero el panel no — pierde el rastro de "qué user descargó qué backup".

El proxy del panel cuesta un poco de bandwidth extra pero gana visibility completa y oculta NKR.

---

## 8. Variantes futuras (no para esta etapa)

### 8.1 Subdomain reverse-proxy del panel

Si los backups crecen a 10+ GB y el bandwidth del panel se vuelve un cuello de botella, se puede migrar a:

```nginx
# panel-edge nginx config
server {
    listen 443 ssl;
    server_name downloads.panel.systemouts.com;

    location ~ ^/(?<jobid>bkp_[a-z0-9]+)/(?<token>[a-z0-9]+)$ {
        # Pre-auth: el panel firma un HMAC del job_id en su backend.
        # nginx valida la firma sin ir al backend.
        set $hmac_ok 0;
        # ... lua/openresty pre-auth ...

        if ($hmac_ok = "0") { return 403; }

        proxy_pass http://nkr-backend/api/v1/backups/$jobid/download;
        proxy_set_header Authorization "Bearer $NKR_TOKEN";
        proxy_buffering off;       # streaming
        proxy_request_buffering off;
    }
}
```

User ve URL `downloads.panel.systemouts.com/bkp_xxx/abc123` (subdomain del panel) y nginx reverse-proxea a NKR. Streaming sin pasar por el panel backend.

Más complejo pero escalable. Diferido hasta que sea necesario.

### 8.2 Pre-firmar URLs con HMAC del panel (sin proxy)

Variante: el panel firma un HMAC del `job_id + exp` con su secreto. El user va al subdomain del panel con `?sig=...&exp=...`. nginx valida en pre-auth. Solo se llega a NKR si la firma es válida.

Combinable con 8.1.

### 8.3 Stream con `Content-Range` para resumable downloads

Para backups de 5+ GB en redes inestables. Requiere implementar Range requests en NKR (no soportado hoy). v2 si hay queja real de usuarios.

---

## 9. Checklist de implementación para el panel

- [ ] Variables de entorno: `NKR_API_URL`, `NKR_API_TOKEN` (server-side only, NUNCA en frontend bundle)
- [ ] 3 routes en el backend del panel: `/start`, `/status`, `/download`
- [ ] Auth middleware: verifica sesión del user en cada uno
- [ ] DB: tabla `backup_jobs` con (id, tenant_id, user_id, status, created_at, downloaded_at)
- [ ] UI: spinner + estado del polling, mensaje claro "backup expira en 24h"
- [ ] Audit log: log cada `start` / `status` / `download` con user_id + tenant_id + IP
- [ ] Error handling: cubrir todos los códigos de §5
- [ ] Streaming: usar `pipe()` o equivalente — NO buffer to memory
- [ ] Tests: end-to-end con tenant dev, verificar que el ZIP es válido (`unzip -t backup.zip`)

---

## 10. Soporte y debugging

**Si el panel pide un backup y NKR responde 503/timeout:**
- Verificar daemon NKR vivo: `curl http://nkr:9090/api/v1/health`
- Ver logs del daemon: `journalctl -u nkr -n 100`
- Ver logs del backup en `/var/log/nkr-backup-cleanup.log` (cron) o el output del backup en proceso

**Si el ZIP descargado está corrupto:**
- Verificar `SHA256` header del response NKR matchea el del archivo
- Reintentar — si persiste, escalar a NKR ops (puede ser bug en pg_dump output)

**Si el panel ve `job_not_found` minutos después de crearlo:**
- ¿Pasaron las 24 h y se ejecutó el cleanup? Es retention por design.
- Generar nuevo backup.

**Contacto NKR ops:** ver `MAINTENANCE.md` para runbooks completos.

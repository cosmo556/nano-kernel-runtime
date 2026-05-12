# `nkr_sso` — Módulo Odoo para SSO desde el panel NKR

Especificación para el equipo del panel / desarrollador del módulo Odoo.

NKR (v1.6.3+) emite URLs firmadas con HMAC-SHA256 que permiten al panel hacer auto-login en cualquier user del tenant sin conocer su password. Este módulo Odoo verifica la firma y crea la sesión.

---

## 1. Contrato HTTP

NKR genera URLs así:

```
https://<tenant-dns>/nkr-sso?u=<login>&exp=<unix_ts>&sig=<hmac_sha256_hex>
```

Donde:
- `u` — login del user en `res.users` (admin, vendedor, etc.). URL-encoded.
- `exp` — unix timestamp UTC en segundos. Expira ~30s después de emisión.
- `sig` — HMAC-SHA256 hex (64 chars) del payload `f"{u}|{exp}"` usando la HMAC key del `odoo.conf` (sección `[nkr_sso]` clave `secret`; legacy: `nkr_sso_secret` en `[options]`).

El módulo Odoo expone un controller en `/nkr-sso` que:
1. Lee la HMAC key del `odoo.conf` — `[nkr_sso] secret` (re-parseando el rc file; Odoo 19 no expone secciones no-`[options]`), con fallback a `nkr_sso_secret` de `[options]`.
2. Reconstruye el payload `f"{u}|{exp}"` y computa HMAC con el secret local.
3. Compara con `sig` usando `hmac.compare_digest` (constant time).
4. Verifica que `exp >= time.time()`.
5. Busca al user por `login` en `res.users`.
6. Crea sesión sudo (sin pedir password) y redirige a `/odoo`.

---

## 2. Estructura del módulo

```
nkr_sso/
├── __init__.py
├── __manifest__.py
└── controllers/
    ├── __init__.py
    └── main.py
```

### `__manifest__.py`

```python
{
    "name": "NKR SSO",
    "version": "19.0.1.0.0",
    "author": "SystemOuts",
    "category": "Hidden",
    "summary": "Auto-login firmado HMAC desde el panel NKR",
    "description": "Verifica URLs firmadas por NKR (clave compartida en odoo.conf) y crea sesión sudo del user solicitado. Diseñado para entrar desde el panel sin compartir passwords.",
    "depends": ["web"],
    "data": [],
    "installable": True,
    "application": False,
    "auto_install": False,
    "license": "LGPL-3",
}
```

### `__init__.py`

```python
from . import controllers
```

### `controllers/__init__.py`

```python
from . import main
```

### `controllers/main.py`

```python
import configparser
import hashlib
import hmac as hmac_lib
import logging
import time

from odoo import http
from odoo.http import request
from odoo.tools import config

_logger = logging.getLogger(__name__)


def _nkr_sso_secret():
    """HMAC key compartida con NKR. Ubicación preferida: sección `[nkr_sso]`
    clave `secret` del odoo.conf — NO genera el WARNING "unknown option" que
    Odoo emite por keys desconocidas de `[options]`. Como Odoo 19 NO expone
    las secciones no-`[options]` en `config` (no hay `config.misc`),
    re-parseamos el archivo rc con configparser. Fallback: clave legacy
    `nkr_sso_secret` en `[options]` (que `config.get()` SÍ devuelve — Odoo la
    guarda as-is, junto con el warning benigno).

    Odoo 19: la ruta del rc file es `config["config"]` (`config.rcfile`
    quedó deprecado en 19.0 — accederlo emite un DeprecationWarning).
    """
    rc = config.get("config")
    if rc:
        try:
            cp = configparser.RawConfigParser()
            cp.read(rc)
            if cp.has_option("nkr_sso", "secret"):
                v = cp.get("nkr_sso", "secret").strip()
                if v:
                    return v
        except Exception:  # pragma: no cover
            pass
    return config.get("nkr_sso_secret") or None


class NkrSSO(http.Controller):
    """Auto-login firmado HMAC desde el panel NKR.

    NKR emite URLs `/nkr-sso?u=<login>&exp=<ts>&sig=<hmac>` firmadas con la
    HMAC key compartida que NKR escribe en `odoo.conf` (sección `[nkr_sso]`
    clave `secret`; legacy: `nkr_sso_secret` en `[options]`). Este controller
    verifica la firma + expiry y crea sesión sudo del user solicitado, sin
    pedir password.

    Seguridad:
        - El secret vive sólo en odoo.conf (root del host requerido).
        - Compromiso del secret = login arbitrario al tenant. Rotar
          = editar odoo.conf + REL_OD (POST /reload).
        - Constant-time HMAC compare evita timing attacks.
        - TTL 30s en `exp` limita ventana de replay.
        - El módulo NO expone el secret en ninguna vista ni API pública.
    """

    @http.route("/nkr-sso", type="http", auth="none", csrf=False, methods=["GET"])
    def sso(self, u=None, exp=None, sig=None, **kwargs):
        # 1. Validar parámetros presentes
        if not (u and exp and sig):
            return request.redirect("/web/login?error=sso_missing_params")

        # 2. Verificar expiración primero (barato, antes de HMAC)
        try:
            exp_int = int(exp)
        except (TypeError, ValueError):
            return request.redirect("/web/login?error=sso_bad_exp")
        if exp_int < int(time.time()):
            return request.redirect("/web/login?error=sso_expired")

        # 3. Leer secret de odoo.conf ([nkr_sso] secret, o legacy nkr_sso_secret)
        secret = _nkr_sso_secret()
        if not secret:
            _logger.warning(
                "nkr_sso: secret no configurado en odoo.conf "
                "([nkr_sso] secret = ... , o legacy nkr_sso_secret en [options]). "
                "Agregar la clave y reiniciar Odoo (REL_OD)."
            )
            return request.redirect("/web/login?error=sso_not_configured")

        # 3.5. (Opcional) Filtro de Referer — defense in depth
        # `nkr_sso_allowed_referer = https://panel.tudominio.com` exige
        # que el Referer del browser empiece con esa URL (= el dev viene
        # del panel, no abrió URL directamente desde otro contexto).
        # Limitación: Referer puede ser suprimido por extensions/policies
        # del browser. No es ironclad pero sube el bar para URLs filtradas.
        # Si no se setea en odoo.conf, no filtra.
        allowed_referer = config.get("nkr_sso_allowed_referer", "").strip()
        if allowed_referer:
            referer = request.httprequest.headers.get("Referer", "")
            if not referer.startswith(allowed_referer):
                _logger.warning("nkr_sso: Referer %r no matchea allowed_referer", referer)
                return request.redirect("/web/login?error=sso_referer_denied")

        # 4. Verificar HMAC (constant time)
        payload = f"{u}|{exp}".encode("utf-8")
        expected = hmac_lib.new(
            secret.encode("utf-8"), payload, hashlib.sha256
        ).hexdigest()
        if not hmac_lib.compare_digest(expected, sig.lower()):
            _logger.warning("nkr_sso: firma inválida para user=%s exp=%s", u, exp)
            return request.redirect("/web/login?error=sso_bad_signature")

        # 5. Buscar user — sudo porque el caller es público (auth='none')
        env = request.env
        user = env["res.users"].sudo().search(
            [("login", "=", u), ("active", "=", True)], limit=1
        )
        if not user:
            _logger.warning("nkr_sso: user no encontrado / inactivo: %s", u)
            return request.redirect("/web/login?error=sso_user_not_found")

        # 6. Crear sesión sudo — sin password, confiando en HMAC verificado arriba
        db = request.env.cr.dbname
        # Odoo session_token = HMAC(salt, user_id|password_hash|...) — atado al
        # user. Lo regeneramos dentro del request.session para que la cookie
        # quede válida en sucesivos requests.
        request.session.uid = user.id
        request.session.login = user.login
        request.session.session_token = user._compute_session_token(
            request.session.sid
        )
        request.session.db = db
        request.session.context = dict(user.context_get())
        # Marca para auditoría — opcional
        _logger.info("nkr_sso: login OK uid=%s login=%s db=%s", user.id, u, db)

        return request.redirect("/odoo")
```

---

## 3. Configuración del tenant

NKR genera y escribe la HMAC key en `<instance>/config/odoo.conf` al crear un tenant (NKR ≥1.6.4) — en una **sección propia `[nkr_sso]`**, no en `[options]`:

```ini
[options]
... (resto de la config de Odoo) ...

[nkr_sso]
secret = <64 hex chars random>
```

> **Por qué una sección propia y no `[options]`:** Odoo emite `WARNING odoo.tools.config: unknown option 'X'` por cualquier key de `[options]` que no esté en su schema hardcoded. Las keys de OTRAS secciones no generan ese warning. La forma legacy (NKR 1.6.3) era `nkr_sso_secret = ...` en `[options]` → generaba el warning (benigno — Odoo guardaba el valor as-is y `config.get("nkr_sso_secret")` lo devolvía, pero ruido en log). El módulo lee la forma nueva re-parseando el rc file con `configparser` (Odoo 19 no expone las secciones no-`[options]` en `config`), con `config.get("nkr_sso_secret")` como fallback para tenants no migrados. Ver `_nkr_sso_secret()` en §2.

**Tenants legacy** (con `nkr_sso_secret` en `[options]`, de NKR 1.6.3) siguen funcionando vía el fallback. Para migrarlos (eliminar el warning): mover la línea de `[options]` a una sección `[nkr_sso]` al final del archivo + reiniciar el tenant. Para tenants sin ninguna key (creados antes de 1.6.3) hay que agregarla:

```bash
SECRET=$(head -c 32 /dev/urandom | xxd -p -c 32)
printf '\n[nkr_sso]\nsecret = %s\n' "$SECRET" \
  >> /mnt/nkr/cells/<cell>/instances/<tenant>/config/odoo.conf

# reiniciar el tenant para que Odoo re-lea el conf + el módulo recoja el secret
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"action":"restart"}' \
  $NKR_API/api/v1/cells/<cell>/instances/<tenant>/actions
# (un `POST /reload`/REL_OD también basta si el tenant tiene workers>0; con
#  workers=0 el reload puede no respawnear Odoo limpiamente — usar restart)
```

### Filtro opcional (defense in depth)

```ini
# odoo.conf

# Si se setea, requiere que el browser tenga Referer empezando con esta URL.
# Útil para asegurar que el dev viene del panel (no abrió URL directamente
# desde otro contexto, ej. un email phishing con la URL filtrada).
# Limitación: Referer puede ser suprimido por extensions/policies del browser.
nkr_sso_allowed_referer = https://stdout.systemouts.com
```

Es **opcional**: si no se setea, no filtra. Si se setea, refuerza el HMAC.

(Descartado el filtro por IP — los devs trabajan desde IPs variables — laptop / mobile / VPN — y mantener una whitelist es operativamente costoso. El TTL de 30s ya limita el riesgo de URL filtrada.)

---

## 4. Deploy del módulo al template (one-time setup)

### 4.1 Idea general

Cada cell tiene una instancia template (`<cell>-odoo-template`, ej. `odoo-v19-odoo-template`) y una DB template (`db-master-<ver>`). NKR clona ambas al crear cualquier tenant nuevo:

- **Código** (`addons/`, `config/`, etc.) → `cp -a --reflink=auto` desde el template instance → el nuevo tenant.
- **Base de datos** → `CREATE DATABASE db-<nuevo-tenant> TEMPLATE db-master-<ver>` → O(1), módulos ya instalados se heredan.

Si pre-instalás `nkr_sso` UNA VEZ en el template (código + DB), **cada tenant futuro nace con SSO listo sin más trabajo del panel**.

### 4.2 Procedimiento (deploy a dev tenant → copia al template)

**El equipo panel no necesita un repo `addons-meta` separado.** El flujo real es:

1. **Panel deploya `nkr_sso` a un tenant de desarrollo** con su flujo habitual (ej. `odoo-v19-intech-devp`). Esto deja el módulo en disco del host:
   ```
   /mnt/nkr/cells/odoo-v19/instances/odoo-v19-intech-devp/addons/nkr_sso/
   ```

2. **Operador NKR copia el módulo al template + lo instala en la DB del template** (one-time, por cell):
   ```bash
   . /etc/nkr/api.env
   CELL="odoo-v19"
   DEV_TENANT="odoo-v19-intech-devp"            # tenant donde el panel deployó
   TEMPLATE_TENANT="odoo-v19-odoo-template"     # convención NKR

   # 1. Copiar código del dev tenant → template (filesystem del host)
   cp -a "/mnt/nkr/cells/$CELL/instances/$DEV_TENANT/addons/nkr_sso" \
         "/mnt/nkr/cells/$CELL/instances/$TEMPLATE_TENANT/addons/"

   # 2. Instalar el módulo en la DB del template (db-master-19 en este ejemplo)
   curl -fsS -X POST \
     -H "Authorization: Bearer $NKR_API_TOKEN" \
     -H "Content-Type: application/json" \
     -d '{"op":"install","modules":["nkr_sso"]}' \
     "$NKR_API/api/v1/cells/$CELL/instances/$TEMPLATE_TENANT/modules"

   # 3. Verificar que quedó installed en la DB template
   curl -fsS -X POST \
     -H "Authorization: Bearer $NKR_API_TOKEN" \
     -H "Content-Type: application/json" \
     -d '{"query":"SELECT name, state FROM ir_module_module WHERE name='"'"'nkr_sso'"'"';"}' \
     "$NKR_API/api/v1/cells/$CELL/instances/$TEMPLATE_TENANT/psql" | jq
   # esperás: { "rows": [["nkr_sso", "installed"]] }
   ```

3. **Repetir paso 2 por cada cell** que vaya a generar tenants nuevos (`odoo-v17`, `odoo-v19`, etc.).

Tras esto, **cualquier tenant creado vía `POST /instances` ya tiene `/nkr-sso` activo sin trabajo extra**.

### 4.3 Por qué esto funciona

| Componente | Mecanismo | Resultado |
|---|---|---|
| `addons/nkr_sso/` en disco | `cp -a --reflink=auto` del template instance | El nuevo tenant ya tiene el código en su `addons/` |
| Fila `ir_module_module.nkr_sso = installed` | `CREATE DATABASE … TEMPLATE db-master-<ver>` | El nuevo tenant nace con el módulo ya marcado como instalado en su DB |
| Ruta `/nkr-sso` registrada | Odoo `_register_hook` al boot lee modules installed | Activa desde el primer arranque del nuevo tenant |
| `[nkr_sso] secret` en `odoo.conf` | NKR genera uno random nuevo en cada `POST /instances` (cell.rs `rewrite_odoo_conf_full`) | Cada tenant tiene **su propio** secret, no se reusa el del template |

> **Detalle clave**: el secret NO se hereda del template — NKR lo regenera por tenant. Si heredáramos el secret, comprometer uno = comprometer todos. La separación HMAC-key por tenant la hace NKR sola, sin trabajo del panel.

### 4.4 Tareas del panel en cada `POST /instances`

Como el módulo ya viene en el template (sección 4.2) **el panel NO necesita hacer install per-tenant**. El checklist post-create es:

1. `POST /instances` → NKR clona código + DB del template (incluye `nkr_sso` ya installed).
2. `POST /dns` → NKR provisiona cert + nginx vhost.
3. `POST /instances/<tenant>/start` (o `running=true` en el body del create) → arranca la VM.
4. (Opcional) Verificar que el módulo respondió ok: `GET https://<dns>/nkr-sso` debe devolver 302 a `/web/login?error=sso_missing_params` (es esperado sin params — significa que el controller está vivo).

### 4.5 Cuándo hacer install per-tenant (fallback)

Sólo en estos casos:

- **Tenants legacy** (creados antes de pre-instalar `nkr_sso` en el template) → copiar el módulo al tenant + install (sección 4.6).
- **Bugfix urgente del módulo en un tenant específico** sin querer tocar el template → mismo flujo, con `op:"upgrade"`.
- **Versión de `nkr_sso` distinta por tenant** (raro pero posible) → mismo flujo.

### 4.6 Flujo per-instance (legacy / hotfix)

```bash
CELL="odoo-v19"
DEV_TENANT="odoo-v19-intech-devp"
TARGET_TENANT="odoo-v19-<tenant>"

# 1. Copiar código al tenant (desde el dev tenant o desde el template)
cp -a "/mnt/nkr/cells/$CELL/instances/$DEV_TENANT/addons/nkr_sso" \
      "/mnt/nkr/cells/$CELL/instances/$TARGET_TENANT/addons/"

# 2. Install (o upgrade) en la DB del tenant
curl -X POST -H "Authorization: Bearer $NKR_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"op":"install","modules":["nkr_sso"]}' \
  "$NKR_API/api/v1/cells/$CELL/instances/$TARGET_TENANT/modules"
```

Una vez instalado, el controller `/nkr-sso` queda activo permanentemente en ese tenant.

### 4.7 Actualizar el módulo en TODOS los tenants existentes

Cuando salga una versión nueva de `nkr_sso` (ej. fix de bug, feature):

```bash
CELL="odoo-v19"
DEV_TENANT="odoo-v19-intech-devp"        # donde el panel deployó la versión nueva
TEMPLATE_TENANT="odoo-v19-odoo-template"

# 1. Panel deploya la versión nueva al dev tenant con su flujo habitual.

# 2. Operador NKR refresca código del template + upgrade
cp -a "/mnt/nkr/cells/$CELL/instances/$DEV_TENANT/addons/nkr_sso" \
      "/mnt/nkr/cells/$CELL/instances/$TEMPLATE_TENANT/addons/"
curl -X POST -H "Authorization: Bearer $NKR_API_TOKEN" \
  -d '{"op":"upgrade","modules":["nkr_sso"]}' \
  "$NKR_API/api/v1/cells/$CELL/instances/$TEMPLATE_TENANT/modules"

# 3. Por cada tenant existente, refrescar código + upgrade (loop)
for TENANT in $(panel list tenants); do
  cp -a "/mnt/nkr/cells/$CELL/instances/$DEV_TENANT/addons/nkr_sso" \
        "/mnt/nkr/cells/$CELL/instances/$TENANT/addons/"
  curl -X POST -H "Authorization: Bearer $NKR_API_TOKEN" \
    -d '{"op":"upgrade","modules":["nkr_sso"]}' \
    "$NKR_API/api/v1/cells/$CELL/instances/$TENANT/modules"
done
```

El upgrade en cada tenant tarda ~2-5s (registro de hook + invalidación cache asset).

---

## 5. Integración del panel (frontend)

```javascript
// SSO genérico: cualquier login del tenant (admin, vendedor, contador, etc.)
async function ssoEnterTenant(tenant, user = 'admin') {
  const resp = await fetch(
    `${NKR_API}/api/v1/cells/${tenant.cell}/instances/${tenant.nkr_name}/sso`,
    {
      method: 'POST',
      headers: {
        'Authorization': `Bearer ${NKR_API_TOKEN}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({ user }),  // ← cualquier login válido en res.users
    }
  );
  if (!resp.ok) {
    const err = await resp.json();
    alert(`SSO error: ${err.error} — ${err.message}`);
    return;
  }
  const { url } = await resp.json();
  window.open(url, '_blank');  // URL válida 30s
}

// Ejemplos de uso:
ssoEnterTenant(tenant);                          // → entra como "admin" (default)
ssoEnterTenant(tenant, 'admin');                 // → entra como "admin"
ssoEnterTenant(tenant, 'vendedor1');             // → entra como user "vendedor1"
ssoEnterTenant(tenant, 'contador@empresa.com');  // → entra como ese email (Odoo soporta login email)
```

### ¿Cómo sabe el panel qué users existen en el tenant?

**Usar `POST /psql` de NKR — esta es la forma canónica.** NKR ya expone este endpoint y lee directamente la DB del tenant. Cero código nuevo en NKR, cero módulo Odoo extra, y siempre devuelve la lista REAL del momento (no copias desincronizadas).

```javascript
// Ejemplo desde el panel backend
async function fetchTenantUsers(tenant) {
  const resp = await fetch(
    `${NKR_API}/api/v1/cells/${tenant.cell}/instances/${tenant.nkr_name}/psql`,
    {
      method: 'POST',
      headers: {
        'Authorization': `Bearer ${NKR_API_TOKEN}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({
        query: `
          SELECT u.login, p.name, u.active
          FROM res_users u
          JOIN res_partner p ON u.partner_id = p.id
          WHERE u.active = true
          ORDER BY (u.login = 'admin') DESC, u.login;
        `
      }),
    }
  );
  const data = await resp.json();
  // data.rows = [["admin", "Administrator", true], ...]
  return data.rows.map(([login, name, active]) => ({ login, name, active }));
}
```

**Por qué psql es la forma correcta** (no inventar otras):

| Alternativa | Problema |
|---|---|
| Panel guarda lista propia | Se desincroniza si alguien crea users desde la UI de Odoo |
| Panel hace JSON-RPC al tenant | Requiere auth (chicken-and-egg con SSO) |
| NKR expone endpoint helper | Innecesario — psql ya cubre el caso |
| **psql con SELECT FROM res_users** | ✅ Single source of truth, sin código nuevo |

**Cachear OK** (5 min razonable) pero leer fresh al hacer click "Refrescar users" en el panel. Eso da el balance correcto entre carga al backend y datos actualizados.

### Patrón UI: selector con default admin + botón Entrar

**El UX esperado** (lo que pide el operador):

```
┌─────────────────────────────────────────────────────┐
│ Tenant: intech-devp                                 │
│ Estado: ● running                                   │
│ DNS:    intech-devp.oa-odoo.com                     │
│                                                     │
│ Entrar como:  [admin (super)            ▼] [Entrar →] │
│                ├─ admin (super)                     │
│                ├─ contador@empresa.com              │
│                ├─ vendedor1 — Pepe                  │
│                └─ soporte                           │
└─────────────────────────────────────────────────────┘
```

- **Default**: `admin` (super-admin del tenant)
- **Selector**: dropdown con todos los users `active=True` del tenant
- **Botón "Entrar →"**: dispara `ssoEnterTenant(tenant, selectedUser)` y abre la URL en pestaña nueva

### Implementación React completa

```jsx
import { useEffect, useState } from 'react';

function TenantSsoButton({ tenant }) {
  const [users, setUsers] = useState([{ login: 'admin', name: 'Administrator', active: true }]);
  const [selected, setSelected] = useState('admin');
  const [loading, setLoading] = useState(false);

  // Al montar: cargar lista de users del tenant
  useEffect(() => {
    fetchTenantUsers(tenant)
      .then(setUsers)
      .catch((e) => console.error('No pude cargar users del tenant', e));
  }, [tenant.id]);

  async function handleEnter() {
    setLoading(true);
    try {
      const url = await ssoUrl(tenant, selected);
      window.open(url, '_blank', 'noopener');
    } catch (e) {
      alert(`Error SSO: ${e.message}`);
    } finally {
      setLoading(false);
    }
  }

  return (
    <div className="tenant-sso">
      <label>Entrar como:</label>
      <select
        value={selected}
        onChange={(e) => setSelected(e.target.value)}
        disabled={loading}
      >
        {users.map(u => (
          <option key={u.login} value={u.login}>
            {u.name === u.login ? u.login : `${u.name} (${u.login})`}
            {u.login === 'admin' ? ' — super' : ''}
          </option>
        ))}
      </select>
      <button onClick={handleEnter} disabled={loading}>
        {loading ? 'Generando…' : 'Entrar →'}
      </button>
    </div>
  );
}

// helpers
async function fetchTenantUsers(tenant) {
  const r = await fetch(`/api/tenant/${tenant.id}/users`);  // panel backend
  return r.ok ? r.json() : [{ login: 'admin', name: 'Administrator', active: true }];
}

async function ssoUrl(tenant, user) {
  const r = await fetch(`/api/tenant/${tenant.id}/sso`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ user }),
  });
  if (!r.ok) throw new Error((await r.json()).message);
  return (await r.json()).url;
}
```

### Backend del panel (Python ejemplo)

```python
# panel/views/tenant.py

@app.route("/api/tenant/<tid>/users")
@login_required
def list_tenant_users(tid):
    """Devuelve users active del tenant para el dropdown SSO."""
    tenant = Tenant.objects.get(id=tid)
    # Cachear ~5 min — los users no cambian seguido
    cached = cache.get(f"users:{tid}")
    if cached:
        return jsonify(cached)
    
    resp = requests.post(
        f"{NKR_API_BASE}/api/v1/cells/{tenant.cell}/instances/{tenant.nkr_name}/psql",
        headers={"Authorization": f"Bearer {NKR_API_TOKEN}"},
        json={"query": """
            SELECT u.login, p.name, u.active
            FROM res_users u
            JOIN res_partner p ON u.partner_id = p.id
            WHERE u.active = true
            ORDER BY (u.login = 'admin') DESC, u.login;
        """},
        timeout=10,
    )
    rows = resp.json().get("rows", [])
    users = [{"login": l, "name": n, "active": a} for (l, n, a) in rows]
    cache.set(f"users:{tid}", users, timeout=300)
    return jsonify(users)


@app.route("/api/tenant/<tid>/sso", methods=["POST"])
@login_required
def open_tenant_sso(tid):
    """Genera URL SSO firmada — recibe `user` del body."""
    tenant = Tenant.objects.get(id=tid)
    user = request.json.get("user", "admin")
    
    # Audit log — quién del panel entró como quién al tenant
    AuditLog.create(
        actor=current_user.email,
        action="sso",
        tenant=tenant.id,
        target_user=user,
    )
    
    resp = requests.post(
        f"{NKR_API_BASE}/api/v1/cells/{tenant.cell}/instances/{tenant.nkr_name}/sso",
        headers={"Authorization": f"Bearer {NKR_API_TOKEN}"},
        json={"user": user},
        timeout=10,
    )
    if not resp.ok:
        return jsonify(resp.json()), resp.status_code
    return jsonify(resp.json())
```

### Contrato resumen para el equipo panel

1. **Al cargar `/tenant/<id>`** → llamar `GET /api/tenant/<id>/users` → cargar dropdown.
2. **Al hacer click "Entrar →"** → llamar `POST /api/tenant/<id>/sso {user}` → recibir `{url}` → `window.open(url)`.
3. **Default del selector**: `admin`. **Orden**: admin primero, después alfabético.
4. **Cache** de la lista de users: 5 min OK (no cambian seguido).
5. **Audit log** del panel: registrar quién del panel hizo SSO como quién del tenant, para trazabilidad.

**Importante**: el user debe estar `active=True` en `res.users` del tenant. Si no, el módulo `nkr_sso` redirige a `/web/login?error=sso_user_not_found`. La query SQL del paso 1 ya filtra por `active=true`, así que el dropdown solo muestra los válidos.

---

## 6. Endpoint NKR

```
POST /api/v1/cells/{cell}/instances/{nkr_name}/sso
Headers:
  Authorization: Bearer <NKR_API_TOKEN>
  Content-Type: application/json
Body:
  {"user": "admin"}        ← opcional, default "admin"

Response 200:
  {
    "url": "https://<dns>/nkr-sso?u=<login>&exp=<ts>&sig=<hex>",
    "user": "<login>",
    "expires_in": 30,
    "nkr_name": "<tenant>",
    "dns": "<dns>"
  }

Errores:
  400 invalid_nkr_name        ← name fuera de [A-Za-z0-9._-]{1,64}
  400 invalid_user            ← user fuera de [A-Za-z0-9._\\-@]{1,128}
  404 instance_not_found
  409 not_running             ← tenant apagado
  409 no_dns_provisioned      ← POST /dns no llamado todavía
  500 sso_secret_missing      ← falta nkr_sso_secret en odoo.conf
  500 hmac_key_invalid        ← secret malformado
```

---

## 7. Tabla de seguridad

| Recurso | Vive en | Quién lo lee |
|---|---|---|
| HMAC key (256 bits, `[nkr_sso] secret`) | `<instance>/config/odoo.conf` (filesystem host) | NKR daemon (root) + proceso Odoo (al boot) |
| URL firmada | Browser del dev / panel | Logueada en server access logs |
| Cookie de sesión post-SSO | Browser del dev | Cliente HTTP del dev |
| Password del user | DB del tenant (hash pbkdf2-sha512) | **Nadie lo necesita para SSO** |

### Compromiso del `nkr_sso_secret`

Si un atacante obtiene el secret:
- Puede emitir URLs SSO válidas para cualquier user del tenant.
- **Mitigación**: rotar el secret. Editar `odoo.conf` + `POST /reload` (≤5s).

### Por qué `auth='none'` en el controller

El controller intencionalmente no requiere autenticación (de hecho, todo su propósito es crear una sesión sin pedir password). La autenticación se hace **vía la firma HMAC** que verificamos manualmente — equivalente a la auth pero usando criptografía simétrica con el host.

### Por qué `constant time compare`

`hmac.compare_digest` evita timing attacks que podrían extraer el secret byte a byte midiendo cuánto tarda la comparación.

---

## 8. Testing

```bash
# 1. Generar URL via NKR
URL=$(curl -s -X POST -H "Authorization: Bearer $NKR_API_TOKEN" \
       -H "Content-Type: application/json" \
       -d '{"user":"admin"}' \
       $NKR_API/api/v1/cells/<cell>/instances/<tenant>/sso \
       | jq -r .url)

# 2. Abrir en browser (o curl con -L para follow redirect)
xdg-open "$URL"
# o
curl -v -c /tmp/sso-cookie.txt "$URL"
cat /tmp/sso-cookie.txt   # debe tener session_id válido

# 3. Verificar sesión
curl -b /tmp/sso-cookie.txt https://<tenant-dns>/odoo
# debe responder 200 con la UI logueada
```

### Casos de error a probar

| Test | URL modificada | Esperado |
|---|---|---|
| HMAC válido + exp futuro | URL del paso 1 | 302 → /odoo |
| HMAC válido + exp en el pasado | cambiar `exp` a `1` | 302 → /web/login?error=sso_expired |
| HMAC inválido | cambiar 1 char de `sig` | 302 → /web/login?error=sso_bad_signature |
| User inexistente | `u=fakeuser` (HMAC re-firmar) | 302 → /web/login?error=sso_user_not_found |
| Faltan params | quitar `sig` | 302 → /web/login?error=sso_missing_params |

---

## 9. Comparación con OAuth2/SAML

NKR SSO es una versión **simplificada y específica** para el setup NKR + Odoo:

| | OAuth2 | NKR SSO |
|---|---|---|
| Necesita servidor IdP | Sí (Keycloak/Auth0/etc) | NKR ya lo es |
| Estado server-side | Sí (refresh tokens, sessions) | No — stateless HMAC |
| Configuración | Endpoints, scopes, clients | Una clave en odoo.conf |
| Federación cross-tenant | Sí | No (cada tenant tiene su secret) |
| Latencia | 3-4 round trips | 1 redirect |
| Caso de uso | Multi-app, multi-IdP | Operador NKR → tenants del operador |

Si en el futuro NKR necesita SSO con providers externos (Google, Okta), se puede agregar un módulo `nkr_oauth` separado sin tocar este.

---

## Resumen

- **NKR** (v1.6.3+) emite URLs firmadas HMAC vía `POST /sso`.
- **Módulo Odoo `nkr_sso`** (este doc) verifica HMAC + crea sesión sudo.
- **Panel** llama POST /sso, abre URL en pestaña nueva.
- **El password del user nunca entra al flujo.**
- **Secret único por tenant** en `odoo.conf`.
- **TTL 30s** + **rotación via REL_OD**.

---

## 10. Cookbook — ejemplos para devs

### 10.1 curl: pedir URL SSO y abrirla

```bash
#!/bin/bash
# nkr-sso.sh — usage: ./nkr-sso.sh <cell> <nkr_name> [user]
set -euo pipefail

CELL="${1:?usage: $0 <cell> <nkr_name> [user]}"
TENANT="${2:?usage: $0 <cell> <nkr_name> [user]}"
USER="${3:-admin}"

# Asumiendo NKR_API_BASE y NKR_API_TOKEN en /etc/nkr/api.env o exportados
: "${NKR_API_BASE:=http://127.0.0.1:9090}"
: "${NKR_API_TOKEN:?NKR_API_TOKEN no seteado}"

URL=$(curl -fsS -X POST \
  -H "Authorization: Bearer $NKR_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"user\":\"$USER\"}" \
  "$NKR_API_BASE/api/v1/cells/$CELL/instances/$TENANT/sso" \
  | jq -r .url)

echo "Abriendo: $URL"
xdg-open "$URL" 2>/dev/null || open "$URL" 2>/dev/null || echo "$URL"
```

Uso:
```bash
./nkr-sso.sh odoo-v19 odoo-v19-intech-devp admin
./nkr-sso.sh odoo-v19 odoo-v19-cliente-42 vendedor1
./nkr-sso.sh odoo-v17 odoo-v17-acme contador@empresa.com
```

### 10.2 Python (panel backend o CLI)

```python
import requests
import webbrowser
from urllib.parse import urlparse

NKR_API_BASE = "http://127.0.0.1:9090"
NKR_API_TOKEN = "<your-bearer>"


def sso_url(cell: str, tenant: str, user: str = "admin") -> str:
    """Pide a NKR una URL SSO one-shot para el `user` del `tenant`.
    
    Returns:
        URL firmada (válida 30s). Hacer redirect del browser inmediato.
    Raises:
        requests.HTTPError si NKR rechaza.
    """
    resp = requests.post(
        f"{NKR_API_BASE}/api/v1/cells/{cell}/instances/{tenant}/sso",
        headers={
            "Authorization": f"Bearer {NKR_API_TOKEN}",
            "Content-Type": "application/json",
        },
        json={"user": user},
        timeout=10,
    )
    resp.raise_for_status()
    return resp.json()["url"]


def sso_open(cell: str, tenant: str, user: str = "admin") -> None:
    """Abre el SSO en el browser local (uso CLI / dev tooling)."""
    url = sso_url(cell, tenant, user)
    print(f"→ {urlparse(url).netloc} as {user}")
    webbrowser.open(url, new=2)


if __name__ == "__main__":
    import sys
    sso_open(sys.argv[1], sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else "admin")
```

Uso CLI:
```bash
python sso.py odoo-v19 odoo-v19-intech-devp admin
```

Uso desde Django/Flask panel backend:
```python
@app.route("/tenant/<tid>/sso-open")
def open_tenant_sso(tid):
    tenant = Tenant.objects.get(id=tid)
    url = sso_url(tenant.cell, tenant.nkr_name, request.user.odoo_login)
    return redirect(url, code=302)
```

### 10.3 Node.js / TypeScript (panel backend)

```typescript
import fetch from 'node-fetch';

interface SsoResponse {
  url: string;
  user: string;
  expires_in: number;
  nkr_name: string;
  dns: string;
}

export async function nkrSsoUrl(
  cell: string,
  tenant: string,
  user: string = 'admin'
): Promise<SsoResponse> {
  const NKR_API_BASE = process.env.NKR_API_BASE!;
  const NKR_API_TOKEN = process.env.NKR_API_TOKEN!;

  const resp = await fetch(
    `${NKR_API_BASE}/api/v1/cells/${cell}/instances/${tenant}/sso`,
    {
      method: 'POST',
      headers: {
        'Authorization': `Bearer ${NKR_API_TOKEN}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({ user }),
    }
  );

  if (!resp.ok) {
    const err = await resp.json() as { error: string; message: string };
    throw new Error(`NKR SSO failed: ${err.error} — ${err.message}`);
  }

  return await resp.json() as SsoResponse;
}

// Express route ejemplo
app.get('/api/tenant/:id/sso', requireAuth, async (req, res) => {
  const tenant = await db.tenant.findUnique({ where: { id: req.params.id }});
  if (!tenant) return res.status(404).end();
  const { url } = await nkrSsoUrl(tenant.cell, tenant.nkrName, req.user.odooLogin);
  res.redirect(302, url);
});
```

### 10.4 PHP (Laravel/raw)

```php
<?php
// app/Services/NkrSso.php

class NkrSsoService
{
    private string $base;
    private string $token;

    public function __construct()
    {
        $this->base  = env('NKR_API_BASE', 'http://127.0.0.1:9090');
        $this->token = env('NKR_API_TOKEN');
    }

    /**
     * @return array{url:string, user:string, expires_in:int, nkr_name:string, dns:string}
     */
    public function ssoUrl(string $cell, string $tenant, string $user = 'admin'): array
    {
        $ch = curl_init("{$this->base}/api/v1/cells/{$cell}/instances/{$tenant}/sso");
        curl_setopt_array($ch, [
            CURLOPT_POST           => true,
            CURLOPT_RETURNTRANSFER => true,
            CURLOPT_HTTPHEADER     => [
                "Authorization: Bearer {$this->token}",
                'Content-Type: application/json',
            ],
            CURLOPT_POSTFIELDS => json_encode(['user' => $user]),
            CURLOPT_TIMEOUT    => 10,
        ]);
        $body = curl_exec($ch);
        $code = curl_getinfo($ch, CURLINFO_HTTP_CODE);
        curl_close($ch);

        if ($code !== 200) {
            throw new \RuntimeException("NKR SSO HTTP $code: $body");
        }
        return json_decode($body, true);
    }
}

// Route
Route::get('/tenant/{id}/sso', function ($id) {
    $tenant = Tenant::findOrFail($id);
    $sso = app(NkrSsoService::class)->ssoUrl(
        $tenant->cell,
        $tenant->nkr_name,
        auth()->user()->odoo_login ?? 'admin'
    );
    return redirect()->away($sso['url']);
})->middleware('auth');
```

### 10.5 React component (frontend del panel)

```jsx
import { useState } from 'react';

function TenantEnterButton({ tenant, availableUsers }) {
  const [loading, setLoading] = useState(false);
  const [selectedUser, setSelectedUser] = useState('admin');

  async function handleEnter() {
    setLoading(true);
    try {
      const resp = await fetch(
        `/api/tenant/${tenant.id}/sso?user=${encodeURIComponent(selectedUser)}`,
        { credentials: 'include' }  // sesión del panel
      );
      if (!resp.ok) {
        const err = await resp.json();
        alert(`Error: ${err.error || resp.statusText}`);
        return;
      }
      const { url } = await resp.json();
      window.open(url, '_blank', 'noopener');
    } finally {
      setLoading(false);
    }
  }

  return (
    <div style={{display: 'inline-flex', gap: 8}}>
      <select
        value={selectedUser}
        onChange={(e) => setSelectedUser(e.target.value)}
        disabled={loading}
      >
        {availableUsers.map(u => (
          <option key={u.login} value={u.login}>
            {u.name} ({u.login})
          </option>
        ))}
      </select>
      <button onClick={handleEnter} disabled={loading}>
        {loading ? 'Generando…' : 'Entrar →'}
      </button>
    </div>
  );
}
```

### 10.6 Test E2E en bash (debugging del módulo)

```bash
#!/bin/bash
# test-sso.sh — valida flujo completo NKR + módulo Odoo
set -euo pipefail

. /etc/nkr/api.env
CELL="${1:-odoo-v19}"
TENANT="${2:-odoo-v19-intech-devp}"
USER="${3:-admin}"

echo "1️⃣  Pidiendo URL a NKR…"
RESP=$(curl -fsS -X POST \
  -H "Authorization: Bearer $NKR_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"user\":\"$USER\"}" \
  "http://127.0.0.1:9090/api/v1/cells/$CELL/instances/$TENANT/sso")

URL=$(echo "$RESP" | jq -r .url)
echo "    URL: $URL"
echo "    expira en: $(echo "$RESP" | jq -r .expires_in)s"

echo ""
echo "2️⃣  Llamando al endpoint /nkr-sso (módulo Odoo) — esperamos redirect 302…"
COOKIES=$(mktemp)
trap "rm -f $COOKIES" EXIT

HTTP_CODE=$(curl -sS -o /tmp/sso-body.html -w "%{http_code}" \
  -c "$COOKIES" \
  -L \
  -e "https://panel.example.com/tenants" \
  "$URL")

echo "    HTTP final: $HTTP_CODE"

echo ""
echo "3️⃣  Cookies guardadas:"
grep "session_id" "$COOKIES" | head -1

echo ""
echo "4️⃣  Validando sesión — GET /web (debe ser 200 logueado):"
curl -sS -b "$COOKIES" -o /dev/null -w "    %{http_code}\n" \
  "https://$(echo "$RESP" | jq -r .dns)/web"

echo ""
echo "5️⃣  Si HTTP_CODE != 200, revisar log Odoo:"
echo "    journalctl -u nkr | grep nkr_sso"
echo "    cat /mnt/nkr/cells/$CELL/instances/$TENANT/logs/odoo.log | grep nkr_sso"
```

Casos esperados:
- `200` → SSO OK, sesión activa, `/web` carga la UI.
- `302 → /web/login?error=sso_expired` → URL vencida (>30s entre paso 1 y 2).
- `302 → /web/login?error=sso_bad_signature` → secret en NKR ≠ secret en odoo.conf.
- `302 → /web/login?error=sso_user_not_found` → `$USER` no existe o está inactivo.
- `302 → /web/login?error=sso_not_configured` → módulo instalado pero falta `nkr_sso_secret` en odoo.conf.
- `404` en `/nkr-sso` → módulo NO instalado en el tenant.

### 10.7 Verificar instalación del módulo en un tenant

```bash
. /etc/nkr/api.env
TENANT="odoo-v19-intech-devp"
CELL="odoo-v19"

# Vía psql al PG del tenant — lista módulos instalados
curl -s -X POST \
  -H "Authorization: Bearer $NKR_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"query":"SELECT name, state FROM ir_module_module WHERE name = '"'"'nkr_sso'"'"';"}' \
  "http://127.0.0.1:9090/api/v1/cells/$CELL/instances/$TENANT/psql" | jq

# Si está, esperás:
#   { "rows": [["nkr_sso", "installed"]] }
# Si NO está instalado:
#   { "rows": [["nkr_sso", "uninstalled"]] }  ← código está en addons/, falta Install
#   { "rows": [] }                            ← el código nunca llegó a addons/nkr_sso/
```

Para forzar Install vía API (sin UI):
```bash
curl -X POST \
  -H "Authorization: Bearer $NKR_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"op":"install","modules":["nkr_sso"]}' \
  "http://127.0.0.1:9090/api/v1/cells/$CELL/instances/$TENANT/modules"
```

### 10.8 Rotar el secret (`[nkr_sso] secret`)

```bash
#!/bin/bash
# rotate-sso-secret.sh — rota la HMAC key en [nkr_sso] secret del odoo.conf
set -euo pipefail

CELL="${1:?usage: $0 <cell> <tenant>}"
TENANT="${2:?usage: $0 <cell> <tenant>}"
CONF="/mnt/nkr/cells/$CELL/instances/$TENANT/config/odoo.conf"

NEW_SECRET=$(head -c 32 /dev/urandom | xxd -p -c 32)

python3 - "$CONF" "$NEW_SECRET" <<'PY'
import sys, re
conf, new = sys.argv[1], sys.argv[2]
lines = open(conf).read().splitlines()
out = []
for ln in lines:
    if re.match(r'^\s*nkr_sso_secret\s*=', ln):       # legacy en [options] → drop
        continue
    if re.match(r'^\s*secret\s*=', ln) and '[nkr_sso]' in '\n'.join(out):  # [nkr_sso] secret → replace
        out.append(f'secret = {new}'); continue
    out.append(ln)
if not any(l.strip() == '[nkr_sso]' for l in out):    # no había sección → crearla al final
    if out and out[-1].strip(): out.append('')
    out += ['[nkr_sso]', f'secret = {new}']
open(conf, 'w').write('\n'.join(out) + '\n')
PY
echo "✓ secret rotado en $CONF ([nkr_sso] secret)"

# Restart para que Odoo re-lea el conf + el módulo recoja el secret nuevo.
# (Para workers>0 un POST /reload basta; para workers=0 usar restart — ver
#  NKR_API.md §4.17.1 sobre el bug del reload en workers=0.)
. /etc/nkr/api.env
curl -fsS -X POST -H "Authorization: Bearer $NKR_API_TOKEN" -H "Content-Type: application/json" \
  -d '{"action":"restart"}' \
  "http://127.0.0.1:9090/api/v1/cells/$CELL/instances/$TENANT/actions"
echo "✓ tenant restarting — poll GET /instances/$TENANT hasta nkr_status.phase=ready"
echo "  Todas las URLs SSO emitidas previas son inválidas desde este momento."
```

### 10.9 Validación standalone del HMAC (debug local)

Si querés verificar la firma sin que intervenga Odoo (para debugear el módulo):

```python
# verify-sso.py
import hashlib, hmac, sys, time
from urllib.parse import urlparse, parse_qs

if len(sys.argv) != 3:
    print("usage: verify-sso.py '<url>' '<secret>'")
    sys.exit(1)

url, secret = sys.argv[1], sys.argv[2]
qs = parse_qs(urlparse(url).query)
u   = qs['u'][0]
exp = qs['exp'][0]
sig = qs['sig'][0]

payload = f"{u}|{exp}".encode()
expected = hmac.new(secret.encode(), payload, hashlib.sha256).hexdigest()

print(f"user:     {u}")
print(f"expires:  {exp} ({'expired' if int(exp) < time.time() else 'valid'})")
print(f"sig (recv):    {sig}")
print(f"sig (expected): {expected}")
print(f"match:    {hmac.compare_digest(sig, expected)}")
```

Uso:
```bash
SECRET=$(awk -F"= *" "/^\\[nkr_sso\\]/{s=1} s&&/^secret/{print $2; exit}" /mnt/nkr/cells/<cell>/instances/<tenant>/config/odoo.conf)
URL=$(curl -s -X POST -H "Authorization: Bearer $TOKEN" \
       $NKR_API/api/v1/cells/<cell>/instances/<tenant>/sso | jq -r .url)
python verify-sso.py "$URL" "$SECRET"
# →  match: True
```

### 10.10 Errores comunes y resoluciones

| Síntoma | Causa | Fix |
|---|---|---|
| 404 `/nkr-sso` en el tenant | Módulo no instalado | `POST /modules {"op":"install","modules":["nkr_sso"]}` |
| `sso_secret_missing` desde NKR | `nkr_sso_secret` no está en `odoo.conf` | Agregar (sección 3) + REL_OD |
| `sso_bad_signature` redirect | Secret distinto entre NKR y módulo | Verificar `odoo.conf` con el comando 10.9. Si no coinciden, rotar (10.8) |
| `sso_expired` | El click pasó >30s después de generar la URL | Generar URL nueva en el momento del click (no pre-generar y guardar) |
| `sso_user_not_found` | Login mal escrito o user `active=False` | Verificar via psql: `SELECT login, active FROM res_users WHERE login='X'` |
| Browser dice "site can't be reached" | DNS no propagado / cert no emitido | Comprobar `POST /dns` se completó OK |
| Botón del panel muestra "loading" eterno | NKR no responde / token inválido | Mirar Network tab del browser, ver el body de error del POST /sso |

### 10.11 Audit log

NKR loguea cada SSO emitido en su journal:
```
[API] sso(odoo-v19-intech-devp, user=admin): URL emitida (TTL 30s, HMAC-only)
```

El módulo Odoo loguea cada SSO consumido (en `odoo.log` del tenant):
```
nkr_sso: login OK uid=2 login=admin db=db-odoo-v19-intech-devp
```

Si querés grabar **quién** del panel disparó cada SSO, agrega un campo `actor` al body:
```bash
curl ... -d '{"user":"admin", "actor":"dev:juan@empresa.com"}' ...
```

NKR actualmente ignora ese campo pero lo loguea en debug. Para que NKR lo persista en audit log puede agregarse en una versión futura (~10 LOC).

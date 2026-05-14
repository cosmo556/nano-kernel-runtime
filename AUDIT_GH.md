# Auditoría: addons privados via submódulos GitHub (jerarquía profunda → layout plano)

**Estado:** análisis técnico, plan de implementación pendiente de aprobar.
**Riesgo abordado:** ninguno crítico — feature, no fix de bug.
**Motivación:** el panel manda **una sola URL de meta-repo** con submódulos privados (que pueden tener más submódulos adentro, jerarquía arbitraria padre/hijo/nieto). NKR clona recursivo, escanea TODO el árbol resultante, y deja **todos los módulos Odoo aplanados en una sola carpeta** `addons/`, sin importar la profundidad de Git de la que vengan.

---

## TL;DR

- En GitHub: jerarquía arbitraria (padre con submódulos, esos submódulos con más submódulos, etc.).
- En NKR: **layout plano absoluto** en `/mnt/extra-addons/`. Todo módulo termina como `addons/<nombre>/`, sin subdirs intermedios.
- NKR detecta módulos buscando `__manifest__.py` en el árbol del clone, **a profundidad arbitraria**. No hay listas de inclusión/exclusión: si tiene manifest, es módulo y se publica.
- **Cada deploy es re-clone completo**, no `git pull` incremental. Aceptamos ~15-30 s de tráfico contra GitHub a cambio de eliminar la complejidad de mantener un `.git` activo en `addons/`.
- **Colisión de nombres = error duro `409 module_name_collision`** que el panel debe mostrar como deploy fallido (rojo / dropped / failed). Cliente debe renombrar uno de los módulos en su árbol Git y re-mandar.
- Auth con un solo PAT fine-grained del owner que tenga acceso al meta-repo + todos los descendientes. NKR reescribe URLs SSH y HTTPS con `url.insteadOf` para que el token aplique recursivamente.

---

## Tabla de contenidos

- [1. Estado actual del código](#1-estado-actual-del-código)
- [2. Arquitectura del feature](#2-arquitectura-del-feature)
- [3. Flujo end-to-end con ejemplo de jerarquía profunda](#3-flujo-end-to-end-con-ejemplo-de-jerarquía-profunda)
- [4. Cambios concretos en NKR](#4-cambios-concretos-en-nkr)
- [5. Manejo de errores y códigos de respuesta](#5-manejo-de-errores-y-códigos-de-respuesta)
- [6. Autenticación](#6-autenticación)
- [7. Casos edge y limitaciones](#7-casos-edge-y-limitaciones)
- [8. Plan de implementación](#8-plan-de-implementación)
- [9. Tests](#9-tests)
- [10. Documentación operativa para el cliente](#10-documentación-operativa-para-el-cliente)
- [11. Decisiones tomadas](#11-decisiones-tomadas)
- [12. Iteración futura (no incluido en esta entrega)](#12-iteración-futura-no-incluido-en-esta-entrega)
- [13. Bitácora de revisiones](#13-bitácora-de-revisiones)

---

## 1. Estado actual del código

`POST /api/v1/cells/{cell}/instances/{nkr_name}/addons/git` ([bin/nkr_api_server.rs:1064](src/bin/nkr_api_server.rs#L1064)) ejecuta:

```bash
git -c core.hooksPath=/dev/null \
    -c protocol.allow=user \
    clone --depth 1 --branch <ref> -- <url> <target>
```

**Sin `--recurse-submodules`.** Si el repo del cliente tiene `.gitmodules`, después del clone los directorios de submódulos quedan vacíos en el filesystem. NKR loguea `clone OK` y devuelve 200; Odoo dentro del guest no encuentra `__manifest__.py` en esos paths y los ignora silenciosamente. **Fallo silencioso en producción.**

El endpoint actual ya tiene una arquitectura útil que se reusa para este feature:

```rust
// bin/nkr_api_server.rs:613-640 (resumen)
let tmp_target = format!("{}/.nkr-tmp-{}", addons_dir, subdir);
let _ = std::fs::remove_dir_all(&tmp_target);     // cleanup tmp si quedó basura
run_git_sync(&clone_req, &tmp_target);            // clone al tmp
explode_modules(&tmp_target, &addons_dir, ...);   // scanea + mueve módulos
let _ = std::fs::remove_dir_all(&tmp_target);     // cleanup SIEMPRE
```

`explode_modules` ya hace el scan + rename a `addons/<nombre>/`, **pero solo a un nivel de profundidad** (subdirs directos del tmp). Para soportar jerarquías padre/hijo/nieto hay que extenderlo a recursivo.

---

## 2. Arquitectura del feature

### Quién hace el trabajo

**NKR** hace el clone recursivo y el aplanado. El panel solo manda URL del meta-repo + PAT.

Razones:
1. El panel ya manda solo `repo_url + auth`. Pedirle que enumere submódulos lo obliga a parsear `.gitmodules`, manejar auth recursivamente, etc. — duplicación.
2. Git ya hace el clone recursivo nativo con `--recurse-submodules`. Reimplementarlo en cualquier capa sobre Git (panel, NKR) es código bug-prone.
3. NKR ya gestiona el dir destino, la auth, el scrub de PAT en logs y la atomicidad del swap (rename + cleanup).

### Garantía operativa

- **Cero `git pull` incremental.** Cada update completo: clone fresh → aplanar → swap.
- **Cero `.git` activo en `addons/`.** El destino final no tiene repositorio Git, solo dirs con código Odoo.
- **Cero subdirs intermedios.** `addons/module-a/__manifest__.py`, sin `addons/group-x/module-a/`.
- **Cero magia de inclusión/exclusión.** Todo dir con `__manifest__.py` se publica. Si el cliente no quiere algo, lo saca de su árbol Git.

---

## 3. Flujo end-to-end con ejemplo de jerarquía profunda

### Lo que el cliente arma en GitHub

```
acme-customs/                       ← repo padre (privado)
├── .gitmodules
├── module-direct/                  ← submódulo HIJO (1 módulo Odoo)
│   ├── .git                        (archivo, no dir — gitlink al padre)
│   ├── __manifest__.py             ← Odoo lo ve
│   └── models/
├── group-frontend/                 ← submódulo HIJO (NO módulo, agrupador)
│   ├── .git
│   ├── .gitmodules                 ← submódulos anidados adentro
│   ├── module-x/                   ← submódulo NIETO
│   │   ├── .git
│   │   └── __manifest__.py
│   └── module-y/                   ← submódulo NIETO
│       ├── .git
│       └── __manifest__.py
└── group-backend/                  ← submódulo HIJO agrupador
    ├── .git
    ├── .gitmodules
    └── module-z/                   ← submódulo NIETO
        ├── .git
        └── __manifest__.py
```

`.gitmodules` del padre:
```ini
[submodule "module-direct"]
    path = module-direct
    url = https://github.com/acme/module-direct.git

[submodule "group-frontend"]
    path = group-frontend
    url = https://github.com/acme/group-frontend.git

[submodule "group-backend"]
    path = group-backend
    url = https://github.com/acme/group-backend.git
```

`.gitmodules` de `group-frontend` (anidado):
```ini
[submodule "module-x"]
    path = module-x
    url = https://github.com/acme/module-x.git

[submodule "module-y"]
    path = module-y
    url = https://github.com/acme/module-y.git
```

`group-frontend` y `group-backend` **NO tienen `__manifest__.py` en su raíz**: son agrupadores, no módulos. NKR los ignora correctamente.

### Lo que NKR produce en `/mnt/nkr/cells/.../instances/<tenant>/addons/`

```
addons/
├── module-direct/__manifest__.py   ← venía de hijo
├── module-x/__manifest__.py        ← venía de nieto (group-frontend/module-x)
├── module-y/__manifest__.py        ← venía de nieto
└── module-z/__manifest__.py        ← venía de nieto (group-backend/module-z)
```

Notar que `group-frontend` y `group-backend` desaparecieron — eran solo dirs intermedios sin manifest. **Aplanado total.**

### Configuración de Odoo resultante

```
addons_path = /usr/lib/python3/dist-packages/odoo/addons,/mnt/extra-addons,/mnt/extra-enterprise
```

Sin subdirs adicionales. Odoo enumera `module-direct`, `module-x`, `module-y`, `module-z` en "Apps".

### Flujo paso a paso

```
panel → POST /addons/git { repo_url, github_token, ref }
   ↓
NKR proxy → daemon root (UDS)
   ↓
NKR crea tmp dir efímero: addons/.nkr-tmp-<id>
   ↓
git -c url.insteadOf=... clone --depth 1 --recurse-submodules --shallow-submodules
        --branch <ref> -- <repo_url> tmp/
   ↓
Git baja recursivo: padre + cada hijo + cada nieto + ... (profundidad arbitraria)
   ↓
NKR walk recursivo del tmp, busca todo dir con __manifest__.py
   ↓
NKR detecta colisiones: ¿dos módulos con mismo dirname en distintas ramas del árbol?
        → sí: 409 module_name_collision, abortar todo
        → no: continuar
   ↓
NKR detecta submódulos vacíos (auth falló, SHA inexistente)
        → sí: 422 submodule_clone_partial, abortar todo
        → no: continuar
   ↓
Para cada módulo encontrado:
   - fs::rename(tmp/<path>/<modulo>, addons/<modulo>/)
   - escribir addons/<modulo>/.nkr-source con repo_url + sha
   ↓
NKR borra tmp/ entero (incluido .git/, .gitmodules, dirs agrupadores).
   ↓
Respuesta 200 con la lista de módulos publicados + sha del padre.
```

---

## 4. Cambios concretos en NKR

| # | Item | Archivo:línea | Cambio | Esfuerzo |
|---|---|---|---|---|
| 1 | `git_clone` con `--recurse-submodules` + URL rewrite por PAT | [bin/nkr_api_server.rs:1064](src/bin/nkr_api_server.rs#L1064) | Aceptar `github_token: Option<&str>`. Inyectar `-c url."https://x-access-token:$PAT@github.com/".insteadOf=...` para SSH y HTTPS. Pasar `--recurse-submodules --shallow-submodules --jobs 2`. | 30 min |
| 2 | Scan recursivo en `explode_modules` | [bin/nkr_api_server.rs:664](src/bin/nkr_api_server.rs#L664) | Reemplazar el walk de "1 nivel" por un walk recursivo con `walkdir` o recursión manual. Buscar `__manifest__.py` a cualquier profundidad. Skipear dirs `.git/`, `.git_modules`, dotfiles. | 60 min |
| 3 | Detección de colisión de nombres | nuevo | Antes de mover nada, agrupar los `__manifest__.py` encontrados por basename del parent dir. Si hay duplicados → `409 module_name_collision` con la lista. | 30 min |
| 4 | Detección de submódulo vacío | nuevo | Tras el clone, leer `tmp/.gitmodules` y `tmp/<sub>/.gitmodules` recursivos. Para cada `path` declarado, verificar que el dir tenga ≥1 archivo regular. Si vacío → `422 submodule_clone_partial`. | 30 min |
| 5 | `git_pull` deprecation | [bin/nkr_api_server.rs:1078](src/bin/nkr_api_server.rs#L1078) | El flujo actual ya hace re-clone fresh ignorando `req.action`. No hay que deprecar nada — confirmar que sigue funcionando con `--recurse-submodules`. | 0 min |
| 6 | Cleanup del tmp en error path | ya existe | El `remove_dir_all(&tmp_target)` en línea 640 ya cubre todos los casos. Sin cambios. | 0 min |
| 7 | Tests unitarios del scan recursivo y collision detection | nuevo | Crear árboles dummy en `tempdir`, verificar que el scan encuentra módulos a 3+ niveles de profundidad y que las colisiones disparan 409. | 90 min |

**Total código**: ~3-4 horas.

### 4.1 `git_clone` con submódulos y URL rewrite

```rust
fn git_clone(
    url: &str,
    target: &str,
    reference: Option<&str>,
    key_path: Option<&str>,
    github_token: Option<&str>,            // ← nuevo
) -> (bool, String, String) {
    let mut cmd = std::process::Command::new("git");

    // Holders for the lifetime of the args slice (avoid dangling &str).
    let pat_ssh;
    let pat_https;

    let mut args: Vec<&str> = vec![
        "-c", "core.hooksPath=/dev/null",
        "-c", "protocol.allow=user",
    ];

    // If a PAT is present, rewrite SSH and bare-HTTPS URLs to embed the
    // token. This makes private submodules (and submodules-of-submodules)
    // under the same owner clone transparently with a single credential.
    if let Some(pat) = github_token {
        pat_ssh = format!(
            "url.https://x-access-token:{}@github.com/.insteadOf=git@github.com:",
            pat
        );
        pat_https = format!(
            "url.https://x-access-token:{}@github.com/.insteadOf=https://github.com/",
            pat
        );
        args.push("-c"); args.push(&pat_ssh);
        args.push("-c"); args.push(&pat_https);
    }

    args.extend_from_slice(&[
        "clone",
        "--depth", "1",
        "--recurse-submodules",         // ← clave
        "--shallow-submodules",         // ← --depth=1 para cada submódulo (recursivo)
        "--jobs", "2",                  // ← clones paralelos limitados, ver §7 sobre rate-limit
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
```

Nota técnica: los `-c key=value` que pasás a `git` se setean para esa invocación y **se propagan a sub-gits via `GIT_CONFIG_PARAMETERS` env var**. Eso significa que `core.hooksPath=/dev/null`, `protocol.allow=user` y los `url.insteadOf` aplican también a los clones recursivos de submódulos sin tener que re-pasarlos. Defensa-en-profundidad heredada gratis.

### 4.2 Scan recursivo + detección de colisión

```rust
/// Walks `tmp_dir` recursively looking for any directory whose entry list
/// includes `__manifest__.py`. Skips `.git/` directories and any dotfile
/// directory. Returns a vector of (absolute-path-of-module-dir, dirname)
/// pairs. Dirname is the basename of the module dir, which becomes the
/// publication name in `addons/<dirname>/`.
fn scan_modules_recursive(tmp_dir: &str) -> Result<Vec<(PathBuf, String)>, String> {
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    let root = Path::new(tmp_dir);
    walk_dir(root, &mut found)?;
    Ok(found)
}

fn walk_dir(dir: &Path, acc: &mut Vec<(PathBuf, String)>) -> Result<(), String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("read_dir {} failed: {}", dir.display(), e))?;
    let manifest = dir.join("__manifest__.py");
    if manifest.is_file() {
        // This dir IS a module. Don't descend further — Odoo modules
        // never contain other Odoo modules as subdirs (data/, models/,
        // wizard/, etc. are not modules).
        let name = dir.file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("invalid dir name at {}", dir.display()))?
            .to_string();
        acc.push((dir.to_path_buf(), name));
        return Ok(());
    }
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
        walk_dir(&path, acc)?;
    }
    Ok(())
}

/// Returns the conflicts as (name, [path1, path2, ...]) for any module name
/// that appears in more than one path of the tree.
fn detect_name_collisions(modules: &[(PathBuf, String)])
    -> Vec<(String, Vec<PathBuf>)>
{
    let mut by_name: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (path, name) in modules {
        by_name.entry(name.clone()).or_default().push(path.clone());
    }
    by_name.into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .collect()
}
```

### 4.3 Validación de submódulos no vacíos (post-clone, pre-mueve)

```rust
/// Reads `.gitmodules` files recursively (the parent's plus each
/// submodule's nested `.gitmodules` if present). For each declared submodule
/// path, verify the dir is non-empty. Returns the list of empty paths so
/// the caller can build a 422 response.
fn validate_submodules_populated(tmp_dir: &str) -> Result<(), Vec<String>> {
    let mut empty_paths: Vec<String> = Vec::new();
    walk_gitmodules(Path::new(tmp_dir), Path::new(tmp_dir), &mut empty_paths);
    if empty_paths.is_empty() { Ok(()) } else { Err(empty_paths) }
}

fn walk_gitmodules(root: &Path, current: &Path, empty: &mut Vec<String>) {
    let gm = current.join(".gitmodules");
    if !gm.is_file() { return; }
    let content = match fs::read_to_string(&gm) {
        Ok(s) => s,
        Err(_) => return,
    };
    // Minimal INI parser: extract `path = X` lines.
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("path") {
            // expected: "path = some/sub"
            let val = rest.trim_start_matches(|c: char| c == ' ' || c == '\t' || c == '=')
                .trim();
            if val.is_empty() { continue; }
            let sub_path = current.join(val);
            let count = fs::read_dir(&sub_path)
                .map(|rd| rd.flatten().count())
                .unwrap_or(0);
            if count == 0 {
                // Compute path relative to root for the error message.
                let rel = sub_path.strip_prefix(root)
                    .unwrap_or(&sub_path)
                    .to_string_lossy().into_owned();
                empty.push(rel);
            } else {
                // Recurse into the submodule's own .gitmodules.
                walk_gitmodules(root, &sub_path, empty);
            }
        }
    }
}
```

### 4.4 Mover los módulos al destino final

Reemplazo de la lógica actual de `explode_modules` para soportar el árbol recursivo:

```rust
fn explode_modules_recursive(
    tmp_dir: &str,
    addons_dir: &str,
    repo_url: &str,
    reference: Option<&str>,
    sha: &str,
) -> Result<Vec<String>, HttpResponse> {
    // 1. Validate submodules are all populated.
    if let Err(empty) = validate_submodules_populated(tmp_dir) {
        return Err(HttpResponse::json(422, serde_json::json!({
            "error": "submodule_clone_partial",
            "message": format!(
                "{} submódulo(s) no se clonaron — verificar scope del PAT y \
                 que cada repo declarado en .gitmodules sea accesible.",
                empty.len()
            ),
            "failed_submodules": empty,
            "remediation": "Confirmar que el PAT tiene Contents:Read sobre \
                            todos los repos del árbol y reintentar el POST.",
        })));
    }

    // 2. Recursive scan for __manifest__.py.
    let modules = scan_modules_recursive(tmp_dir)
        .map_err(|e| HttpResponse::error(500, "scan_failed", Some(&e)))?;

    if modules.is_empty() {
        return Err(HttpResponse::error(422, "no_modules_found",
            Some("el árbol clonado no contiene ningún __manifest__.py")));
    }

    // 3. Detect name collisions BEFORE moving anything.
    let collisions = detect_name_collisions(&modules);
    if !collisions.is_empty() {
        // Build a structured payload so the panel can render exactly which
        // module name is duplicated and where.
        let conflicts: Vec<serde_json::Value> = collisions.iter()
            .map(|(name, paths)| serde_json::json!({
                "module_name": name,
                "found_at": paths.iter()
                    .map(|p| p.strip_prefix(tmp_dir)
                        .unwrap_or(p).to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            }))
            .collect();
        return Err(HttpResponse::json(409, serde_json::json!({
            "error": "module_name_collision",
            "message": "dos o más módulos con el mismo nombre fueron \
                        encontrados en distintas ramas del árbol Git. \
                        Renombrar uno de ellos en el repo y re-mandar.",
            "conflicts": conflicts,
            "remediation": "Renombrar el directorio del módulo en uno \
                            de los repos en conflicto y commit + push. \
                            NKR no eligió un ganador automáticamente \
                            para evitar perder código silenciosamente.",
        })));
    }

    // 4. Conflict check vs already-existing modules from another repo.
    //    (This is the same `.nkr-source` mechanism the current
    //    explode_modules already uses; reused as-is.)
    // ... [code idéntico al actual, reusable sin cambios]

    // 5. Move each module to addons/<name>/. fs::rename is atomic on the
    //    same filesystem. Each rename is a single syscall.
    let mut published: Vec<String> = Vec::new();
    for (src_path, name) in &modules {
        let dst = format!("{}/{}", addons_dir, name);
        let _ = fs::remove_dir_all(&dst);
        if let Err(e) = fs::rename(src_path, &dst) {
            return Err(HttpResponse::error(500, "move_failed",
                Some(&format!("rename {} → {}: {}", src_path.display(), dst, e))));
        }
        write_nkr_source(&dst, repo_url, reference, sha);
        published.push(name.clone());
    }
    Ok(published)
}
```

### 4.5 Body del endpoint sin cambios visibles

El body actual ya soporta `github_token`. **Nada nuevo en la API HTTP** desde el punto de vista del panel:

```json
{
  "repo_url": "https://github.com/acme/customs.git",
  "ref": "main",
  "github_token": "ghp_..."
}
```

NKR lo recibe y lo propaga. El panel no necesita cambiar nada para empezar a usar submódulos — solo armar correctamente su meta-repo.

### 4.6 No-atomicidad multi-module: caveat preexistente

`explode_modules_recursive` itera N renames consecutivos. **Si el daemon muere en mitad del loop**, queda con N₁ módulos del clone nuevo y N₂ módulos del clone viejo (los que aún no se renombraron, que siguen apuntando al snapshot anterior).

**Esta no-atomicidad ya existe en el endpoint actual** (no es introducida por submódulos). Documentado como deuda conocida; mitigación posible en una iteración futura usando un swap atómico de un dir completo:

```
addons.next/    ← preparar acá todos los renames
mv addons addons.old && mv addons.next addons   ← rename atómico de DOS dirs (no soportado por kernel)
```

Linux **no soporta** rename atómico de dos paths simultáneamente. La alternativa (`renameat2(RENAME_EXCHANGE)`) intercambia dos paths atómicamente y sí está disponible desde kernel 3.15. Aplicarlo requiere refactor del flujo entero — fuera de scope de este feature.

**Riesgo real en producción**: el daemon NKR no muere mid-rename salvo SIGKILL externo o panic. Con `panic = "abort"` un panic mata el proceso. Mitigación: monitoring del exit code de `nkr.service`; si murió mid-deploy, el operador re-manda el POST y el flujo idempotente repara el estado.

---

## 5. Manejo de errores y códigos de respuesta

| HTTP | Slug | Cuándo | Qué espera ver el panel | Acción del operador |
|---|---|---|---|---|
| 200 | (success) | Todo OK, módulos publicados. | Lista de módulos publicados, sha del padre. | Ninguna. Reiniciar Odoo si corresponde. |
| 401 | `git_auth_required` | PAT inválido o ausente sobre HTTPS. | Mensaje + `repo_url` + `ref`. | Verificar PAT y scope. |
| 401 | `git_ssh_auth_failed` | Deploy key no autorizada en algún repo del árbol. | Mensaje. | Agregar la key como deploy key en el repo afectado o cambiar a PAT. |
| 404 | `git_repo_not_found` | Repo inexistente o PAT sin scope sobre él. | `repo_url`, `ref`. | Verificar URL y scope del PAT. |
| 409 | **`module_name_collision`** | Dos `__manifest__.py` con mismo dirname encontrados en distintas ramas del árbol. | `conflicts: [{ module_name, found_at: [...] }]` + `remediation`. | Renombrar uno de los dirs en conflicto en su repo, commit + push, re-mandar el POST. **El panel debe mostrar este deploy como FAILED / DROPPED / ROJO en su UI** — NKR no eligió un ganador para evitar pérdida silenciosa de código. |
| 422 | `submodule_clone_partial` | Algún submódulo del árbol quedó vacío (PAT sin scope sobre ese repo, SHA inexistente por force-push, owner distinto, etc.). | `failed_submodules: ["path/relativo/al/sub", ...]` + `remediation`. | Corregir el scope del PAT o autorizar el repo, re-mandar. NKR no aplicó cambios al filesystem destino. |
| 422 | `no_modules_found` | El árbol clonado no contiene ningún `__manifest__.py`. | Mensaje. | Verificar que el repo es realmente un meta-repo de addons Odoo. |
| 500 | `move_failed` | `fs::rename` falló (espacio en disco, permisos, fs corrupto). | Path src/dst y `errno`. | Operacional — revisar host. |
| 500 | `scan_failed` | `read_dir` falló durante el walk. | Path donde falló. | Operacional. |
| 504 | `git_timeout` | Git tardó más de `GIT_TIMEOUT_S` (default 180s). | Mensaje. | Repo/red lentos. Reintentar o revisar conectividad host. |

### 5.1 Detalle del payload `module_name_collision` (409)

Ejemplo: cliente tiene `module-x` en `group-frontend/` Y en `group-backend/` (mismo dirname, distintos repos):

```json
HTTP 409
{
  "error": "module_name_collision",
  "message": "dos o más módulos con el mismo nombre fueron encontrados en distintas ramas del árbol Git. Renombrar uno de ellos en el repo y re-mandar.",
  "conflicts": [
    {
      "module_name": "module-x",
      "found_at": ["group-frontend/module-x", "group-backend/module-x"]
    }
  ],
  "remediation": "Renombrar el directorio del módulo en uno de los repos en conflicto y commit + push. NKR no eligió un ganador automáticamente para evitar perder código silenciosamente.",
  "repo_url": "https://github.com/acme/customs.git",
  "ref": "main"
}
```

El panel debe:
- Mostrar el deploy como **FAILED** en la UI del operador (color rojo / status `dropped` / equivalente).
- Listar los módulos en conflicto con el path Git completo de cada uno.
- Presentar el `remediation` al operador.
- **No reintentar automáticamente** — el cliente tiene que arreglar el árbol Git primero.

### 5.2 Detalle del payload `submodule_clone_partial` (422)

```json
HTTP 422
{
  "error": "submodule_clone_partial",
  "message": "2 submódulo(s) no se clonaron — verificar scope del PAT y que cada repo declarado en .gitmodules sea accesible.",
  "failed_submodules": [
    "group-frontend/module-x",
    "group-backend/module-z"
  ],
  "remediation": "Confirmar que el PAT tiene Contents:Read sobre todos los repos del árbol y reintentar el POST."
}
```

NKR **no toca el filesystem destino** cuando hay fallo parcial — la VM Odoo del tenant sigue corriendo con la versión anterior. Re-mandar con PAT corregido aplica el árbol nuevo completo.

---

## 6. Autenticación

### Caso recomendado: un solo PAT del owner

Si todos los repos (meta + hijos + nietos + bisnietos) están bajo el mismo owner GitHub, **un solo PAT cubre todo**:

1. Crear PAT en https://github.com/settings/personal-access-tokens (fine-grained):
    - **Repository access**: "Only select repositories" → seleccionar meta-repo + cada repo de submódulo a cualquier profundidad.
    - **Permissions**: `Contents: Read-only`, `Metadata: Read-only`. Nada más.
    - **Expiration**: 90 días o el plazo que tu compliance permita.
2. Panel manda el PAT como `github_token` en `POST /addons/git`.

### Cómo NKR aplica el PAT recursivamente

Los `.gitmodules` (del padre o de submódulos anidados) típicamente declaran URLs en formato SSH (`git@github.com:acme/...`) o HTTPS sin auth. NKR usa `url.<base>.insteadOf` para reescribir URLs en runtime sin tocar los `.gitmodules` del cliente:

```bash
git -c url.https://x-access-token:$PAT@github.com/.insteadOf=git@github.com: \
    -c url.https://x-access-token:$PAT@github.com/.insteadOf=https://github.com/ \
    clone --recurse-submodules ...
```

Estos `-c` se propagan al sub-git via `GIT_CONFIG_PARAMETERS` env var. Aplican a los clones recursivos a cualquier profundidad. **El cliente no toca su `.gitmodules`** — NKR hace la reescritura transparente.

### Caso no soportado: submódulos en owners distintos

Si tu meta-repo `acme/customs` referencia un submódulo `mi-otra-org/lib` privado, el PAT de `acme` **no autentica** repos privados de `mi-otra-org` salvo que el user sea collaborator allá.

SSH deploy keys tampoco resuelven (Git solo permite una key activa por sesión).

**Recomendación**: mantener todo el árbol bajo un mismo owner. Si tenés que cruzar owners, no se soporta en esta entrega.

### Scrub del PAT en logs

[bin/nkr_api_server.rs:1280](src/bin/nkr_api_server.rs#L1280) ya tiene `scrub_url_credentials` que reemplaza `https://x-access-token:PAT@host/...` → `https://***:***@host/...`. **Aplica también a los `-c url.insteadOf` echoed en stderr de git** porque cualquier mensaje que git imprima va por la misma función.

Sin cambios adicionales necesarios en este feature.

---

## 7. Casos edge y limitaciones

| Caso | Comportamiento |
|---|---|
| Submódulo apunta a un SHA que ya no existe (force-push en el repo del submódulo) | Git falla con "fatal: reference is not a tree". NKR retorna 422 con el path del submódulo y el log de git. |
| Cliente añade un submódulo nuevo al meta-repo | El siguiente `POST /addons/git` clona recursivo y publica el nuevo módulo automáticamente. Funciona limpio. |
| Cliente quita un submódulo del meta-repo | El módulo desaparece del clone fresco. El módulo viejo en `addons/<m>/` queda en disco hasta que algo lo borre. **Mitigación**: `addons/<m>/.nkr-source` registra el repo origen — un endpoint futuro `POST /addons/cleanup` podría barrer módulos cuyo `repo_url` ya no aparece en el deploy actual. **Fuera de scope de esta entrega** (zombies son cosméticos: Odoo los carga como módulos no instalados, no afectan tenants reales). |
| Submódulo del cliente tiene `.gitmodules` propio (anidado, nietos) | `--recurse-submodules` lo maneja recursivamente. NKR escanea desde la raíz del tmp y encuentra los nietos a profundidad arbitraria. |
| Submódulo apunta a otro proveedor (GitLab, Gitea) | El `url.insteadOf` solo cubre `github.com`. Si un submódulo está en `gitlab.com`, NKR no autentica esa URL. **No se soporta**. Cliente debe espejar todo en GitHub o cambiar el feature en una iteración futura para soportar multi-host. |
| Submódulo es público | Funciona sin PAT. El `url.insteadOf` solo aplica si el PAT existe; sin PAT, las URLs originales se usan tal cual. |
| Token expirado | GitHub devuelve 401 "Bad credentials". NKR lo clasifica como `git_auth_required` (genérico). Distinguir `git_token_expired` queda como mejora futura — ver §12. |
| Repo padre clonado OK pero un nieto falla la auth | `validate_submodules_populated` lo detecta: ese subdir queda vacío. NKR retorna 422 con el path del submódulo afectado. **No aplica nada al filesystem destino.** |
| GitHub rate-limit (5000 req/h por PAT) | Cada `clone --depth 1` ≈ 1-2 requests. 20 submódulos × 50 deploys/hora = 2000 requests; 100 deploys/hora con muchos submódulos puede acercarse al límite. Mitigación: `--depth 1` mantenido religiosamente; `--jobs 2` no aumenta requests, solo paraleliza. |
| Concurrent POSTs sobre el mismo tenant | El endpoint actual NO tiene lock — dos POSTs podrían ejecutarse simultáneamente y el segundo `remove_dir_all(&tmp_target)` podría barrer el clone del primero. **Deuda preexistente**, no introducida por este feature. Mitigación: el panel no debe disparar deploys concurrentes sobre el mismo tenant (si lo necesita, agregar inflight set como en `handle_action`). |

---

## 8. Plan de implementación

### Fase 1 — Código + tests (medio día)

1. Modificar `git_clone` para aceptar `github_token` y añadir flags + URL rewrite.
2. Implementar `scan_modules_recursive` y `walk_dir`.
3. Implementar `detect_name_collisions`.
4. Implementar `validate_submodules_populated` (recursivo a través de `.gitmodules` anidados).
5. Refactorizar `explode_modules` → `explode_modules_recursive` con la nueva lógica.
6. Tests unitarios para los 4 helpers (árboles dummy en `tempdir`).

### Fase 2 — Validación con un repo real (medio día)

1. Cliente crea meta-repo de prueba con jerarquía padre/hijo/nieto:
   - Padre `acme-test-customs` con 1 submódulo módulo (`module-direct`) y 1 submódulo agrupador (`group-frontend`).
   - `group-frontend` con 2 submódulos módulo (`module-x`, `module-y`).
2. Panel manda `POST /addons/git` con PAT.
3. Verificar dentro del guest:
   - `ls /mnt/extra-addons/` muestra `module-direct`, `module-x`, `module-y` planos.
   - `ls /mnt/extra-addons/group-frontend` retorna "no such file" (el agrupador desapareció correctamente).
   - Odoo reload: encuentra los 3 módulos.
4. Provocar colisión deliberada (renombrar un módulo a uno ya existente) y verificar `409 module_name_collision`.
5. Provocar fallo parcial (sacar permisos del PAT a un nieto) y verificar `422 submodule_clone_partial`.

### Tiempo total

| Fase | Tiempo |
|---|---|
| 1 — código + tests | 4-5 horas |
| 2 — validación | 2-3 horas |
| **Total** | **~1 día** |

---

## 9. Tests

```rust
#[test]
fn scan_finds_modules_at_arbitrary_depth() {
    let tmp = tempdir();
    // Layout:
    //   tmp/module-direct/__manifest__.py     (depth 1)
    //   tmp/group-frontend/module-x/__manifest__.py  (depth 2)
    //   tmp/group-frontend/sub/module-z/__manifest__.py  (depth 3)
    fs::create_dir_all(tmp.join("module-direct")).unwrap();
    fs::write(tmp.join("module-direct/__manifest__.py"), "{}").unwrap();
    fs::create_dir_all(tmp.join("group-frontend/module-x")).unwrap();
    fs::write(tmp.join("group-frontend/module-x/__manifest__.py"), "{}").unwrap();
    fs::create_dir_all(tmp.join("group-frontend/sub/module-z")).unwrap();
    fs::write(tmp.join("group-frontend/sub/module-z/__manifest__.py"), "{}").unwrap();

    let found = scan_modules_recursive(&tmp.to_string_lossy()).unwrap();
    let names: HashSet<_> = found.iter().map(|(_, n)| n.clone()).collect();
    assert_eq!(names.len(), 3);
    assert!(names.contains("module-direct"));
    assert!(names.contains("module-x"));
    assert!(names.contains("module-z"));
}

#[test]
fn scan_skips_dotfile_dirs() {
    let tmp = tempdir();
    fs::create_dir_all(tmp.join(".git/modules/foo")).unwrap();
    fs::write(tmp.join(".git/modules/foo/__manifest__.py"), "{}").unwrap();
    fs::create_dir_all(tmp.join("good-module")).unwrap();
    fs::write(tmp.join("good-module/__manifest__.py"), "{}").unwrap();

    let found = scan_modules_recursive(&tmp.to_string_lossy()).unwrap();
    let names: Vec<_> = found.iter().map(|(_, n)| n.clone()).collect();
    assert_eq!(names, vec!["good-module".to_string()]);
}

#[test]
fn scan_does_not_descend_into_modules() {
    // A module dir contains data/, models/, wizard/ — those are NOT modules,
    // they are subdirs of a single module. Scan must stop at the first
    // __manifest__.py found in the chain.
    let tmp = tempdir();
    fs::create_dir_all(tmp.join("module-a/models")).unwrap();
    fs::write(tmp.join("module-a/__manifest__.py"), "{}").unwrap();
    fs::write(tmp.join("module-a/models/__init__.py"), "").unwrap();

    let found = scan_modules_recursive(&tmp.to_string_lossy()).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].1, "module-a");
}

#[test]
fn detect_collisions_flags_duplicates() {
    let mods = vec![
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
fn validate_submodules_detects_empty_dir() {
    let tmp = tempdir();
    fs::write(tmp.join(".gitmodules"), r#"
[submodule "module-a"]
    path = module-a
    url = https://github.com/acme/module-a.git
[submodule "module-b"]
    path = module-b
    url = https://github.com/acme/module-b.git
"#).unwrap();
    fs::create_dir_all(tmp.join("module-a")).unwrap();
    fs::write(tmp.join("module-a/__manifest__.py"), "{}").unwrap();
    fs::create_dir_all(tmp.join("module-b")).unwrap();
    // module-b queda vacío

    let res = validate_submodules_populated(&tmp.to_string_lossy());
    assert!(res.is_err());
    let empty = res.unwrap_err();
    assert!(empty.iter().any(|p| p.contains("module-b")));
}

#[test]
fn validate_submodules_recurses_into_nested_gitmodules() {
    let tmp = tempdir();
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
    fs::create_dir_all(tmp.join("group-frontend/nested-module")).unwrap();
    // nested-module queda vacío

    let res = validate_submodules_populated(&tmp.to_string_lossy());
    assert!(res.is_err());
    let empty = res.unwrap_err();
    assert!(empty.iter().any(|p| p.contains("nested-module")));
}
```

---

## 10. Documentación operativa para el cliente

Texto a agregar en `NKR_API.md` §4.10 (Git addons):

> ### Submódulos privados con jerarquía profunda
>
> NKR clona recursivo el meta-repo (incluyendo submódulos anidados a cualquier profundidad) y publica todos los módulos Odoo que encuentre **planos en `addons/`**, sin importar de qué nivel del árbol Git vinieron.
>
> **Convención obligatoria del meta-repo:**
>
> 1. Cada submódulo que sea un módulo Odoo debe tener `__manifest__.py` en su raíz.
> 2. Los nombres de directorio de los módulos deben ser **únicos en todo el árbol**. Si dos repos distintos tienen un módulo llamado `module-x`, NKR rechaza el deploy con `409 module_name_collision`.
> 3. Submódulos que NO son módulos Odoo (agrupadores tipo `group-frontend/`) son ignorados — NKR los recorre buscando módulos adentro pero no los publica como tal.
>
> **Auth:** un solo PAT fine-grained con `Contents: Read-only` sobre todos los repos del árbol. NKR usa `url.insteadOf` para que el token aplique recursivamente.
>
> **Cross-owner GitHub no se soporta**: todos los repos del árbol deben estar bajo un mismo owner.
>
> **Errores comunes:**
> - `409 module_name_collision`: dos módulos con mismo nombre. Renombrar uno y re-push.
> - `422 submodule_clone_partial`: PAT no tiene scope sobre algún repo del árbol. Corregir y re-mandar.

---

## 11. Decisiones tomadas

- **Layout absolutamente plano en `addons/`** — el cliente arma jerarquía profunda en GitHub si quiere, NKR la aplana.
- **Re-clone completo cada update** — sin `git pull` incremental. Coherente con el endpoint actual.
- **No hay `.git` en el destino final** — `addons/` es un dir normal sin repositorio Git activo.
- **Sin filtros de inclusión/exclusión.** Todo dir con `__manifest__.py` se publica. Si el cliente no quiere algo, lo saca de su árbol Git.
- **Colisión de nombres es error duro `409`** — NKR no elige un ganador para evitar pérdida silenciosa de código. El panel debe marcar el deploy como **failed/dropped/rojo** en su UI.
- **Auth recomendada: PAT fine-grained único** del owner. Cubre meta-repo + todos los descendientes via `url.insteadOf`.
- **Cross-owner no se soporta** en esta entrega.
- **Scrub del PAT en logs** ya está cubierto por `scrub_url_credentials`.
- **`--shallow-submodules`** activado por default (`--depth 1` también para cada submódulo).
- **`--jobs 2`** como default conservador. Override via env var `NKR_GIT_SUBMODULE_JOBS` para hosts con clientes grandes.

---

## 12. Iteración futura (no incluido en esta entrega)

Funcionalidades que vale tener mapeadas pero no se implementan ahora:

- **Slug `git_token_expired`** distinto de `git_auth_required`. Permitiría al panel diferenciar "token mal" de "token expirado" y disparar alertas específicas. Implementación: parsear stderr de git buscando "Bad credentials" + estado 401 → slug específico.
- **Endpoint `POST /addons/git/check-token`**: valida un PAT contra una URL de repo sin clonar (usa `git ls-remote --exit-code`, <1s). Útil para que el panel valide tokens al guardarlos.
- **Auto-detect de `requirements.txt`**: NKR escanea el árbol clonado, junta todos los `requirements.txt` que encuentre, dedupea y aplica `pip install` automáticamente. Por ahora el cliente debe consolidar manualmente y mandar al endpoint `PUT /pylibs`.
- **Endpoint `POST /addons/cleanup`** que barre módulos en `addons/` cuyo `repo_url` ya no aparece en el deploy actual. Resuelve el caso de zombies cuando el cliente quita un submódulo del meta-repo.
- **Multi-host (GitLab, Gitea, self-hosted)**: extender `url.insteadOf` para más bases. Requiere el panel pase el host en el body.
- **Inflight lock por `(cell, instance)`** para `POST /addons/git`: prevenir POSTs concurrentes sobre el mismo tenant. Mismo patrón que `handle_action`/`handle_delete` async.
- **Atomicidad multi-module via `renameat2(RENAME_EXCHANGE)`**: prepar todos los renames en `addons.next/` y swap atómico con `addons/`. Cierra la ventana de inconsistencia si el daemon muere mid-rename.

---

## 13. Bitácora de revisiones

| Fecha | Cambio | Origen |
|---|---|---|
| 2026-05-08 | Versión inicial. Análisis del soporte de submódulos GitHub privados, énfasis en auth via PAT del owner. | Pedido directo del operador. |
| 2026-05-08 | **Revisión 2**: cinco refinamientos. Eliminada heurística por path. Auditoría de nginx ampliada. "Riesgo cero" Fase 1 → "bajo riesgo". Plan B (per-cell reflink) agregado. | Crítica del operador. |
| 2026-05-08 | **Revisión 3**: re-diseño completo. Aclarado que el cliente quiere **jerarquía arbitraria padre/hijo/nieto en GitHub** + **layout absolutamente plano en NKR**. Eliminadas todas las secciones del enfoque anterior (git pull incremental, git clean -ffd, submodule update post-pull): no aplican porque el destino final no tiene Git activo. Mantenido auth con PAT, scrub, atomicidad de single rename. Agregado scan recursivo a profundidad arbitraria con detección de colisión 409 y validación de submódulos vacíos 422. Documentación de errores específicos para que el panel marque deploys fallidos como rojos/dropped/failed. | Confirmación explícita del operador con ejemplo concreto: padre/hijo/nieto en GitHub → todo plano en una sola carpeta `addons/` en NKR, sin escaleras. Decisiones B.1 (colisión = error duro) y C.1 (todo dir con manifest = módulo, sin filtros). |

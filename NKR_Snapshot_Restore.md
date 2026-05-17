# NKR Snapshot/Restore — análisis y plan de implementación

**Estado:** propuesta de arquitectura, no implementado. Doc para tomar la decisión.
**Fecha:** 2026-05-17
**Versión NKR de referencia:** 1.6.9

**Historial de revisión:**
- **v1 (2026-05-17)** — propuesta inicial.
- **v2 (2026-05-17, primera rueda de revisión)** — incorpora feedback del equipo (sección §10) sobre tres puntos ciegos críticos: (1) refcount de CPython rompe el sharing CoW del heap, (2) virtio-fs en snapshot causa pánico al restore, (3) la matemática real de densidad descuenta footprint de Postgres con 100 DBs activas. Números corregidos en TL;DR, §3, §4, §6, §7, §8.
- **v3 (2026-05-17, segunda rueda de revisión — sección §11)** — el equipo recordó que **KSM ya está descartado en NKR por decisión arquitectónica previa** (compatibilidad/estabilidad — confirmado en `CLAUDE.md`: *"KSM es una mentira"*). La v2 lo había revivido como mitigación mandatoria; v3 lo elimina de toda la propuesta. Sin KSM, la matemática de densidad colapsa: target real **60–75 Odoos activos** (no 80–110), techo absoluto ~75 con margen cero. El proyecto sigue, pero la justificación principal pasa de "densidad" a **velocidad de boot (5 s → 1.5–3 s)** y eliminación del boot storm. **Luz verde a Fase 0 con misión alterada:** medir el sangrado de CoW post-restore bajo carga HTTP real (100 reqs concurrentes × 10 min), no medir sharing teórico.

---

## TL;DR

- **Pregunta original:** ¿podemos eliminar los ~120 MB de overhead de RAM que cada nano-VM gasta solo por tener kernel + initramfs + estructuras de kernel?
- **Respuesta corta:** sí, vía **snapshot/restore con memoria CoW compartida entre VMs** (técnica popularizada por AWS Lambda / Firecracker, pero la implementación es 100 % `ioctl(KVM_*)` directos — no necesitamos Firecracker como dependencia).
- **Ahorro estimado (v3, post-2ª revisión, SIN KSM):** **40–80 MB de RAM compartibles por VM** vía CoW de kernel + libs read-only + bytecode immortal. **El heap de Python NO se comparte** tras el primer request por el refcount de CPython, y **sin KSM (descartado en NKR) nadie viene a re-fusionar páginas refcount-divergidas con contenido idéntico.** RSS efectivo por VM: **~100 MB al restore, estabiliza en ~250–300 MB tras horas de tráfico** (vs ~400–500 MB hoy con boot frío). Mejora real: ~30–40 %, no 50 % como decía v2.
- **Tiempo de boot:** baja de **~5 s a ~1.5–3 s** (sin cambio vs v2 — el snapshot core-only obliga a cargar el module registry post-restore para no romper virtio-fs).
- **Densidad esperada en 32 GB (v3, post-2ª revisión):** **~60–75 Odoos activos** (vs ~22 hoy). **La meta del whitepaper (100 Odoos) NO es alcanzable en este hardware sin KSM** — para llegar a 100 requiere host de 64 GB. v3 reposicionado: el proyecto se justifica por velocidad de boot + eliminación de boot storm + mejora modesta de densidad (3–3.5×), no por la promesa de "100 Odoos en 32 GB".
- **Costo de implementación:** **~3–4 semanas de un ingeniero senior**. Refactor de `vmm.rs` + `compose.rs` + nuevo módulo `snapshot.rs` + `crates/nkr-init-agent`. Cero deps nuevas.
- **Riesgo (v3):** alto. Tres focos: (1) virtio-fs DEBE quedar fuera del snapshot — cualquier intento de incluirlo crashea el guest; (2) **el sangrado de CoW post-restore por refcount es irreversible sin KSM** — Fase 0 mide qué tan rápido y profundo es ese sangrado bajo carga real; si el RSS se dispara de vuelta a 450 MB en <10 minutos bajo tráfico, el snapshot solo acelera el arranque pero no ahorra RAM a largo plazo; (3) TSC drift + entropy pool + per-tenant patching tienen que ser perfectos. Todo resoluble pero exige rigor.

---

## 1. ¿Depende de Firecracker?

**No.** El malentendido vino de que cité "Firecracker snapshot" como atajo para la técnica — pero el mecanismo es puro KVM.

| Lo que necesita el snapshot/restore | ¿De dónde viene? |
|---|---|
| Dump del estado de vCPU (regs, sregs, FPU, MSRs) | ioctls `KVM_GET_REGS`, `KVM_GET_SREGS`, `KVM_GET_FPU`, `KVM_GET_MSRS` |
| Dump del estado de LAPIC + IOAPIC + PIT | `KVM_GET_LAPIC`, `KVM_GET_IRQCHIP` |
| Dump de páginas dirty de la memoria del guest | `KVM_GET_DIRTY_LOG` + `madvise(MADV_DONTNEED)` |
| Memoria CoW compartida entre restores | `memfd_create` + `mmap(MAP_PRIVATE)` + `madvise(MADV_DONTFORK)` (todo libc estándar) |
| Restore: re-crear VM + setear estado | mismas ioctls con `KVM_SET_*` |
| Restore: re-attachar memory regions | `KVM_SET_USER_MEMORY_REGION` |

**Todo eso ya existe en Linux KVM desde ~kernel 3.x.** No hay nada que NKR no pueda llamar directo. Firecracker es solo un consumidor de estas APIs — escrito en Rust, igual que `vmm.rs`. Podemos implementarlo nosotros y nos quedamos con la misma postura ("zero external dependencies") que justifica todo NKR.

Comparación de dependencias:

| | NKR hoy | NKR + snapshot/restore propuesto | NKR + Firecracker (hipotético) |
|---|---|---|---|
| KVM | sí | sí | sí |
| libc | sí | sí | sí |
| Crate Rust adicional | ninguno crítico | **ninguno** | `firecracker` binary, jailer, API REST |
| Binarios externos en runtime | ninguno | **ninguno** | sí (firecracker, jailer) |
| Proceso supervisor distinto | no | no | sí (jailer) |
| Lockin a otro stack | no | **no** | sí |

---

## 2. NKR hoy — arquitectura de boot por VM

Cada `nkr run` (1 instancia tenant) hace:

```
1. memfd_create(guest_ram, MAP_SHARED, size = ram_mb)         ← memoria guest
2. mmap(rootfs.ext4, MAP_SHARED) via virtio-pmem (DAX)        ← rootfs read-only compartido
3. Cargar kernel image en memfd offset 0                      ← ~12 MB de kernel
4. Cargar initramfs cpio                                       ← ~3 MB
5. Setup cmdline con nkr.ip / nkr.fs* / etc.
6. ioctl(KVM_CREATE_VM), KVM_CREATE_VCPU
7. ioctl(KVM_SET_USER_MEMORY_REGION) — registrar la memoria
8. ioctl(KVM_SET_REGS) — RIP apuntando al entry point del kernel
9. ioctl(KVM_RUN) — vCPU empieza a ejecutar el kernel desde frío
10. Kernel descomprime initramfs, sube interfaces, monta DAX rootfs
11. /sbin/init → /usr/sbin/nkr-start.sh → odoo arranca
12. Total: ~5 segundos hasta primer GET /web/login OK
```

**Lo que se gasta de RAM (post-boot, VM ociosa):**

| Componente | RAM |
|---|---:|
| Kernel TEXT (idéntico entre VMs) | 8–12 MB |
| Kernel rodata + ksymtab (idéntico) | 5–8 MB |
| Kernel BSS + slab caches (per-guest) | 40–60 MB |
| `struct page` array (60 B × páginas) | ~15 MB por GB |
| Page tables EPT del host | ~RAM/512 = 2 MB por GB |
| Initramfs descomprimido | 2–4 MB |
| Driver state (virtio rings, IRQ tables) | 3–5 MB |
| Stacks de kthreads | ~5 MB |
| **Total overhead "kernel"** | **~80–120 MB** |
| **De los cuales potencialmente compartibles** | ~15–25 MB |

**Densidad actual:** con 32 GB de host RAM y VMs de 1024–1300 MB cada una, salen ~22 VMs por cell. La meta del whitepaper (100 Odoos en 32GB) requiere bajar RAM per-VM agresivamente.

---

## 3. Arquitectura propuesta — snapshot/restore in-house

### 3.1 Flujo nuevo

**Fase 1 — Construcción del snapshot (una vez por kernel/cell, offline) — v2 post-1ª revisión:**

```
1. Bootear UN guest "template" full normal (igual que hoy, ~5s)
2. Guest llega a un punto bien definido — CORE-ONLY, sin virtio-fs montado:
   - Kernel arriba, initramfs montado (DAX rootfs OK — eso sí va al snapshot)
   - Python interpretado, `import odoo` ejecutado (interpreter + libs cargadas)
   - ATENCIÓN: NO se monta /mnt/extra-addons, /mnt/systemouts-addons,
     /mnt/odoo-filestore, /mnt/odoo-logs, /mnt/odoo-pylibs (todos virtio-fs)
   - Odoo NO carga el module registry todavía (sin addons disponibles)
   - Odoo NO conecta a Postgres, NO bind() :8069
   - Init custom espera en /dev/hvc0 por mensaje del host (paused state)
3. Antes del pause:
   - `gc.disable()` + `gc.freeze()` desde el init custom Python — congela
     todos los objetos cargados como "permanent generation" para que el GC
     scan no los toque post-restore (mitigación de write-on-read del GC)
   - Si Python >= 3.12: marcar registry + módulos importados como immortal
     vía sys.setrefcount o el nuevo PyUnstable_Object_EnableDeferredRefcount
     (CONFIRMAR versión Python en rootfs Odoo 19 — si es 3.10/3.11 no aplica
     y este step se omite)
   - Cerrar todos los FDs no-esenciales (solo dejar /dev/hvc0)
4. Host pausa la vCPU: ioctl(KVM_INTERRUPT) + ioctl(KVM_RUN con flag pause)
5. Host snapshotea:
   - Estado vCPU: KVM_GET_REGS, KVM_GET_SREGS, KVM_GET_FPU, KVM_GET_XSAVE,
     KVM_GET_MSRS, KVM_GET_VCPU_EVENTS, KVM_GET_DEBUGREGS
   - LAPIC + IOAPIC: KVM_GET_LAPIC, KVM_GET_IRQCHIP
   - Reloj: KVM_GET_CLOCK
   - Memoria guest: el memfd entero, dumpeado a /mnt/nkr/snapshots/<cell>.snap
   - Virtio device state: SOLO devices que van a sobrevivir al restore —
     virtio-console (hvc0), virtio-balloon, virtio-net, virtio-pmem (DAX
     rootfs). virtio-fs queda EXCLUIDO porque virtiofsd es per-VM y no
     puede compartirse → al restore montamos virtio-fs FRESCO via hot-plug.
6. Guarda metadata JSON: cmdline, layout de memory regions, kernel_sha256,
   python_version, odoo_version, snapshot_format_version
7. Snapshot total ≈ 1024 MB (size del guest RAM, comprimible a ~300-400 MB
   con zstd nivel 3)
```

> **Por qué virtio-fs NO va en el snapshot (1ª revisión, crítica del equipo):**
> El estado de virtio-fs vive en (a) virtqueue indices del kernel guest, (b) FUSE
> session state del kernel guest, (c) proceso `virtiofsd` del host. (c) es per-VM
> por diseño y desaparece al apagar el template. Si congelamos (a) y (b) en el
> snapshot y restoramos con un virtiofsd nuevo apuntando a otro path, los
> índices de la vring del guest no matchean con el daemon nuevo → kernel BUG
> o D-state inmediato, antes que el init-agent pueda ejecutar nada. La única
> arquitectura segura es: snapshot core-only, mount virtio-fs post-restore via
> PCI hot-plug.

**Fase 2 — Restore (cada `nkr run` nuevo, online) — v2 post-1ª revisión:**

```
1. memfd_create(guest_ram, MAP_PRIVATE)   ← CRÍTICO: PRIVATE, no SHARED
2. mmap(snapshot.snap, MAP_PRIVATE | MAP_NORESERVE) sobre el memfd
   → las páginas NO escritas son CoW del snapshot, compartidas físicamente
     entre TODOS los restores activos
   → ATENCIÓN (1ª revisión): las páginas del HEAP de Python NO se mantendrán
     compartidas tras los primeros requests — el refcount de CPython escribe
     en el header de cada PyObject leído → CoW split. Las que SÍ se quedan
     compartidas: kernel .text/.rodata, .text de Python + libpython + glibc
     + .so cargadas read-only, bytecode pages inmutables. Ver §3.2.
3. KVM_CREATE_VM, KVM_CREATE_VCPU
4. KVM_SET_USER_MEMORY_REGION con el memfd
5. KVM_SET_REGS, KVM_SET_SREGS, ... — restaurar TODO el estado vCPU del snapshot
6. KVM_SET_LAPIC, KVM_SET_IRQCHIP, KVM_SET_CLOCK
   KVM_SET_TSC_KHZ + KVM_KVMCLOCK_CTRL con el clock actual del host (evita
   drift entre restores)
7. Spawn virtiofsd nuevo del host (per-tenant) — apunta a los paths del
   tenant (addons, filestore, logs, pylibs). NO se attacha todavía al guest.
8. Inyectar parches per-tenant via /dev/hvc0 ANTES de KVM_RUN:
   - hostname, MAC, IP, gateway, DB name, admin_passwd hash, nkr_sso secret
   - 256 bytes random del host (para reseed de /dev/urandom)
   - paths de virtio-fs a montar
9. KVM_RUN — vCPU reanuda exactamente donde se pauseó
10. Init agent del guest (corriendo en el snapshot pre-pauseado) sale del
    wait() en hvc0, aplica los parches:
    a. Reseed /dev/urandom con los 256 bytes random del host
    b. ip neigh flush all + ip addr add <new>/24 dev eth0 + ip route add default
    c. hostname <new>
    d. Hot-plug virtio-fs devices via PCI (host attacha; guest los enumera)
    e. mount -t virtiofs addons /mnt/extra-addons (y los demás shares)
    f. Ahora SÍ: Odoo carga el module registry desde los addons recién montados
    g. Conecta a PG con el db_name nuevo
    h. bind() :8069
    i. Listo
11. Total: ~1.5-3 s hasta primer GET /web/login OK
   (NO 200-500 ms como decía v1 — el module load post-mount añade ~1-3s,
    pero sigue siendo 2-3× más rápido que el boot full actual)
```

### 3.2 Qué se comparte físicamente vía CoW — v2 post-1ª revisión (honesto)

Con `mmap(MAP_PRIVATE)` sobre un mismo archivo (`snapshot.snap`):
- El kernel del host crea VMAs separados por proceso, pero las **páginas físicas son las mismas** hasta que alguien escriba.
- Al primer write en una página → CoW → solo esa VM toma una copia privada.

**Qué se comparte de verdad (medido / proyectado):**

| Región | Tamaño típico | ¿Se queda compartido? | Por qué |
|---|---:|---|---|
| Kernel `.text` + `.rodata` (guest) | 15–25 MB | **Sí, sólido** | Kernel no se modifica a sí mismo en runtime normal |
| Initramfs descomprimido | 2–4 MB | **Sí, sólido** | Read-only post-mount |
| `.text` de Python interpreter + libpython | 5–10 MB | **Sí, sólido** | Read-only, mmap del rootfs DAX |
| `.text` de glibc + .so de NumPy/lxml/Pillow/cryptography | 20–30 MB | **Sí, sólido** | Read-only, mmap del rootfs DAX |
| Bytecode pages (code objects de Python) | 5–10 MB | **Sí parcial** | CPython no escribe sobre code objects post-parse |
| `struct page` array del host (per-guest) | 15 MB / GB | No | Tracking físico per-VM |
| Page tables EPT del host | 2 MB / GB | No | Per-guest mapping |
| Kernel slab caches (vfs, dentry, inode, skbuff) | 40–60 MB | No | Estado mutable per-guest |
| **Heap de Python** (PyObjects, dicts, listas, registry de Odoo) | 200–400 MB | **NO — CoW se rompe rápido** | Ver explicación abajo |
| Stacks de kthreads + driver state | ~10 MB | No | Per-guest |

**Total sólido compartido por VM: ~50–80 MB** (no los 100–150 MB que decía v1).

**Por qué el heap de Python NO se comparte (1ª revisión, crítica del equipo):**

CPython implementa garbage collection vía **reference counting**. Cada `PyObject` tiene un campo `ob_refcnt` en su header (primeros 8 bytes de la estructura). Toda lectura no trivial — pasar variable, llamar método, indexar dict, iterar — ejecuta `Py_INCREF` (al entrar al scope) y `Py_DECREF` (al salir), que escriben en `ob_refcnt`.

```c
// Macro real de CPython (Include/object.h, simplificado):
#define Py_INCREF(op)  ((op)->ob_refcnt++)
#define Py_DECREF(op)  do { if (--(op)->ob_refcnt == 0) _Py_Dealloc(op); } while (0)
```

Consecuencia para el snapshot:
1. Snapshot tiene la registry de Odoo cargada al heap, refcounts en valores estables.
2. Restore en VM-1: usuario hace GET /web/login → Odoo lee `request.session.uid`, `request.env['res.users']`, etc. → cientos de PyObjects leídos → cientos de páginas con `ob_refcnt` modificado → CoW split de todas esas páginas.
3. Una página de 4 KB típicamente contiene 30–60 PyObjects. Basta tocar 1 para que la página entera se separe físicamente entre VM-1 y las demás.
4. Tras el primer request HTTP de cada VM, el ~40–60 % del heap de Python ya está dirtied → la RAM compartida del heap colapsa.

**Mitigaciones reales (no rescatan todo, pero ayudan):**

- **`gc.disable()` + `gc.freeze()` pre-snapshot** (Python 3.7+) — `gc.freeze()` marca objetos como "permanent generation"; el GC no los scanea → evita escrituras del GC mark/sweep. **Pero NO mitiga el refcount** (que es el problema mayor). Aplicar igual: barato, ayuda en el margen.
- **PEP 683 / immortal objects** (Python 3.12+) — marca objetos específicos como inmortales; CPython skipea `INCREF`/`DECREF` sobre ellos vía sentinel check. Si Python del rootfs es ≥ 3.12, marcar la registry + módulos importados como immortal podría recuperar 30–50 MB extras de páginas estáticas. **Hay que confirmar versión Python en rootfs Odoo 19** — si es 3.10/3.11, no aplica.
- **NoGIL Python (3.13+ experimental)** — deferred refcounting para algunos objetos. Far future para Odoo.

**Sin KSM en NKR (v3, post-2ª revisión — decisión arquitectónica previa):**

Una vez que CoW separa una página, su contenido en ambas VMs puede seguir siendo idéntico hasta que algo más cambie (ej. dos VMs leen el mismo objeto N veces → refcount termina en N+(snapshot value) en ambas → contenido idéntico, página físicamente distinta). En otras arquitecturas, `ksmd` del host scaneaba periódicamente, detectaba contenido idéntico y re-fusionaba como CoW.

**En NKR, KSM está descartado por decisión arquitectónica previa** (compatibilidad / estabilidad — confirmado en `CLAUDE.md` y en los resúmenes públicos del proyecto). No vamos a revivirlo solo porque la matemática lo necesite. Eso tiene consecuencias directas:

- **El sangrado de CoW por refcount es irreversible.** Las páginas que se separan al primer request HTTP no se re-fusionan nunca más.
- **El RSS por VM sube monotónicamente** desde ~100 MB al restore hasta estabilizar en ~250–300 MB tras horas de tráfico activo. A partir de ese punto se mantiene estable porque el heap de Odoo no crece linealmente con el tiempo (la app es REST/HTTP, no acumula state permanente entre requests más allá de session cache + ORM cache).
- **No hay sharing inter-cell ni inter-tenant a largo plazo.** Solo lo que el kernel/libs read-only mapean del DAX rootfs sigue compartido.

Mitigaciones que SÍ aplican (sin KSM):
- `gc.disable()` + `gc.freeze()` pre-snapshot: marca los objetos cargados como permanent generation; el GC no los scanea → evita escrituras del mark/sweep. **No mitiga el refcount**, pero reduce un vector secundario.
- **PEP 683 / immortal objects** (Python 3.12+): marca objetos específicos como inmortales; CPython skipea `INCREF`/`DECREF` sobre ellos. **Esta es la única herramienta real para frenar el sangrado del refcount** en objetos seleccionados (la registry de Odoo, los modelos, los managers de pool). Si el rootfs Odoo 19 usa Python ≥ 3.12, podría recuperar 30–50 MB de páginas estáticas que de otro modo se irían a CoW. **Confirmar versión en Fase 0.**
- Sin Python 3.12, no hay segunda mitigación. La degradación va de ~100 MB inicial a ~250–300 MB final y no hay reversión.

**Resultado realista esperado (v3, sin KSM):**

| Estado | RSS por VM | Notas |
|---|---:|---|
| Post-restore (inmediato) | ~100 MB | Solo páginas dirty del bootstrap del init-agent + Python startup |
| Tras primer request HTTP | ~150 MB | Refcount empieza a separar el heap |
| Tras ~1 hora de tráfico moderado | ~200 MB | Mayoría del módulo registry "tocado" → CoW split |
| Estable post-12 h | ~250–300 MB | Heap activo de la app diverge; libs read-only siguen compartidas |
| **Hoy sin snapshot (referencia)** | **~400–500 MB** | Boot frío + bootstrap completo |

Densidad teórica en 32 GB (sin KSM):
- 32 GB − 2 GB (OS+NKR) − 8 GB (Postgres con N DBs activas) = 22 GB netos
- 22 GB / 280 MB (RSS estable medio) ≈ **78 VMs techo absoluto, ~60–75 activas con tráfico real**

**La meta del whitepaper (100 Odoos en 32 GB) NO se cumple sin KSM.** Para 100 Odoos en este hardware el camino real es: snapshot/restore + upgrade del host a 64 GB. La inversión del proyecto se justifica entonces por velocidad de boot y eliminación del boot storm, no por la promesa original de densidad.

### 3.3 Per-tenant patching post-restore

El snapshot tiene valores genéricos (hostname=`tenant-template`, MAC=00:00:..., DB=`db-tenant-template`, etc.). Necesitamos inyectar los reales per-tenant **después** del restore y **antes** de que Odoo bind() el puerto.

Mecánica: un mini init-agent NKR (Rust o shell, en initramfs) que en el snapshot está esperando un mensaje en `/dev/hvc0`. El host escribe un JSON con los parámetros tenant-específicos justo después de `KVM_RUN`. El agent:

1. Lee JSON del hvc0
2. `hostname <new>`
3. Reescribe `/etc/resolv.conf`, `/etc/hosts`
4. `ip addr add <new>/24 dev eth0`, `ip route add default via <gw>`
5. Reescribe `/etc/odoo/odoo.conf` con: `db_name`, `admin_passwd`, `[nkr_sso] secret`
6. Inicia Odoo (`exec /usr/bin/odoo --config=/etc/odoo/odoo.conf`)
7. Listo

Esto añade ~50–100 ms al restore. El total sigue siendo <500 ms.

---

## 4. Comparación lado-a-lado — v3 post-2ª revisión (sin KSM)

| Aspecto | NKR hoy (boot desde kernel) | NKR + snapshot/restore in-house |
|---|---|---|
| **Tiempo de boot por VM** | ~5 s | **~1.5–3 s** (2–3× más rápido) |
| **RAM RSS post-restore (inmediato)** | ~400–500 MB cold | **~100 MB** |
| **RAM RSS estable tras horas de tráfico** | ~400–500 MB | **~250–300 MB** (-30 a -40 %) — sin KSM no hay re-fusión de páginas refcount-divergidas |
| **Densidad activa en 32 GB** (descontando 8 GB Postgres) | ~22 Odoos | **~60–75 Odoos** (no 100 — la meta del whitepaper requiere 64 GB) |
| **Overhead "kernel" per VM** | ~80–120 MB | ~30–50 MB sólido compartido (kernel TEXT + libs read-only del DAX rootfs) |
| **POST /instances end-to-end** | ~13 s (clone DB + boot) | ~5–8 s (clone DB + restore + module load post-mount) |
| **REL_OD / commit→reload** | 5–8 s | 5–8 s (sin cambio — es un re-exec dentro del guest) |
| **POST /actions {restart}** | 30–60 s | **5–15 s** (restore + module load en vez de boot full) |
| **Boot storm (levantar N VMs simultáneo al iniciar la cell)** | N × 5s, CPU/IO saturado | N × ~1.5s, despreciable — gran win |
| **Dependencias externas nuevas** | — | **ninguna** (puros ioctls KVM) |
| **Líneas de Rust nuevas** | — | ~1500–2500 (`snapshot.rs`, refactor `vmm.rs`) |
| **Crates nuevos** | — | **0** |
| **Binarios externos** | — | **0** |
| **Procesos extra** | — | **0** |
| **Compatibilidad con balloon dinámico** | sí | sí (snapshot incluye el state del balloon driver) |
| **Compatibilidad con virtio-fs shares per-tenant** | sí | **requiere cuidado** (ver §6) |
| **Compatibilidad con DAX rootfs compartido** | sí | sí (el snapshot referencia el mismo virtio-pmem) |
| **Compatibilidad con SSO HMAC per-tenant** | sí (secret en odoo.conf) | sí (inyectado post-restore vía hvc0) |
| **Compatibilidad con admin_user_password rotation** | sí (JSON-RPC post-boot) | sí (igual, post-restore) |
| **Compatibilidad con `nkr ps` / métricas** | sí | sí (mismo registry de state) |
| **Watchdog REL_OD-aware** | sí | sí (sin cambio) |
| **Update del kernel del cell** | rebuild kernel + nada más | rebuild kernel + **regenerar snapshot del cell** (proceso nuevo, ~5 min, una vez por update) |
| **Update de Odoo del cell** | rebuild rootfs ext4 | rebuild rootfs + **regenerar snapshot** |
| **Snapshot puede romperse** | n/a | sí, si cambia el kernel o algún driver virtio sin regenerar snapshot |
| **TSC drift entre restores** | n/a | hay que setear `KVM_SET_TSC_KHZ` + `KVM_KVMCLOCK_CTRL` correctamente |
| **Entropy pool del guest** | se siembra al boot normal | se siembra una vez en snapshot; **hay que reseedear post-restore** vía `/dev/urandom` desde el agent, sino todos los tenants arrancan con el mismo pool inicial (problema de seguridad mediano) |
| **Confidential computing (SEV/TDX)** | compatible | **incompatible** — memoria encriptada no se puede CoW-share |
| **Seguridad multi-tenant** | strict (KVM isolation) | igual de strict (las páginas CoW se separan al primer write — no hay leak posible entre tenants una vez divergen) |

---

## 5. Cambios concretos en el código

### Archivos nuevos

- **`src/snapshot.rs`** (~800 líneas)
  - `pub fn capture(vm_id, output: &Path) -> Result<()>` — pausea vCPU + dump estado + dump memoria
  - `pub fn restore(snapshot: &Path, tenant: TenantPatch) -> Result<Vm>` — mmap CoW + KVM_SET_* + spawn vCPU thread
  - `pub fn validate(snapshot: &Path, kernel_hash: [u8;32]) -> Result<()>` — verifica que el snapshot matchea el kernel actual
  - Tipos: `SnapshotMeta` (cmdline, layout, kvm_clock, vcpu_count, kernel_sha256)

- **`crates/nkr-init-agent/`** (~200 líneas Rust musl static, embebido en initramfs)
  - Reemplaza a `nkr-start.sh` en el path de snapshot
  - Loop: wait en `/dev/hvc0` por un JSON con tenant patches; aplica; exec odoo
  - **No reemplaza el boot normal** — convive como fallback cuando no hay snapshot

### Archivos modificados

- **`src/vmm.rs`** (~400 líneas modificadas)
  - `run()` aprende a aceptar `--from-snapshot <path>` además de `--kernel <path>`
  - Splittear `configure_memory` para soportar memfd MAP_PRIVATE con mmap CoW del snapshot
  - Manejar el case "vCPU restaurada vs vCPU desde kernel entry"

- **`src/compose.rs`** (~150 líneas modificadas)
  - Selector: si existe `<cell>/snapshots/current.snap` válido → usar restore path; sino → boot normal (fallback)
  - Pasar tenant patches al stdin del `nkr run` (que las pasa al init-agent via hvc0)

- **`src/cell.rs`** (~100 líneas)
  - Nuevo subcomando `nkr cell snapshot <cell>` para generar el snapshot
  - Trigger automático del snapshot tras `nkr build` cuando se updatea kernel/rootfs

- **`src/initramfs.rs`** (~50 líneas)
  - Incluir `nkr-init-agent` binary cuando se compone el initramfs

### Archivos sin cambio

- `src/api.rs`, `src/bin/nkr_api_server.rs` — el HTTP API no cambia. El panel sigue llamando POST /instances igual.
- `src/state.rs`, `src/metrics.rs`, `src/watchdog.rs`, `src/janitor.rs` — sin cambio.
- `src/balloon.rs` — sin cambio (el estado del balloon va en el snapshot, se restaura automáticamente).
- Todos los módulos Odoo (`nkr_sso`, `systemouts-addons`) — sin cambio.

---

## 6. Riesgos y pitfalls honestos — v2 post-1ª revisión

### Crítico (1ª revisión: escalados desde "alto/medio" tras feedback del equipo)

1. **virtio-fs en el snapshot causa kernel panic / D-state inmediato al restore.** El estado de virtio-fs vive en tres lugares: virtqueue indices del guest, FUSE session state del guest, proceso `virtiofsd` del host (per-VM, no compartible). Si congelamos el estado del guest apuntando a un virtiofsd del template y restoramos con un virtiofsd nuevo, los índices de la vring no matchean → kernel BUG antes que el init-agent ejecute. **Mitigación obligatoria (NO opcional):** el snapshot se toma SIN virtio-fs montado (core-only), el mount es post-restore via PCI hot-plug. Trade-off: añade ~1–3 s al restore (module registry load post-mount) — el boot total queda en 1.5–3 s, no 200–500 ms como decía v1.

2. **Refcount de CPython degrada el sharing del heap en horas — sin reversión posible en NKR.** `Py_INCREF`/`Py_DECREF` escriben en `ob_refcnt` con cada lectura de objeto → CoW split de la página entera. Tras el primer request HTTP, ~40–60 % del heap se separa físicamente entre VMs. **Mitigaciones que SÍ aplican (sin KSM):** (a) `gc.disable()` + `gc.freeze()` pre-snapshot — barato, ayuda en el margen (no toca refcount, sí evita escrituras del GC scan); (b) si Python ≥ 3.12 en rootfs, marcar registry + módulos como immortal (PEP 683) — **única herramienta real para frenar el refcount en objetos seleccionados**. **KSM NO está disponible en NKR** (decisión arquitectónica previa, ver `CLAUDE.md`) → las páginas refcount-divergidas no se re-fusionan. Consecuencia: el RSS por VM sube monotónicamente hasta estabilizar en ~250–300 MB y se queda ahí. La densidad cae a 60–75 Odoos activos en 32 GB (no 100). El proyecto se justifica por velocidad de boot, no por densidad.

### Alto

3. **TSC y kvmclock drift**: si dos VMs restoran del mismo snapshot con TSC offsets distintos, el reloj del guest puede saltar. **Solución:** setear `KVM_SET_TSC_KHZ` + `KVM_KVMCLOCK_CTRL` con el clock actual del host en el restore; el guest hace `clock_gettime()` y obtiene el valor correcto. Estudiado, hay código de referencia en QEMU.

4. **Entropy pool del kernel guest**: el snapshot fija el estado inicial de `/dev/random`. Todos los tenants arrancan con el mismo pool → predecible (riesgo de seguridad mediano: PRNGs derivados quedan correlacionados entre tenants). **Solución:** el init-agent escribe 256 bytes random (sourced del hvc0 desde el host) a `/dev/urandom` antes de cualquier `exec` — reseed completo.

### Medio

5. **Open file descriptors en el snapshot**: si el snapshot tiene FDs abiertos hacia `/dev/hvc0`, `/dev/virtio-portsN`, etc., al restore esos FDs apuntan a devices recién creados — pueden estar en estado inconsistente. **Solución:** el guest del snapshot cierra todos los FDs no-esenciales antes del pause. Solo deja abierto `/dev/hvc0` (el canal que el init-agent usa para recibir patches).

6. **Network ARP / neighbor cache**: el snapshot puede tener entradas ARP del template. **Solución:** init-agent hace `ip neigh flush all` post-restore.

7. **Postgres connection en el snapshot**: si el snapshot tuviera la conexión abierta, todas las VMs intentarían usar el mismo TCP socket → catástrofe. **Solución:** snapshot DELIBERADAMENTE se toma ANTES de que Odoo abra la conexión PG. Init-agent post-restore deja a Odoo abrir la conexión él mismo (es la primera cosa que hace al exec).

8. **PCI hot-plug latency del virtio-fs post-restore**: hot-attachar 4+ virtio-fs devices al guest desde el host añade ~100–300 ms (depende de cuántos shares por tenant y qué tan rápido el kernel guest los enumera). Es el costo del "snapshot sin virtio-fs". **Mitigación:** paralelizar el hot-plug de los N devices en vez de serial. Aceptable dentro del presupuesto de 1.5–3 s de boot total.

9. **Enumeración PCI / colisión de IRQs en hot-plug (2ª revisión)**: el guest pre-pauseado del snapshot tiene un árbol PCI ya enumerado (virtio-console, virtio-balloon, virtio-net, virtio-pmem). Al hotpluggear 4 virtio-fs devices nuevos, hay que asegurar que los slots PCI asignados no pisen IRQs ya en uso (especialmente el de virtio-net — si colisiona, el guest pierde la red al primer paquete). **Mitigación:** reservar un rango fijo de slots PCI en el snapshot (ej. slots 8–11) para los virtio-fs que vienen después, validar pre-hotplug que esos slots están libres en el guest enumerado. Listado como riesgo medio porque la mecánica es estándar (QEMU lo hace todos los días), pero NKR tiene un VMM custom y hay que hacerlo bien la primera vez.

### Bajo

7. **MTU del virtio-net en el snapshot**: si el cell cambia, hay que regenerar. Aceptable — los cells no cambian network config seguido.

8. **Kernel cmdline parameters en el snapshot**: hardcoded al momento del snapshot. Tenant-específicos (IP, hostname) NO van en cmdline en este modelo — van por hvc0. Esto es un cambio de mentalidad para futuros params.

9. **CPUID features**: si el snapshot se toma en una CPU Intel y se restora en AMD, fallará. **Solución:** snapshot por host (no por cluster). Aceptable, NKR ya es single-host.

---

## 7. Plan de implementación por fases — v2 post-1ª revisión

### Fase 0 — Spike de viabilidad (3 días) — v3 misión alterada (sin KSM)

**Nueva misión (2ª revisión):** el objetivo NO es medir si KSM nos salva (KSM no aplica). El objetivo es medir **cuán rápido y cuán profundo es el sangrado de CoW** cuando bombardeas la instancia restorada con tráfico real, **sin ningún mecanismo de re-fusión disponible**.

- Escribir un `snapshot_poc.rs` standalone que: bootea una VM con Odoo full hasta el punto del snapshot core-only, pausea, dump regs+memoria, restora N=5 procesos nuevos en paralelo, monta virtio-fs post-restore, lanza Odoo.
- Medir RSS en 3 puntos temporales:
  - **T=0** (inmediatamente post-restore, antes del primer request): RSS esperado ~100 MB
  - **T=10 min** tras bombardear con **100 peticiones HTTP concurrentes** sobre cada VM (login + lista de productos + creación de orden + búsqueda): RSS esperado ~200–250 MB
  - **T=60 min** tras tráfico sostenido moderado (10 req/s sobre cada VM): RSS esperado estabiliza ~250–300 MB
- Si el RSS a T=60 min se dispara a ~450 MB (equivalente a boot frío) en menos de 10 minutos → el snapshot solo acelera el arranque pero no ahorra RAM a largo plazo → reconsiderar si el refactor de 3–4 semanas vale solo por velocidad.
- Si el RSS a T=60 min se estabiliza en ~250–300 MB → el proyecto tiene producto sólido: 2–3× boot speedup + 30–40 % ahorro de RAM sostenido + densidad 60–75 Odoos.
- **Confirmar versión exacta de Python** en el rootfs de odoo-v19 (`python3 --version` dentro del guest). Si ≥ 3.12 → aplicar PEP 683 immortal en Fase 1. Si < 3.12 → solo `gc.freeze()`.
- **Salida:** go/no-go con números reales de RSS estable post-tráfico + versión Python confirmada.

### Fase 1 — snapshot del template community (1 semana) — v3 sin KSM

- Implementar `snapshot.rs` completo (capture + restore + validate).
- Implementar `nkr-init-agent` Rust musl static, embebido en initramfs.
- En el path de captura del snapshot: ejecutar `gc.disable()` + `gc.freeze()` desde el init Python pre-pause. Si Python ≥ 3.12 (confirmado en Fase 0), marcar registry + módulos como immortal vía `PyUnstable_Object_EnableDeferredRefcount` o sentinel `_Py_IMMORTAL_REFCNT` — única herramienta para frenar el refcount sin KSM.
- `nkr cell snapshot odoo-v19` genera el snapshot del template community **SIN virtio-fs montado** (core-only: kernel + initramfs + Python + odoo importado, pero sin addons cargados).
- Modificar `compose.rs` para usar restore cuando hay snapshot.
- **No tocar production todavía** — probar solo en una cell de test paralela.
- **Salida:** demo de 5 tenants restorados, RSS host real medido pre- y post-dirty bajo carga HTTP (mismo bombardeo que Fase 0).

### Fase 2 — Patches per-tenant completos + virtio-fs hot-plug (1 semana)

- Init-agent maneja, en orden post-restore:
  1. Reseed `/dev/urandom` con bytes random del host (256 B vía hvc0)
  2. `ip neigh flush all` + setup network (hostname, MAC, IP, gateway, DNS)
  3. **PCI hot-plug de los virtio-fs devices del tenant** (host attacha, guest enumera)
  4. `mount -t virtiofs` de los shares (addons, filestore, logs, pylibs, systemouts-addons)
  5. Patch `odoo.conf`: db_name, admin_passwd, `[nkr_sso] secret`
  6. `exec /usr/bin/odoo` — Odoo carga el module registry desde addons recién montados
- E2E test: crear 10 tenants restorados, login HTTP, instalar 1 módulo, verificar que cada uno tiene su DB/secret/identidad correcta y addons propios.
- **Medir boot time real**: target 1.5–3 s, alarm si >5 s.

### Fase 3 — Enterprise + variantes de cell (3 días)

- Snapshot separado para template enterprise (`<cell>-odoo-template-enterprise.snap`).
- Snapshot por versión (`odoo-v17`, `odoo-v19`).
- Validación de snapshot vs kernel actual (`kernel_sha256`).

### Fase 4 — Cutover + rollback path (2 días)

- Flag `NKR_USE_SNAPSHOT=1` por cell (opt-in).
- Si flag activa pero snapshot inválido/missing → fallback automático a boot normal con warning.
- Migrar cells de prod una a una. Cell de test primero, luego v17 (vacía), luego v19.
- **Rollback:** `unset NKR_USE_SNAPSHOT` + restart daemon → todas las VMs nuevas vuelven a boot normal. Las VMs ya corriendo no se tocan.

### Fase 5 — Optimizaciones (post-cutover, scope reducido v3)

- Compresión zstd del snapshot en disco.
- ~~KSM tuning~~ — descartado (KSM no aplica en NKR).
- ~~Pre-warm pool~~ — **diferido indefinidamente** por decisión de la 2ª revisión: añade complejidad de orquestación por un ahorro de ~1 s. El foco queda en estabilidad del flujo principal, no en optimizaciones marginales.

---

## 8. Recomendación — v3 post-2ª revisión (sin KSM, refocus en boot speed)

1. **Luz verde a Fase 0 (spike) con misión alterada.** Aprobado por el equipo en la 2ª revisión. Cuesta 3 días, mide el sangrado de CoW bajo carga HTTP real (100 req concurrentes × 10 min), no sharing teórico. El umbral go/no-go:
   - RSS estable a T=60 min ≤ ~300 MB → producto sólido (boot speedup + ahorro real)
   - RSS se dispara a ~450 MB en <10 min → snapshot solo acelera arranque, no ahorra RAM a largo plazo → reconsiderar si el refactor de 3–4 semanas vale solo por velocidad

2. **El proyecto se justifica ahora por VELOCIDAD DE BOOT, no por densidad.**

   | Beneficio | Magnitud | Valor real |
   |---|---|---|
   | Boot por VM 5 s → 1.5–3 s | 2–3× | Elasticidad / UX — un ERP que arranca en 2 s es magia negra en la industria |
   | Eliminación del boot storm al levantar la cell | N × 5s → N × 1.5s | Cell de 20 VMs: 100 s → 30 s, sin saturar CPU/IO del host |
   | POST /actions {restart} | 30–60 s → 5–15 s | Restart de tenant casi imperceptible |
   | RSS estable | -30 a -40 % | ~60–75 Odoos en 32 GB (vs ~22 hoy) |
   | Densidad 100 Odoos en 32 GB | **NO se cumple** | Requiere host de 64 GB |

   Tres puntos sobre la densidad:
   - El whitepaper original prometía 100 en 32 GB asumiendo sharing efectivo del heap. La realidad de Python (refcount) sin re-fusión (sin KSM) hace eso imposible en este hardware.
   - **Mejora real:** 3–3.5× más densidad (22 → 60–75), no 5× como decía v1.
   - Si el negocio exige estrictamente 100 Odoos en producción, la decisión correcta no es forzar el snapshot a ese objetivo — es upgradear el host a 64 GB o segmentar en 2 hosts × 32 GB.

3. **No depender de Firecracker.** Implementar in-house con ioctls KVM nativos. Mismo principio que justificó escribir el VMM en vez de usar QEMU.

4. **No revivir KSM.** La decisión arquitectónica previa de NKR (ya documentada en `CLAUDE.md` y removida de los resúmenes públicos) se respeta. No vamos a meter una tecnología descartada solo porque la matemática lo necesite.

5. **Diferir confidential computing.** SEV/TDX no es compatible con snapshot CoW por diseño. Si en el futuro hay clientes que pidan memoria encriptada, ese subset corre sin snapshot (boot normal). El 95 % del workload SaaS no necesita SEV.

6. **Pre-warm pool diferido indefinidamente.** Complejidad de orquestación alta por ~1 s de ahorro. Foco en estabilidad del flujo principal.

---

## 9. Conclusión — v3 post-2ª revisión

| Decisión a tomar | Recomendación v3 (sin KSM) |
|---|---|
| ¿Empezar el spike? | **Sí — luz verde a Fase 0**, aprobado por el equipo en 2ª revisión |
| ¿Depender de Firecracker? | **No**, implementación in-house con KVM ioctls |
| ¿Usar KSM? | **No.** Decisión arquitectónica previa de NKR. No se revive. |
| ¿virtio-fs puede ir en el snapshot? | **No.** Snapshot core-only, virtio-fs hot-plug post-restore. |
| ¿gc.disable + gc.freeze pre-snapshot? | **Sí**, barato, ayuda en el margen |
| ¿Marcar objetos como immortal (PEP 683)? | **Sí si Python ≥ 3.12** en el rootfs (única herramienta para frenar refcount sin KSM). Confirmar en Fase 0. |
| ¿Bajar el VM_RAM mínimo aprovechando esto? | Sí, una vez en producción — dev de 1300 → 800–1000, prod igual pero más VMs por host |
| ¿Cambiar la API o el contrato del panel? | **No**, transparente para el panel |
| ¿Cambiar la arquitectura per-tenant (DAX rootfs, virtio-fs shares)? | **No**, todo se preserva |
| ¿Target de densidad? | **60–75 Odoos activos en 32 GB** (no 100). Para 100 Odoos: host de 64 GB. |
| ¿Pre-warm pool? | **Diferido indefinidamente** |
| ¿Confidential computing (SEV/TDX)? | **No**, incompatible con CoW. Si requerido en el futuro: subset con boot normal. |
| ¿Cuidar enumeración PCI en hot-plug virtio-fs? | **Sí, riesgo medio agregado por equipo en 2ª revisión** — validar que los slots PCI hotpluggeados no pisen IRQs del virtio-net. Listado en §6. |

**El cambio paga, con expectativas honestas y mucho más modestas que v1.** Es "boot 2–3× más rápido + 30–40 % menos RAM sostenida + eliminación de boot storm + ~60–75 Odoos en 32 GB". NO es 100 Odoos en 32 GB — eso requiere KSM (descartado) o más hardware. La inversión de 3–4 semanas se justifica principalmente por la velocidad de boot, no por la densidad.

---

## 10. Primera rueda de revisión — Q&A del equipo (2026-05-17)

> **⚠️ SUPERSEDED por §11 en algunos puntos:** la 2ª rueda de revisión descartó KSM como mitigación (ver §11). Cualquier referencia a "KSM mandatorio" o "KSM como salvavidas" en esta §10 fue válida en el momento de la 1ª rueda pero queda **anulada por la 2ª rueda**. Los demás puntos (refcount de Python, virtio-fs out, snapshot core-only) siguen vigentes. Esta sección se conserva como registro histórico de la iteración.

Esta sección documenta el feedback técnico del equipo sobre la versión v1 del doc. Las correcciones del equipo están incorporadas a las secciones §3, §4, §6, §7, §8 y al TL;DR. Esta sección queda como registro para que la 2ª rueda de revisión sepa qué se ajustó y qué quedó pendiente de debate.

### Pregunta 1 del equipo — "La trampa del CoW con el refcounting de Python"

> "Tu premisa en el apartado 3.2 es que las páginas de la Registry de Odoo y el heap de Python compilado se mantendrán compartidas bajo MAP_PRIVATE. Esto es falso para Python. Python maneja la memoria mediante conteo de referencias y tiene un recolector de basura cíclico. Cada vez que Odoo lee un objeto de la Registry, ejecuta un método o simplemente pasa una variable, Python modifica el contador de referencias (`ob_refcnt`) de ese objeto en el heap. Modificar un contador es una operación de escritura. El kernel del host detecta la escritura, dispara el Copy-on-Write y duplica la página entera de 4 KB para esa VM específica. Tras el primer click de un usuario en la interfaz, el 'heap compartido' se desmorona."

**Respuesta de Claude:**

Correcto al 100 %. Subestimé este punto en v1. Las páginas del heap NO se mantienen compartidas — `Py_INCREF`/`Py_DECREF` escriben en el header del PyObject (`ob_refcnt`, primeros 8 bytes), y una página de 4 KB típicamente contiene 30–60 PyObjects → basta tocar uno para que la página entera diverja. Tras el primer request HTTP, ~40–60 % del heap se separa físicamente entre VMs.

**Lo que SÍ se queda compartido (corregido en §3.2):**
- Kernel `.text` + `.rodata` del guest (~15–25 MB sólido)
- `.text` de Python interpreter, libpython, glibc, .so de NumPy/lxml/Pillow (~30–50 MB sólido, read-only mapped)
- Bytecode pages inmutables (~5–10 MB)
- Initramfs (~2–4 MB)

**Total compartible sólido por VM: ~50–80 MB** (vs los 100–150 MB que decía v1, demasiado optimista).

**Mitigaciones aceptadas e incorporadas al plan:**
- `gc.disable()` + `gc.freeze()` pre-snapshot (Python 3.7+) → reduce escrituras del GC scan, no del refcount. Barato, se aplica.
- PEP 683 immortal objects (Python 3.12+) → solo aplicable si rootfs usa Python ≥ 3.12. **Pendiente confirmar en Fase 0 cuál versión corre el rootfs Odoo 19.**
- **KSM mandatorio agresivo desde día 1** → recupera las páginas refcount-divergidas que llegan a contenido idéntico. Movido de Fase 5 a Fase 1. Sin esto el sharing colapsa.

**Estado:** **acordado**, doc actualizado. Pendiente confirmar versión de Python en Fase 0.

---

### Pregunta 2 del equipo — "El campo de minas de virtio-fs"

> "En el riesgo número 1 del documento propones hacer un umount y remount dentro del init-agent post-restore. Esto es mecánicamente imposible si cambias el directorio del host. El estado del dispositivo virtio-fs (los índices de los Virtqueues, los punteros de la cabeza/cola del anillo virtual y los IDs de sesión FUSE) queda congelado en el snapshot apuntando al proceso virtiofsd del template. Si en la nueva VM levantas un proceso virtiofsd diferente apuntando al path de otro tenant, los descriptores del guest colisionarán inmediatamente con el estado del nuevo daemon en el host. El guest entrará en un kernel panic o en el temido D-state antes de que tu agente pueda siquiera ejecutar la línea del umount. Para solucionar esto, NKR tendría que emular un hot-unplug y hot-plug de la controladora PCI virtual en cada restore, lo que arruina el tiempo de boot de 200 ms."

**Respuesta de Claude:**

Correcto al 100 %. Mi propuesta de "umount + remount" en v1 era simplemente incorrecta — el guest no llega a ejecutar el umount porque crashea primero. La solución del equipo (snapshot core-only sin virtio-fs montado + hot-plug post-restore) es la correcta arquitecturalmente.

**Impacto en el tiempo de boot que tengo que admitir:**

El snapshot core-only fuerza a cargar el module registry de Odoo **post-restore** (porque sin addons montados al snapshot, Odoo no tenía qué cargar). El registry load + import de modelos toma 1–3 s. Más el hot-plug de los virtio-fs (~100–300 ms paralelizado), el boot total realista queda en **1.5–3 s**, NO en 200–500 ms como decía v1.

Sigue siendo 2–3× más rápido que los 5 s actuales, pero no el 10–25× que vendí. Corrección incorporada en TL;DR, §3.1, §3.2, §4.

**Estado:** **acordado**, doc actualizado. virtio-fs queda fuera del snapshot por arquitectura. Post-restore mount via hot-plug.

---

### Pregunta 3 del equipo — "La matemática real de densidad"

> "Asumiendo que mitigamos los problemas anteriores, recalculemos la RAM real basándonos en la degradación por CoW en producción. Kernel Overhead Limpio: 15 MB. Páginas Sucias por Bootstrapping/GC de Python: ~150 MB. Filestore/Core de Odoo: 0 MB (DAX). Consumo Neto Real por VM: ~165 MB RSS en el host. Física del Host: 32 GB totales - 2 GB (OS/NKR) - 8 GB (Postgres) = 22 GB netos para las VMs. Capacidad Real con Snapshot: 22 GB / 165 MB = 133 instancias. Conclusión: el snapshot/restore in-house es el único camino técnico real para rozar el objetivo de los 100 Odoos en este hardware, pero no por las razones que expone tu documento. No vas a ahorrar RAM porque el código se mantenga compartido (Python lo va a ensuciar), sino porque te saltas el pico de consumo de CPU e I/O del proceso de inicialización (Boot Storm) y destruyes el costo de los kernels individuales."

**Respuesta de Claude:**

Su matemática es más rigurosa que la mía y la acepto. Sus números:
- ~165 MB RSS efectivo por VM ociosa (vs mis 100–150 MB de v1)
- Descontar 8 GB para Postgres con 100 DBs activas (yo no lo había contabilizado)
- 22 GB netos / 165 MB = ~133 VMs techo teórico

Mi v2 ajusta a **150–200 MB RSS por VM** (un poco más conservador que sus 165 MB para dejar margen) y **80–110 Odoos activos** con tráfico real (no las 133 puramente ociosas).

**Acepto su corrección sobre la motivación:** el valor principal del snapshot/restore NO es "compartir páginas de Python" (Python las ensucia), sino:
1. Amortizar el costo del kernel duplicado N veces (kernel TEXT + rodata + libs read-only → sí compartibles)
2. Eliminar el boot storm (100 VMs no arrancan kernel desde cero simultáneamente)
3. KSM en background recupera lo que CoW divergió pero sigue siendo idéntico en contenido

**Estado:** **acordado**, números corregidos en TL;DR, §3.2, §4. La meta de 100 Odoos en 32 GB sigue siendo alcanzable pero con poco margen — si el cliente quisiera 150+, hay que mover a host de 64 GB.

---

### Pregunta 4 del equipo — "Modificaciones obligatorias al plan"

> "Si decides dar luz verde al proyecto, tu plan de fases necesita tres cambios drásticos para no fracasar:
> 1. Inyección de código para mitigar CoW: en la Fase 2, antes de tomar el snapshot, debes congelar el recolector de basura de Python (`gc.disable()`) o forzar un `gc.freeze()` (disponible en Python 3.7+).
> 2. KSM no es opcional, es el salvavidas: en la comparación de la sección 8 dices que no son excluyentes. Error: KSM es mandatorio.
> 3. Los Addons no pueden ir por virtio-fs en el Snapshot: para que el snapshot funcione sin romper los anillos de virtio, el template del snapshot no debe tener montado el directorio de addons. El snapshot debe tomarse con el core puro de Odoo, y el agente de inicialización debe realizar el montaje limpio de virtio-fs desde cero después del restore."

**Respuesta de Claude — punto por punto:**

1. **`gc.disable()` + `gc.freeze()` antes del snapshot** → **acepto al 100 %.** Es barato y ayuda en el margen (reduce escrituras del GC mark/sweep). NO ataca el refcount (que es el problema mayor), pero sumar. Movido al paso 3 de la captura del snapshot en §3.1. **Adicionalmente propongo:** si Python ≥ 3.12, marcar también la registry + módulos como immortal vía PEP 683 — esto sí ataca el refcount para objetos seleccionados.

2. **KSM mandatorio (no opcional)** → **acepto al 100 %.** Movido de Fase 5 a Fase 1 en §7. Listado como mitigación crítica en §6. Reformulado en §8 como "parte del modelo, no extra".

3. **Snapshot sin addons / virtio-fs montados** → **acepto al 100 %.** Snapshot es core-only: kernel + initramfs + Python + `import odoo`, **sin module registry cargado, sin virtio-fs**. Init-agent post-restore monta virtio-fs vía hot-plug y dispara el module load. Corregido en §3.1 y §3.2. Cuesta ~1–3 s de boot extras (registry load post-mount) — acepto el trade-off porque la alternativa (virtio-fs en snapshot) crashea el guest.

**Estado:** **los tres cambios acordados e incorporados.**

---

### Puntos abiertos para la 2ª rueda de revisión

| # | Pregunta | Propuesta de Claude | Esperando feedback del equipo |
|---|---|---|---|
| 1 | ¿Versión exacta de Python en el rootfs Odoo 19? | Confirmar en Fase 0 con `python3 --version` dentro del guest. Si ≥ 3.12 → aplicar PEP 683 immortal. | OK / preferencia distinta |
| 2 | ¿Aceptamos boot 1.5–3 s en vez de 200 ms? | Sí, es el costo de no romper virtio-fs. Sigue siendo 2–3× mejor que hoy. | OK / preferimos otro trade-off |
| 3 | ¿Target de densidad es estrictamente 100 Odoos o aceptable 80–110? | 80–110 activos con margen para tráfico real. 100 ociosos posibles. | OK / requerimos 100 garantizados |
| 4 | ¿Pre-warm pool en Fase 5 vale el trabajo extra? | Sí, baja perceptible <100 ms vs 1.5–3 s. Aumenta complejidad. | Sí / no / diferir |
| 5 | ¿Confidential computing (SEV/TDX) es requerido en algún roadmap? | Incompatible con snapshot CoW. Si se requiere, ese subset corre con boot normal. | No requerido / sí, plan B necesario |
| 6 | ¿Acordamos los 3 (NKR, equipo, Claude) antes de empezar Fase 0? | Sí, sin acuerdo total no se procede. | Acordado / objeciones |
| 7 | ¿Hay alguna feature de NKR que olvidé considerar en el snapshot? (balloon stats vq, watchdog REL_OD-aware, métricas guest_mem, etc.) | Todas conservadas — balloon state va en snapshot, watchdog sin cambio, métricas sin cambio. | Confirmar / faltan |
| 8 | ¿El daemon NKR debe poder coexistir con cells "viejas" (boot normal) y nuevas (snapshot) en el mismo host? | Sí, flag `NKR_USE_SNAPSHOT` por cell. Fallback automático si snapshot inválido. | OK / preferimos all-or-nothing |

**Sin acuerdo de los 3 en estos 8 puntos, no se procede a Fase 0.**

---

## 11. Segunda rueda de revisión — respuestas del equipo (2026-05-17)

El equipo respondió los 8 puntos abiertos de §10 y agregó un nuevo punto crítico que yo había omitido. Cierre de la 2ª rueda: **luz verde a Fase 0** con misión alterada (sin KSM) y target de densidad ajustado.

### Punto nuevo del equipo — "El elefante en la habitación: KSM ya está descartado en NKR"

> "El documento v2 hace que KSM sea mandatorio (Fase 1) para salvar el colapso del CoW. Sin embargo, tenemos que ser implacables con las restricciones de nuestra propia infraestructura: **KSM ya no va en este proyecto por problemas de compatibilidad.** Ya habíamos tomado la dura decisión de sacarlo de nuestra documentación y resúmenes públicos porque, en la práctica, nos rompe más cosas de las que soluciona. No podemos revivir a KSM solo porque la matemática de la densidad lo necesita."

**Respuesta de Claude:**

**Aceptado al 100 %, y debo disculparme** — la `CLAUDE.md` ya decía *"KSM es una mentira: NKR usa memfd + MAP_SHARED. El kernel rechaza MADV_MERGEABLE en VMAs compartidos"*. Yo lo interpreté como "KSM no funciona para la región DAX shared, pero podría aplicarse a una región privada nueva". Eso técnicamente es cierto, pero **políticamente / arquitecturalmente** el equipo ya había cerrado el tema: KSM ROMPE COSAS en NKR y queda fuera, punto. Lo reviví en v2 sin chequear esa decisión preexistente.

**Implicaciones que el equipo señala correctamente:**

- RSS por VM post-restore empieza en ~100 MB.
- Tras horas de tráfico, refcount CoW separa el heap completo. RSS estabiliza en ~250–300 MB.
- En 32 GB (menos OS + Postgres), techo real **~60–75 Odoos activos**, no 100.
- **La meta del whitepaper (100 en 32 GB) no es alcanzable con este hardware sin KSM.**
- **El proyecto sigue valiendo, pero por velocidad de boot — no por densidad.**

**Cambios incorporados en v3:**
- Todas las menciones de "KSM mandatorio" eliminadas de §3.2, §6, §7, §8, §9.
- Números de RSS / densidad ajustados en TL;DR y §4.
- Recomendación de §8 reposicionada: el valor es boot speed, no densidad.
- Misión de Fase 0 alterada (§7): medir sangrado de CoW bajo carga real sin re-fusión disponible.

**Estado:** **acordado**, doc actualizado a v3. KSM permanentemente fuera del scope del proyecto.

---

### Respuestas del equipo a los 8 puntos abiertos de §10

| # | Pregunta | Respuesta del equipo | Estado v3 |
|---|---|---|---|
| 1 | Versión Python en rootfs Odoo 19 | "Confirmarlo en Fase 0 es crítico. Si es Python 3.12, immortal objects (PEP 683) son nuestra única herramienta para frenar la degradación sin KSM." | **Acordado.** Step explícito en Fase 0. |
| 2 | ¿Boot 1.5–3 s aceptable vs 200 ms? | "Aceptado. Es el único camino arquitectónicamente sano. Un arranque de 2 segundos para un ERP pesado sigue siendo magia negra en la industria." | **Acordado.** Confirmado en §4. |
| 3 | ¿Target estricto 100 Odoos o aceptable 60–110? | "Tenemos que ser honestos con el negocio. Sin KSM, el target garantizado debe bajarse a ~60–70 activos. Si el cliente exige 100, necesitamos saltar a 64 GB de RAM. La física es la física." | **Acordado.** Target oficial: 60–75 activos. 100 requiere 64 GB. Reflejado en TL;DR, §4, §8, §9. |
| 4 | Pre-warm pool en Fase 5 | "Diferir indefinidamente. Añade demasiada complejidad de orquestación por un ahorro de ~1 segundo. Concentrémonos en que el flujo principal no explote." | **Acordado.** Removido de Fase 5. |
| 5 | Confidential computing (SEV/TDX) | "Ignorar. Si algún cliente Enterprise paranoico lo pide en 2027, le hacemos un flag para que use el boot normal sin snapshot." | **Acordado.** Fuera de scope. |
| 6 | Acuerdo de los 3 antes de Fase 0 | "Acuerdo total sobre la viabilidad mecánica, sujeto a la eliminación de KSM de la arquitectura." | **Acordado.** KSM removido en v3. Luz verde. |
| 7 | ¿Features de NKR olvidadas? | "Revisa bien la enumeración de los slots PCI al hacer el hot-plug del virtio-fs post-restore. El guest pre-pausado tiene un árbol PCI; si inyectas 4 shares de virtio-fs, asegúrate de que el guest los mapee sin pisar los IRQs de red." | **Acordado.** Agregado como riesgo medio #9 en §6. Reservar slots PCI 8–11 para hotplug virtio-fs en el snapshot. |
| 8 | Flag `NKR_USE_SNAPSHOT` por cell | "Aprobado. Es la estrategia de rollout perfecta y nos da un botón de pánico gratuito." | **Acordado.** Conservado en Fase 4. |

---

### Decisión final de la 2ª rueda

> "Autorizo el inicio de la Fase 0 (Spike de 3 días), pero con una misión alterada: tu objetivo en `snapshot_poc.rs` ya no es ver si KSM nos salva. Tu objetivo es medir cuán rápido y cuán profundo es el sangrado del CoW cuando bombardeas la instancia restorada con 100 peticiones HTTP concurrentes. Si el RSS se estabiliza en ~250 MB por VM sin KSM, tenemos un producto sólido de alta disponibilidad. Si se dispara de vuelta a los 450 MB como un boot frío en menos de 10 minutos, entonces el esfuerzo del Snapshot/Restore solo sirve para acelerar el arranque, pero no nos ahorra un solo byte de RAM a largo plazo."

**Acuerdo cerrado de los 3 (NKR / equipo / Claude):** Fase 0 arranca cuando el equipo confirme calendario. Misión: bombardeo HTTP + medición de RSS en T=0 / T=10 min / T=60 min + confirmación de versión Python. Output go/no-go basado en el umbral de ~300 MB estable a T=60 min.

**No hay más rounds de revisión antes de Fase 0.** Si Fase 0 trae resultados inesperados (RSS dispara a 450 MB, o Python es 3.8 sin alternativa, etc.), se abre una 3ª rueda para reconsiderar.

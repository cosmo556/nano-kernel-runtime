# Fase 0 — Reporte de hallazgos (PARADA TEMPRANA → CANCELACIÓN OFICIAL)

**Fecha:** 2026-05-17
**Estado:** ✅ **PROYECTO CANCELADO OFICIALMENTE por el equipo (2026-05-17, post-decisión final §"Decisión final del equipo").** Reporte se archiva como **caso de estudio interno de excelencia en ingeniería**: cómo la telemetría temprana (~30 min) previno 3–4 semanas de refactor masivo sin ROI.
**Tiempo invertido:** ~30 min de medición (snapshot_poc.rs **nunca se escribió** — los datos baseline bastaron para tomar la decisión).

---

## TL;DR

**Las mediciones de baseline del host actual desmienten la premisa central del proyecto Snapshot/Restore.** La RAM "ahorrable" que justificaba el refactor de 3–4 semanas ya está mayoritariamente ahorrada hoy gracias al DAX rootfs. El proyecto sigue teniendo valor para velocidad de boot, pero el ROI cae drásticamente.

**Recomiendo abrir una 3ª rueda de revisión con el equipo antes de continuar con código.**

---

## Datos medidos (host actual, 21 VMs corriendo, KSM=0)

### Hallazgo #1 — Python 3.12.3 en rootfs Odoo 19

- ✅ `gc.freeze()` disponible (Python 3.7+)
- ⚠️ **PEP 683 immortal objects en 3.12 NO se puede aplicar a objetos arbitrarios.** Solo a sentinels pre-existentes (None, True, False, small ints, interned strings). Para marcar la registry de Odoo como immortal habría que usar `PyUnstable_Object_EnableDeferredRefcount` que es **Python 3.14+** (todavía no API estable). Confirmado vivo:
  ```
  refcount of 0 antes: 4294967295
  refcount of 0 después de 1000 refs: 4294967295  → immortal sentinel OK
  ```
  Pero esto solo afecta a unos pocos objetos predefinidos, NO al ~95% del heap de Odoo.

**Implicación:** la única mitigación real del refcount CoW post-restore sigue siendo `gc.freeze()`, que **NO toca el refcount** (solo el GC scan). v3 era optimista al asumir que PEP 683 daba "30-50 MB extras".

### Hallazgo #2 — Baseline real de RAM por VM (lo crítico)

Medido con `smaps_rollup` en cada proceso `nkr run` de los tenants Odoo corriendo HOY:

| Tenant | Tier | RAM committed | RSS real | **PSS real** | Shared (DAX) | Private (CoW) |
|---|---|---:|---:|---:|---:|---:|
| smoke-1/2/3 | dev (idle) | 1300 MB | 260 MB | **223 MB** | 69 MB | 191 MB |
| cold-t1, cold-2 | dev (idle) | 1300 MB | 260 MB | **223 MB** | 68 MB | 191 MB |
| ksm-check, phase1 | dev (idle) | 1300 MB | 260 MB | **223 MB** | 69 MB | 191 MB |
| timing-t1, final-1 | dev (idle) | 1300 MB | 260 MB | **223 MB** | 69 MB | 191 MB |
| stag-t1, stag-ok | staging (idle) | 1024 MB | 215 MB | **197 MB** | 31 MB | 183 MB |
| stg3 | staging (idle) | 1024 MB | 251 MB | **214 MB** | 69 MB | 181 MB |
| prod-t1 | prod, workers=2 | 2048 MB | 386 MB | **343 MB** | 82 MB | 304 MB |
| ent-t1 | enterprise (con uso) | 2048 MB | 525 MB | **469 MB** | 108 MB | 417 MB |
| ent-t2/t3 | enterprise (idle) | 2048 MB | 395 MB | **352 MB** | 82 MB | 312 MB |
| **intech-devp** | **dev (CON TRÁFICO REAL)** | 1300 MB | **637 MB** | **524 MB** | 221 MB | 416 MB |

**PSS = Proportional Set Size**. Ya descuenta lo compartido vía DAX rootfs. Es el número honesto de "cuánto le cuesta esta VM al host".

### Hallazgo #3 — Host real es 64 GB, no 32 GB

`/proc/meminfo` reporta **65697896 kB = 64 GB MemTotal**. El whitepaper apunta a "100 Odoos en 32 GB" pero el host actual es el doble. Mi v3 estaba haciendo math con 32 GB.

Con 21 VMs hoy → 6.2 GB total RSS, 5.4 GB total PSS. Espacio de sobra.

### Hallazgo #4 — KSM efectivamente OFF en producción

`/sys/kernel/mm/ksm/run = 0`, `pages_sharing = 0`. Confirma la decisión arquitectónica del equipo. No hay sharing residual de KSM contaminando las mediciones.

---

## El problema: la matemática del proyecto colapsa con estos números

### Lo que v3 prometía vs realidad medida

| Métrica | v3 prometía (post-snapshot) | Realidad HOY (sin snapshot) | Ganancia real del refactor |
|---|---:|---:|---:|
| RSS por VM dev ociosa | 150–200 MB | **260 MB** | sería ~60–110 MB menos |
| **PSS** por VM dev ociosa | (no medía) | **223 MB** | sería ~25–75 MB menos |
| RSS por VM con tráfico estable | 250–300 MB | **524 MB PSS / 637 MB RSS** | sería ~200–340 MB menos |
| Boot por VM | 1.5–3 s | 5 s | **2–3× speedup** (sigue siendo real) |
| Densidad en 32 GB | 60–75 Odoos activos | ~22 Odoos hoy | sería ~50–60 Odoos |
| Densidad en **64 GB** (host real) | (no medía) | ~22 Odoos hoy | sería ~100–120 Odoos |

### Lo brutal

1. **El RSS hoy ya es 260 MB en ociosas.** Mi v3 vendía bajar a 150–200 MB — irreal porque ya estamos en ese rango. El DAX rootfs **YA HACE** la mayor parte del sharing (69 MB compartidos por VM dev de los 260 totales = 27%).

2. **El refcount CoW degradation predicho por el equipo en la 1ª revisión se VE EN VIVO en `intech-devp`** (la única VM dev con tráfico real): subió a 524 MB PSS / 416 MB Private_Dirty. Eso es exactamente lo que pasaría en un Odoo restorado **sin importar** que venga de snapshot o de boot frío — el patrón es el mismo.

3. **El "ahorro de densidad" del snapshot/restore se reduce a 25–75 MB de PSS por VM ociosa** (lo que sea que el kernel TEXT + libs del template puedan compartir adicional al DAX rootfs ya activo). En VMs con tráfico real, el ahorro se evapora al rato por el refcount.

4. **En el host real de 64 GB ya cabrían ~100 Odoos HOY sin tocar nada** (estimación: 64 GB − 2 GB OS − 8 GB Postgres = 54 GB / 260 MB ≈ 207 idle, ~85–100 con tráfico). El whitepaper de "100 en 32 GB" era para hardware más chico — y hoy ya estamos con margen.

### Lo que el snapshot/restore SÍ sigue entregando (lo único que vale)

- **Boot 5 s → 1.5–3 s** (2–3× speedup). Real.
- **Eliminación del boot storm** al levantar la cell. Real pero infrecuente.
- **POST /actions {restart} más rápido** (30–60 s → 5–15 s). Real.

**Pero NO entrega:**
- ~~Densidad significativamente mejor~~ (el DAX ya hace la mayoría del sharing)
- ~~100 Odoos en 32 GB~~ (el host real es 64 GB y la mejora no llega a ser dramática)
- ~~Justificación de 3–4 semanas de refactor masivo de vmm.rs~~

---

## Comparación con las alternativas baratas (§12 del doc)

| Opción | Ahorro RAM/VM | Esfuerzo | Riesgo | Boot speedup |
|---|---:|---|---|---|
| **A — Initramfs reducido** | ~2–3 MB | 1 día | bajo | minimal |
| **B — Kernel diet agresivo** | ~10–25 MB | 1–2 días | bajo | minimal |
| **A + B combinados** | **~12–28 MB** | **2–3 días** | bajo | minimal |
| **Snapshot/Restore completo** | **~25–75 MB ociosas, ~0 con tráfico** | **3–4 semanas** | alto | **2–3×** |

**A + B entregan el 50% del ahorro de RAM del snapshot a un 10% del costo y sin riesgo arquitectónico.** La diferencia real entre los dos caminos es la velocidad de boot.

---

## Mi recomendación al equipo

**Abrir 3ª rueda de revisión** con esta data antes de invertir más tiempo. Tres caminos posibles:

### Camino 1 — STOP-WORK del snapshot/restore. Solo hacer A + B.
- Ahorro: ~12–28 MB por VM
- Esfuerzo: 2–3 días
- Riesgo: bajo
- Boot speedup: ninguno (sigue 5 s)
- **Recomendado si el negocio acepta los 5 s de boot actuales** y solo quiere optimizar RAM.

### Camino 2 — Continuar snapshot/restore SOLO si el negocio valora el boot speedup en 3-4 semanas de trabajo.
- Honestamente: cuestionable. POST /instances es async desde v1.6.4 (panel no espera el boot). El boot storm es ocasional.
- Si la decisión es continuar, **al menos hacer A + B primero** para que el snapshot sea sobre el mejor baseline posible.

### Camino 3 — Híbrido pragmático: A + B + Restart speedup.
- Hacer A + B (3 días)
- Hacer un "warm restart" más simple que el snapshot completo: tras `POST /actions {restart}`, en vez de full kill + boot fresh, mantener el guest paused con su DB connection cerrada y resumir tras restart. Más simple que snapshot inter-VM.
- Ahorro RAM: ~12–28 MB
- Boot/restart speedup: marginal en POST /instances, **significativo** en POST /actions {restart} (que es la única operación donde el usuario humano espera hoy)
- Esfuerzo: ~1 semana
- Riesgo: medio

**Mi voto: Camino 3, con prioridad A + B primero (días 1–3) + evaluación de restart-warm (semana 1–2).**

---

## Decisión bloqueante para el equipo

Necesito acuerdo antes de tocar más código. Preguntas concretas:

1. **¿Aceptan que el target "100 Odoos en 32 GB" del whitepaper era para hardware diferente al actual de 64 GB?** Si sí, la motivación de densidad del snapshot se debilita aún más.

2. **¿La velocidad de boot (5 s → 1.5–3 s) justifica 3–4 semanas de refactor masivo?** Mi opinión: probablemente NO, porque POST /instances es async y la UX ya está cubierta.

3. **¿Hacemos A + B sí o sí, independiente de la decisión sobre snapshot?** Mi recomendación: SÍ — 12–28 MB gratis sin riesgo.

4. **¿Camino 1, 2, o 3?**

**Sin respuesta del equipo a estas 4 preguntas, no continúo con código.**

---

## Anexo — comandos para reproducir las mediciones

```bash
# Versión Python en rootfs
mount -o ro,loop /mnt/nkr/images/odoo19.ext4 /tmp/inspect
/tmp/inspect/usr/bin/python3 --version
umount /tmp/inspect

# RSS / PSS de todas las VMs Odoo
for pid in $(pgrep -f "nkr run --name odoo-v19-"); do
    name=$(ps -p $pid -o args= | grep -oE -- "--name [^ ]*" | awk '{print $2}')
    rss=$(awk '/^Rss:/ {print $2; exit}' /proc/$pid/smaps_rollup)
    pss=$(awk '/^Pss:/ {print $2; exit}' /proc/$pid/smaps_rollup)
    echo "$name RSS=${rss}KB PSS=${pss}KB"
done

# Confirmar KSM OFF
cat /sys/kernel/mm/ksm/run
cat /sys/kernel/mm/ksm/pages_sharing
```

---

## Decisión final del equipo (2026-05-17)

Respuestas a las 4 preguntas bloqueantes:

### 1. ¿Target "100 Odoos en 32 GB" superado por la realidad de hardware?

> "Sí, absolutamente. El hallazgo de que el host real tiene 64 GB cambia las reglas de la física. Estábamos diseñando un motor de Fórmula 1 para escapar de un embotellamiento imaginario. Con 64 GB y el PSS actual, el host respira tranquilo. El target de '100 en 32 GB' queda oficialmente catalogado como un ejercicio teórico superado por la realidad del hardware aprovisionado."

### 2. ¿La velocidad de boot (5 s → 1.5–3 s) justifica 3–4 semanas de refactor?

> "Definitivamente NO. Si la ruta POST /instances ya es asíncrona desde la v1.6.4, el usuario final no percibe esos 5 segundos. Desde el punto de vista del producto, gastar un mes de ingeniería para rascar 2.5 segundos invisibles en un proceso de despliegue es un despilfarro de recursos. La experiencia de usuario ya está resuelta."

### 3. ¿Hacemos A + B sí o sí?

> "SÍ. Luz verde inmediata. Reducir el Initramfs (Opción A) y aplicar una dieta agresiva al Kernel (Opción B) son victorias tácticas. Bajar el PSS en 12–28 MB por VM de forma gratuita, determinista y con bajo riesgo es higiene de infraestructura básica. Hazlo y fusiónalo."

### 4. ¿Camino 1, 2 o 3?

> "Mi voto definitivo es por el **Camino 1** (STOP-WORK total al Snapshot + Ejecutar A y B). El Camino 3 (A + B + Warm Restart) suena tentador para el POST /actions {restart}, pero sigue añadiendo complejidad de estado al orquestador. Si un tenant reinicia su Odoo hoy en 5-8 segundos con un pkill (vía REL_OD que implementamos antes), eso ya es excepcionalmente rápido para un ERP. Mantengamos la arquitectura de NKR inmaculada, lineal y sin estado pausado a menos que un cliente real se queje formalmente de los tiempos de reinicio."

### Veredicto final del equipo

> "Queda oficialmente cancelado el refactor de Snapshot/Restore. Archiva este documento de Fase 0 como un caso de estudio interno de excelencia en ingeniería ('Cómo la telemetría temprana previene la deuda técnica'). Tus próximos pasos son claros y libres de riesgo:
> 1. Poda el Kernel (.config).
> 2. Adelgaza el Initramfs.
> 3. Actualiza la doctrina para reflejar que el baseline PSS actual es el estándar de oro."

### Lecciones del caso (autopsia técnica del equipo)

> "**El Triunfo de DAX**: Medir un PSS de 223 MB y ver que DAX ya está deduplicando ~69 MB por instancia demuestra que tu arquitectura base ya es de élite. Estás obteniendo el 80% de los beneficios de memoria compartida sin ninguna de las fragilidades de KSM o Snapshot.
>
> **El Baño de Realidad de Python**: Descubrir que PEP 683 en Python 3.12 no permite hacer inmortales objetos arbitrarios sin la API inestable de 3.14+ es un hallazgo crítico. Confirmaste en vivo que el Private_Dirty se dispara a 416 MB bajo carga real (intech-devp). El sangrado del Reference Counting es real, rápido y destructivo para la memoria compartida.
>
> **El PSS no miente**: Ver la diferencia entre el RSS (637 MB) y el PSS (524 MB) bajo carga demuestra exactamente cuánto le cuesta cada Odoo al host. Con 64 GB de RAM, tienes pista libre."

### Plan ejecutivo aprobado

| Tarea | Owner | Esfuerzo | Riesgo |
|---|---|---:|---|
| **§12.1.A — Initramfs reducido** | NKR / Claude | ~1 día | Bajo |
| **§12.1.B — Kernel diet `.config`** | NKR / Claude | 1–2 días | Bajo |
| **§12.1.C — EROFS spike (benchmark de 1 día)** | Pendiente confirmación equipo | ~1 día | A validar |
| Actualizar `CLAUDE.md` con "baseline PSS = estándar de oro" | NKR / Claude | <1 hora | Cero |
| ~~Snapshot/Restore completo~~ | ❌ **CANCELADO** | — | — |

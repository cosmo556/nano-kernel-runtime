#!/bin/bash
# =============================================================================
# switch-kernel.sh — Fuerza a los nkr-compose.yml a usar NanoLinux (ELF)
# =============================================================================
# Ejecutar DESPUÉS de "make install"
# Uso: ./switch-kernel.sh
# =============================================================================
set -e

INSTALL_DIR="/mnt/nkr/kernel"
KERNEL_PATH="${INSTALL_DIR}/nanolinux"

if [ ! -f "$KERNEL_PATH" ]; then
    echo "❌ Error Crítico: No existe $KERNEL_PATH"
    echo "   Ejecuta 'make install' en el directorio del kernel primero."
    exit 1
fi

echo "▶ Desplegando núcleo maestro: $KERNEL_PATH ($(du -sh "$KERNEL_PATH" | cut -f1))"
echo ""

# Buscar todos los orquestadores en /home
COMPOSES=$(find /home -name "nkr-compose.yml" 2>/dev/null)

if [ -z "$COMPOSES" ]; then
    echo "⚠  No se encontraron archivos nkr-compose.yml en /home"
    exit 0
fi

echo "▶ Orquestadores de micro-VMs encontrados:"
echo "$COMPOSES"
echo ""

for COMPOSE in $COMPOSES; do
    # Verifica si el campo kernel existe respetando espacios previos
    if grep -q "^[[:space:]]*kernel:" "$COMPOSE"; then
        # Extrae el valor antiguo para el log
        OLD=$(grep "^[[:space:]]*kernel:" "$COMPOSE" | head -1 | sed 's/^[[:space:]]*kernel: *//')
        
        # Inyecta el nuevo path manteniendo la indentación YAML intacta
        sed -i "s|^\([[:space:]]*\)kernel:.*|\1kernel: ${KERNEL_PATH}|g" "$COMPOSE"
        
        echo "✓ $COMPOSE: $OLD → $KERNEL_PATH"
    else
        echo "⚠  $COMPOSE: no tiene campo 'kernel:' definido (ignorado)"
    fi
done

echo ""
echo "============================================================"
echo "✅ Migración a NanoLinux completada en todos los orquestadores."
echo "Para aplicar el cambio y activar el DAX en las instancias:"
echo "  cd /home/cosmo99pruebas/odoo8070 && sudo nkr compose down && sudo nkr compose up"
echo "============================================================"
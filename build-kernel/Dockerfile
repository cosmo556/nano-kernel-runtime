# =============================================================================
# NKR NanoLinux Builder — Entorno efímero de compilación
# =============================================================================
FROM debian:bookworm-slim

ARG KERNEL_VERSION=6.6.87

# 1. Dependencias estrictas (incluyendo 'dwarves' para pahole)
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential bc bison flex libssl-dev libelf-dev wget \
    ca-certificates cpio xz-utils python3 rsync binutils dwarves \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# 2. Descarga segura, extracción y limpieza (Modo 100% Silencioso)
RUN echo "▶ Descargando Kernel ${KERNEL_VERSION} (Silencioso, esto tomará unos segundos)..." && \
    wget -q "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz" && \
    echo "▶ Descomprimiendo código fuente..." && \
    tar -xf "linux-${KERNEL_VERSION}.tar.xz" && \
    rm "linux-${KERNEL_VERSION}.tar.xz" && \
    ln -s "linux-${KERNEL_VERSION}" linux

# 3. Inyectar tu script maestro y disparar la compilación monolítica
COPY build-nanolinux.sh /build/linux/build-nanolinux.sh
RUN cd /build/linux && \
    chmod +x build-nanolinux.sh && \
    ./build-nanolinux.sh

# 4. Entrega del artefacto final
CMD ["sh", "-c", "cp /build/linux/nanolinux /output/nanolinux && echo '✅ NanoLinux copiado a /output/nanolinux'"]
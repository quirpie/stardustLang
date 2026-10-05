# syntax=docker/dockerfile:1
# Imagen de stardust-server para Fly.io (ver docs/deploy-fly.md).

# ── Fase 1: compilación ─────────────────────────────────────────────────────
FROM rust:1.98-bookworm AS build

# Debe coincidir EXACTAMENTE con `wasm-bindgen = "=…"` de Cargo.toml.
ARG WASM_BINDGEN_VERSION=0.2.126

RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 \
 && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown \
 && cargo install --locked wasm-bindgen-cli --version ${WASM_BINDGEN_VERSION}

WORKDIR /src
COPY . .

# Las cachés de BuildKit aceleran los deploys siguientes; `target` vive en la
# caché, así que los artefactos se copian a /out dentro del mismo RUN.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    mkdir -p /out \
 && bash web/build-playground.sh /out/playground.html \
 && cargo build --release --locked --features server --bin stardust-server --bin stardust \
 && cp target/release/stardust-server target/release/stardust /out/

# ── Fase 2: ejecución ───────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /out/stardust-server /out/stardust /usr/local/bin/
COPY --from=build /out/playground.html /out/runner.html /app/web/

ENV PORT=8080 \
    STARDUST_DATA=/data \
    STARDUST_WEB=/app/web

EXPOSE 8080
CMD ["stardust-server"]

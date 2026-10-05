# Despliegue en Fly.io

Estado: **propuesta**. Describe cómo se despliega el servidor de StardustLang (`stardust-server`,
aún no implementado; diseño en [`servidor.md`](servidor.md)) en Fly.io, y fija el contrato que ese
servidor debe cumplir con el entorno para que el despliegue funcione tal cual.

## Qué se despliega

```text
                         ┌──────────────── máquina Fly (1) ────────────────┐
 IA / CLI ──HTTPS──▶     │  stardust-server  (Rust, axum)                  │
 navegador ─HTTPS──▶ Fly │    ├─ API: installProgram / updateProgram / …   │
                   proxy │    ├─ playground.html (estático, embebido)      │
                         │    └─ runner (otro Host, mismo proceso)         │
                         │  /data  (volumen)                               │
                         │    └─ stardust.db  (SQLite, WAL)                │
                         └───────────────┬─────────────────────────────────┘
                                         │ réplica continua (etapa 2)
                                         ▼
                               Tigris (S3) vía Litestream
```

- **Un único binario**, `stardust-server`, sirve la API, el playground y el runner. Los programas
  **no se ejecutan en el servidor**: corren en el navegador de quien los abre (VM en WASM). El
  servidor guarda, versiona y **valida** (con `check.rs` y `lang::compile`, el mismo código de la VM).
- **Estado en SQLite** sobre un volumen de Fly. Sin base de datos gestionada ni servicios externos
  en la etapa 1.
- **Una sola máquina.** Un volumen de Fly se monta en una sola máquina y SQLite no admite varios
  escritores en red, así que no se escala horizontalmente. Para el uso previsto (registro de
  programas, poca escritura) una máquina pequeña sobra.

## Contrato del servidor con el entorno

Lo que `stardust-server` debe cumplir para que este despliegue funcione. Si algo cambia en la
implementación, se cambia aquí también.

| Aspecto | Contrato |
|---|---|
| Binario | `[[bin]] stardust-server` en el mismo crate, detrás del feature `server` (axum, tokio, rusqlite con `bundled`). El CLI `stardust` y el build WASM no arrastran esas dependencias. |
| Escucha | `0.0.0.0:$PORT` (por defecto `8080`). HTTP plano: Fly termina TLS. |
| Datos | Todo el estado bajo `$STARDUST_DATA` (por defecto `./data`). Base: `$STARDUST_DATA/stardust.db`, en modo WAL. Nada de estado fuera de ese directorio. |
| Estáticos | `playground.html` y el runner se leen de `$STARDUST_WEB` (por defecto `./web`). |
| Salud | `GET /healthz` → `200` solo si la base abre y responde `SELECT 1`. Sin autenticación. |
| Parada | Ante `SIGINT` o `SIGTERM`: deja de aceptar conexiones, termina las peticiones en curso, hace checkpoint del WAL y sale en < 10 s. |
| Orígenes | `STARDUST_PUBLIC_ORIGIN` (app y API) y `STARDUST_RUNNER_ORIGIN` (runner). El servidor enruta por cabecera `Host`: en el host del runner solo sirve `/run/…`, con `Content-Security-Policy: frame-ancestors <PUBLIC_ORIGIN>`. Si ambos coinciden, arranca en «modo desarrollo» y lo avisa en el log. |
| Ejecución (`/run`, etapa 3) | Lanza el binario `stardust` (en el `PATH`) como proceso hijo, con `--no-net`, un `--data` temporal y límites de tiempo y memoria. Ver [`servidor.md`](servidor.md). |
| Arranque | Crea la base y aplica migraciones al arrancar. Las migraciones solo añaden (columnas o tablas nuevas), nunca rompen la versión anterior: así un rollback de imagen no se encuentra con una base incompatible. |
| Admin | `STARDUST_ADMIN_TOKEN` (secreto) habilita la gestión de usuarios. También hay subcomandos `stardust-server user add/list/token/disable`, que actúan sobre la base directamente (para usarlos por `fly ssh console`). |
| Logs | A stdout, una línea por evento; nivel por `RUST_LOG`. Nunca se registran tokens ni cuerpos de programas. |

## Ficheros en el repositorio

```text
Dockerfile
.dockerignore
fly.toml
deploy/
  entrypoint.sh       # etapa 2: restaura y replica con Litestream
  litestream.yml      # etapa 2
.github/workflows/deploy.yml   # opcional: deploy al hacer push a main
```

### `Dockerfile`

Dos fases: una compila el playground (WASM) y el servidor; la otra es una imagen Debian mínima
con el binario y el HTML.

```dockerfile
# syntax=docker/dockerfile:1

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
COPY --from=build /out/playground.html /app/web/playground.html

ENV PORT=8080 \
    STARDUST_DATA=/data \
    STARDUST_WEB=/app/web

EXPOSE 8080
CMD ["stardust-server"]
```

Notas:
- La imagen de Rust se fija a la versión de desarrollo (`rustc 1.98.1`). Al subir de versión en
  local, se sube aquí también.
- `cargo install wasm-bindgen-cli` tarda unos minutos, pero queda en una capa cacheada y solo se
  repite si cambia `WASM_BINDGEN_VERSION`.
- `--locked` exige que `Cargo.lock` esté al día: el build falla en vez de resolver otras versiones.
- El proceso corre como root porque el volumen se monta con propietario root. Es aceptable en una
  micro-VM de Fly; si se quiere endurecer, se añade un usuario y un `chown /data` en el entrypoint.

### `.dockerignore`

```text
target/
web/pkg/
web/*.html
!web/*.template.html
dist/
data/
.git/
*.db
*.db-wal
*.db-shm
```

Evita mandar al builder gigas de `target/` y que un HTML generado en local tape el que se genera
en la imagen.

### `fly.toml`

```toml
app = "stardust"            # nombre global en Fly: cámbialo si está ocupado
primary_region = "cdg"      # ver `fly platform regions`; elige la más cercana a los usuarios

kill_signal = "SIGINT"
kill_timeout = "10s"        # = margen de parada del contrato

[build]
  dockerfile = "Dockerfile"

[env]
  RUST_LOG = "info"
  STARDUST_PUBLIC_ORIGIN = "https://stardust.example.com"
  STARDUST_RUNNER_ORIGIN = "https://run.example-apps.com"

[[mounts]]
  source = "stardust_data"
  destination = "/data"

[http_service]
  internal_port = 8080
  force_https = true
  auto_stop_machines = "stop"     # se apaga sin tráfico…
  auto_start_machines = true      # …y el proxy la arranca con la primera petición
  min_machines_running = 0

  [http_service.concurrency]
    type = "requests"
    soft_limit = 200
    hard_limit = 250

  [[http_service.checks]]
    grace_period = "10s"
    interval = "30s"
    method = "GET"
    path = "/healthz"
    timeout = "5s"

[[vm]]
  size = "shared-cpu-1x"
  memory = "256mb"
```

Decisiones:
- **Apagado automático** (`auto_stop_machines = "stop"`, `min_machines_running = 0`): sin tráfico
  la máquina no cuesta CPU ni RAM, solo el volumen. A cambio, la primera petición tras un rato
  tarda en torno a un segundo más (arranque de la micro-VM y del binario). Si molesta, se pone
  `min_machines_running = 1`.
- **256 MB** bastan para axum + SQLite + validar programas (etapas 1 y 2). Al activar `/run`
  (etapa 3), que lanza hasta 2 procesos `stardust` de 128 MiB cada uno, se sube a `512mb`
  (`fly scale memory 512`).
- **Sin `[deploy] strategy`**: con una sola máquina y volumen, el deploy por defecto la para y la
  arranca con la imagen nueva. Hay unos segundos de corte en cada deploy; se acepta. `bluegreen` no
  está disponible con volúmenes.

## Primer despliegue, paso a paso

Requisitos: cuenta en Fly.io con tarjeta añadida, y `flyctl` instalado
(`powershell -Command "iwr https://fly.io/install.ps1 -useb | iex"` en Windows).

```sh
fly auth login

# 1. Crear la app SIN desplegar (usa el fly.toml del repo).
fly launch --no-deploy --copy-config --name stardust

# 2. Volumen para /data, en la misma región que primary_region.
fly volumes create stardust_data --region cdg --size 1

# 3. Secretos.
fly secrets set STARDUST_ADMIN_TOKEN="$(openssl rand -hex 32)"

# 4. Desplegar con UNA máquina (ver nota).
fly deploy --ha=false

# 5. Comprobar.
fly status
fly logs
curl -fsS https://stardust.fly.dev/healthz
```

**Nota sobre `--ha=false`:** por defecto, el primer deploy crea dos máquinas para alta
disponibilidad. Con un volumen y SQLite eso es incorrecto: la segunda máquina pediría su propio
volumen y tendría una base distinta. Si ya se crearon dos máquinas: `fly scale count 1`.

Guarda el `STARDUST_ADMIN_TOKEN` en un gestor de contraseñas antes de cerrar la terminal: Fly no
deja leer los secretos después.

### Crear el primer usuario

```sh
fly ssh console -C "stardust-server user add ana"
# → imprime el token de API de ana una sola vez
```

## Dominios y aislamiento del runner

El runner ejecuta código de terceros en el navegador de quien lo abre. Para que un programa no
pueda leer la sesión ni el IndexedDB del playground, **el runner se sirve desde otro dominio
registrable**, no solo otro subdominio:

| Uso | Ejemplo | Por qué |
|---|---|---|
| App y API | `stardust.example.com` | Sesión de usuario y playground. |
| Runner | `run.example-apps.com` | Otro sitio (eTLD+1 distinto): ni cookies ni almacenamiento compartidos. |

Los dos nombres apuntan a la misma app de Fly; el servidor los distingue por `Host`.

```sh
fly certs add stardust.example.com
fly certs add run.example-apps.com
fly ips list          # IPv6 dedicada + IPv4 compartida (gratis)
```

En el DNS de cada dominio, un `CNAME` hacia `stardust.fly.dev`. En un dominio raíz, donde no hay
CNAME, se usan registros `A` (IPv4 compartida) y `AAAA` (IPv6) con las IPs de `fly ips list`.
`fly certs show <dominio>` indica cuándo el certificado está emitido.

Después, actualiza `STARDUST_PUBLIC_ORIGIN` y `STARDUST_RUNNER_ORIGIN` en `fly.toml` y vuelve a
desplegar.

**Sin dominio propio** (solo `stardust.fly.dev`): ambos orígenes coinciden y el servidor arranca
en modo desarrollo. Sirve para probar, pero no para abrir programas de otras personas.

## Copias de seguridad

### Etapa 1: snapshots del volumen (ya incluidos)

Fly hace un snapshot diario de cada volumen y lo guarda 5 días por defecto.

```sh
fly volumes list
fly volumes snapshots list <volume-id>
fly volumes update <volume-id> --snapshot-retention 14   # más días
```

Restaurar = crear un volumen nuevo desde un snapshot y montarlo en lugar del anterior:

```sh
fly volumes create stardust_data --snapshot-id <snapshot-id> --region cdg --size 1
fly volumes destroy <volume-id-viejo>
fly deploy --ha=false
```

**Límite:** se pierde lo escrito desde el último snapshot (hasta 24 h). Además, el volumen vive en
el disco de un host concreto: si ese host falla del todo, solo quedan los snapshots.

### Etapa 2: réplica continua con Litestream

Cuando haya usuarios reales, se añade Litestream: replica el WAL de SQLite a un bucket S3 cada
pocos segundos y restaura la base al arrancar si el volumen está vacío.

```sh
fly storage create      # bucket Tigris; define BUCKET_NAME, AWS_ENDPOINT_URL_S3,
                        # AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY y AWS_REGION como secretos
```

`deploy/litestream.yml`:

```yaml
dbs:
  - path: /data/stardust.db
    replicas:
      - type: s3
        bucket: ${BUCKET_NAME}
        path: stardust.db
        endpoint: ${AWS_ENDPOINT_URL_S3}
        region: ${AWS_REGION}
```

`deploy/entrypoint.sh`:

```sh
#!/bin/sh
set -e
if [ -n "$BUCKET_NAME" ]; then
  # Volumen nuevo o vacío: recupera la base desde el bucket.
  litestream restore -if-db-not-exists -if-replica-exists /data/stardust.db
  # Litestream lanza el servidor como hijo, le reenvía las señales y sincroniza al salir.
  exec litestream replicate -exec stardust-server
fi
exec stardust-server
```

En el `Dockerfile` (fase 2), se instala Litestream y se cambia el arranque:

```dockerfile
ARG LITESTREAM_VERSION=<última de github.com/benbjohnson/litestream/releases>
ADD https://github.com/benbjohnson/litestream/releases/download/v${LITESTREAM_VERSION}/litestream-v${LITESTREAM_VERSION}-linux-amd64.tar.gz /tmp/ls.tgz
RUN tar -xzf /tmp/ls.tgz -C /usr/local/bin && rm /tmp/ls.tgz
COPY deploy/litestream.yml /etc/litestream.yml
COPY deploy/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh
CMD ["/usr/local/bin/entrypoint.sh"]
```

Comprueba el nombre exacto del tarball en la página de releases al fijar la versión. Con la
réplica, perder el volumen deja de ser grave: se crea uno vacío y la base se restaura al arrancar.

## Despliegues posteriores

```sh
fly deploy            # build en el builder remoto de Fly y reinicio de la máquina
fly deploy --local-only   # alternativa: build con el Docker local
```

### Automático desde GitHub (opcional)

Requiere que el proyecto esté en un repositorio git en GitHub (hoy no lo está).

```sh
fly tokens create deploy -x 999999h   # token limitado a esta app
# → guárdalo como secreto FLY_API_TOKEN en GitHub (Settings → Secrets → Actions)
```

`.github/workflows/deploy.yml`:

```yaml
name: deploy
on:
  push:
    branches: [main]
concurrency: deploy   # nunca dos deploys a la vez sobre la misma máquina
jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --locked          # no se despliega con tests rotos
      - uses: superfly/flyctl-actions/setup-flyctl@master
      - run: flyctl deploy --remote-only --ha=false
        env:
          FLY_API_TOKEN: ${{ secrets.FLY_API_TOKEN }}
```

## Probar la imagen en local antes de desplegar

```sh
docker build -t stardust-server .
docker run --rm -p 8080:8080 -v "$PWD/data:/data" \
  -e STARDUST_ADMIN_TOKEN=dev stardust-server
curl -fsS http://localhost:8080/healthz
```

Es la misma imagen que corre en Fly. Si funciona aquí y falla allí, el problema está en
`fly.toml`, los secretos o el volumen, no en el código.

## Operación

| Tarea | Comando |
|---|---|
| Ver logs en vivo | `fly logs` |
| Estado de la máquina y checks | `fly status`, `fly checks list` |
| Shell dentro de la máquina | `fly ssh console` |
| Gestión de usuarios | `fly ssh console -C "stardust-server user list"` |
| Copiar la base a local | `fly ssh sftp get /data/stardust.db ./stardust.db` (mejor con la app sin tráfico; si hay Litestream, `litestream restore` desde el bucket es más seguro) |
| Ver releases | `fly releases --image` |
| Volver a la imagen anterior | `fly deploy --image <imagen-de-fly-releases> --ha=false` |
| Más memoria | `fly scale memory 512` |
| Más disco | `fly volumes extend <volume-id> --size 3` (solo se puede crecer, no encoger) |

**Rollback:** volver a una imagen anterior **no** revierte la base de datos. Por eso las
migraciones solo añaden (ver el contrato). Si alguna vez hace falta una migración destructiva, se
hace en dos deploys: primero el código deja de usar lo viejo y después se borra.

## Coste aproximado

Precios de referencia; confírmalos en `fly.io/pricing` antes de decidir.

- Máquina `shared-cpu-1x` de 256 MB: unos 2 USD/mes si estuviera siempre encendida. Con apagado
  automático se paga solo el tiempo encendida.
- Volumen: unos 0,15 USD por GB al mes (1 GB en la configuración inicial).
- IPv4 compartida e IPv6: gratis. La IPv4 dedicada se paga aparte y no hace falta.
- Tigris (etapa 2): para una base pequeña, el coste es mínimo o cabe en el nivel gratuito.

## Lista de comprobación del primer despliegue

- [ ] `stardust-server` cumple el contrato (puerto, `/healthz`, `$STARDUST_DATA`, parada limpia).
- [ ] `docker build` y `docker run` funcionan en local.
- [ ] `fly launch --no-deploy`, nombre de app y región decididos.
- [ ] Volumen `stardust_data` creado en `primary_region`.
- [ ] `STARDUST_ADMIN_TOKEN` creado y guardado fuera de Fly.
- [ ] `fly deploy --ha=false` y `fly status` con una sola máquina y checks en verde.
- [ ] Primer usuario creado; `installProgram` probado con un programa de `programs/texto/`.
- [ ] Dominios propios y runner en otro dominio antes de abrir programas de terceros.
- [ ] Retención de snapshots revisada; Litestream antes de tener usuarios reales.

## Decisiones abiertas

1. **Región:** depende de dónde estén los usuarios (`fly platform regions`).
2. **Dominios:** nombres concretos de la app y del runner.
3. **Apagado automático o máquina siempre encendida:** coste frente a latencia del primer acceso.
4. **Cuándo pasar a la etapa 2** (Litestream): con los primeros usuarios externos.

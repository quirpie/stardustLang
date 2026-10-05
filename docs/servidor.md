# Servidor de StardustLang (`stardust-server`)

Estado: **etapas 1 y 2 implementadas** (`src/server/`, `src/bin/stardust-server.rs`,
`tests/server.rs`, `web/playground.template.html`, `web/runner.template.html`); etapa 3, propuesta. Convierte el playground en una plataforma para instalar, versionar,
probar y ejecutar programas StardustLang, al estilo de los artefactos de Claude: cada programa
tiene una URL, un historial de versiones y se puede instalar o actualizar **sin interfaz gráfica**,
por una IA o un script. El despliegue está en [`deploy-fly.md`](deploy-fly.md).

## Qué cambia y qué no

```text
hoy:        navegador ── playground.html ── Biblioteca (actor FILE) ── IndexedDB local
propuesta:  IA / CLI ──┐
                       ├── HTTPS ──▶ stardust-server ── SQLite (programas, versiones, usuarios)
            navegador ─┘                  │
                                          └─▶ playground y runner (la VM sigue en el navegador)
```

- **La VM no cambia.** Los programas se siguen ejecutando en el navegador de quien los abre
  (WASM). El servidor guarda, versiona y valida.
- **La validación es la de la VM**: `lang::check_source` para `.stardust` y `check::check` para la
  IR en JSON, el mismo código que usan `stardust --check` y `check_program()` en WASM. Un programa
  que el servidor acepta, la VM lo carga.
- **El playground actual sigue funcionando sin servidor** (modo local, con la Biblioteca en
  IndexedDB). Con servidor, la biblioteca pasa a ser la lista de programas del usuario.
- **Pruebas sin navegador** (etapa 3): el servidor puede ejecutar un programa con el CLI `stardust`
  en un proceso aislado y devolver su salida y su traza. Es lo que necesita una IA para probar lo
  que escribe.

## Conceptos

| Concepto | Qué es |
|---|---|
| **Usuario** | Un `handle` único (`ana`), sin contraseña. Se autentica con tokens. |
| **Token** | Credencial de API de un usuario. Un usuario puede tener varios (uno por agente o máquina), con nombre, y revocarlos por separado. |
| **Programa** | `{owner}/{name}`, p. ej. `ana/calculadora`. Pertenece a un usuario. Tiene visibilidad y un historial de versiones. |
| **Versión** | Una instantánea **inmutable**: fuente original, formato, IR compilada, resumen de capacidades, hash y nota. Se numeran 1, 2, 3… La última es la actual. |

**El historial solo crece.** Actualizar crea una versión nueva; volver atrás también (copia una
versión antigua como versión nueva). Nunca se reescribe ni se borra una versión concreta: borrar es
borrar el programa entero.

### Nombres

- `handle` y `name`: `^[a-z0-9][a-z0-9-]{0,39}$`. Minúsculas, cifras y guiones; sin puntos ni
  barras, así que sirven tal cual en rutas, URLs y nombres de base de datos del navegador.
- Al instalar sin `name`, se deriva del campo `program` del programa (minúsculas, y lo que no sea
  `[a-z0-9]` pasa a `-`).
- Si en una actualización el campo `program` ya no coincide con `name`, se avisa (warning), pero se
  acepta: la identidad del programa es su URL.

### Visibilidad

| Valor | Quién puede leerlo y ejecutarlo |
|---|---|
| `private` (por defecto) | Solo el propietario. |
| `link` | Cualquiera con la URL. No aparece en ningún listado. |

No hay programas «públicos» indexados en esta propuesta.

## Usuarios y autenticación

**Muy básica a propósito:** sin contraseñas, sin correo y sin OAuth.

1. **El administrador crea usuarios.** Lo hace con `stardust-server user add ana [--admin]` (por
   `fly ssh console`) o con `POST /api/v1/admin/users` y el `STARDUST_ADMIN_TOKEN`. La respuesta
   incluye el primer token del usuario, que se muestra **una sola vez**.
2. **Las IAs y scripts** mandan `Authorization: Bearer <token>` en cada petición.
3. **Las personas en el navegador** pegan su token una vez en el playground. Este lo cambia por una
   sesión (`POST /api/v1/session`) en una cookie `HttpOnly; Secure; SameSite=Strict`, válida 30
   días. El token no se guarda en el navegador.
4. **Cada usuario gestiona sus tokens**: crea uno por agente (`claude-portatil`, `ci`), los lista
   (con prefijo y fecha de último uso) y los revoca.

**Formato del token:** `sd_` seguido de 32 bytes aleatorios en base32 (~55 caracteres). En la base
solo se guarda su SHA-256 y el prefijo visible (`sd_ab12cd…`). El token tiene alta entropía, así que
el SHA-256 basta (Argon2 o similares son para contraseñas, que tienen poca). El prefijo `sd_` sirve
para que los escáneres de secretos lo reconozcan si se filtra a un repositorio.

**Permisos:** en la v1, cualquier token puede hacer todo sobre los programas de su usuario. Los
tokens de solo lectura o limitados a un programa quedan para después.

**Por qué no Ed25519 todavía:** firmar cada petición exige control de reloj y de repeticiones
(nonces), y librerías en cada cliente. Un bearer token sobre HTTPS es lo que cualquier agente sabe
usar sin código extra. Ed25519 encaja mejor más adelante para **firmar programas** (ver
[`bytecode.md`](bytecode.md), «Integridad y firma»): quién es el autor, no quién hace la petición.

## API HTTP

Base: `https://<STARDUST_PUBLIC_ORIGIN>/api/v1`. JSON en UTF-8 en peticiones y respuestas.

### Resumen

| Operación | Método y ruta | Auth |
|---|---|---|
| Guía para agentes | `GET /api/v1` | no |
| Validar sin guardar | `POST /check` | sí |
| **installProgram** | `POST /programs` | sí |
| **updateProgram** | `POST /programs/{owner}/{name}/versions` | sí, propietario |
| Volver a una versión | `POST /programs/{owner}/{name}/rollback` | sí, propietario |
| Leer programa (versión actual) | `GET /programs/{owner}/{name}` | según visibilidad |
| Listar versiones | `GET /programs/{owner}/{name}/versions` | propietario |
| Leer una versión | `GET /programs/{owner}/{name}/versions/{n}` | según visibilidad |
| Cambiar visibilidad | `PATCH /programs/{owner}/{name}` | propietario |
| Borrar programa | `DELETE /programs/{owner}/{name}` | propietario |
| Mis programas | `GET /programs` | sí |
| Ejecutar sin navegador (etapa 3) | `POST /run` | sí |
| Quién soy | `GET /me` | sí |
| Tokens | `GET/POST /tokens`, `DELETE /tokens/{id}` | sí |
| Sesión web | `POST /session` (`{"token"}` → cookie), `DELETE /session` | token / cookie |
| Usuarios (admin) | `GET/POST /admin/users`, `POST /admin/users/{handle}/disable` | admin |

Las rutas de la tabla son relativas a `/api/v1`.

### Fuente del programa

Las operaciones que reciben un programa usan el mismo objeto:

```json
{
  "source": "app calculadora\n…",
  "format": "stardust"
}
```

- `format`: `"stardust"` (texto) o `"json"` (IR). Si se omite: si `source` empieza por `{` tras
  quitar espacios, es `json`; si no, `stardust`. Es la misma regla que el CLI (que decide por la
  extensión).
- Tamaño máximo de `source`: **256 KiB**. Si se pasa, `413`.

### Informe de validación

Todas las operaciones que validan devuelven el mismo informe, con la forma que ya produce la VM:

```json
{
  "ok": false,
  "errors": [
    { "line": 12, "col": 5, "message": "variable 'total' sin definir", "hint": "…" }
  ],
  "warnings": [],
  "capabilities": { "actors": { "Calculadora": ["RENDER"] }, "net_allow": [] }
}
```

- Para `.stardust`, los errores llevan `line` y `col` (de `lang::Diagnostic`). Para JSON, llevan
  `path` (de `check::Issue`, p. ej. `actors[0].on_message.body[2]`). Un cliente debe aceptar ambas
  formas.
- `capabilities` resume lo que pide el programa: las capacidades de cada actor y la allowlist de
  red. Es lo que el runner enseña a quien lo abre (ver «Runner»).

### Respuesta de un cambio

installProgram, updateProgram y rollback responden con la misma forma:

```json
{
  "program": { "owner": "ana", "name": "contador", "current_version": 4, "version": { "n": 4, … } },
  "report": { "ok": true, "errors": [], "warnings": [], "capabilities": { … } },
  "unchanged": false,
  "capabilities_changed": false
}
```

- `program` va **sin `source` ni `ir`**: quien llama acaba de enviarlos. Para leerlos, `GET`.
- `report` no aparece en rollback (no se valida de nuevo: la IR guardada no cambia).
- Un `201` lleva la cabecera `Location` con la ruta del programa en la API.
- Los avisos sin posición (p. ej. «el programa se declara 'x' pero se guarda como 'y'») solo
  llevan `message`.

### `POST /check`: validar sin guardar

Cuerpo: un objeto fuente. Respuesta: `200` con el informe, sea válido o no; el campo `ok` dice si
lo es. Si la fuente es `.stardust` y compila, incluye además `ir`.

Es el bucle de una IA: generar, `check`, corregir con los errores por línea y repetir. No guarda
nada y no cuenta para los límites de almacenamiento.

### `POST /programs`: installProgram

```json
{ "name": "calculadora", "source": "…", "format": "stardust", "visibility": "private", "note": "primera versión" }
```

`name`, `format`, `visibility` y `note` son opcionales.

| Caso | Respuesta |
|---|---|
| Nuevo y válido | `201` + programa (versión 1) + informe. Cabecera `Location`. |
| No valida | `422` + informe. No se guarda nada. |
| Ya existe con la **misma fuente** en la versión actual | `200` + programa, `"unchanged": true`. |
| Ya existe con otra fuente | `409` `program_exists` + `current_version`. Para cambiarlo, usa updateProgram. |

El caso `unchanged` hace la instalación **idempotente**: si una IA reintenta tras un timeout, no
recibe un error confuso.

### `POST /programs/{owner}/{name}/versions`: updateProgram

```json
{ "base_version": 3, "source": "…", "format": "stardust", "note": "botón de borrar" }
```

`base_version` es **obligatorio**: es la versión sobre la que se hizo el cambio. Así, una IA no
pisa sin darse cuenta lo que acaba de guardar una persona (ni al revés).

| Caso | Respuesta |
|---|---|
| `base_version` = actual y válido | `201` + nueva versión + informe. |
| Fuente idéntica a la actual | `200`, `"unchanged": true`. No crea versión. Se comprueba **antes** que la base: un reintento tras un timeout no da conflicto. |
| No valida | `422` + informe. |
| `base_version` ≠ actual | `409` `version_conflict` + `current_version`. |
| `"force": true` en vez de `base_version` | Se acepta sobre la versión actual, sea cual sea. Solo para scripts que saben lo que hacen. |

La respuesta incluye `capabilities_changed`: `true` si la nueva versión pide capacidades o
prefijos de red que la anterior no pedía. El runner lo usa para volver a pedir permiso.

### `POST /programs/{owner}/{name}/rollback`

```json
{ "to_version": 2, "base_version": 5 }
```

Crea la versión 6 con el contenido de la 2. Tiene las mismas reglas de conflicto que updateProgram.

### Programa y versión: forma de respuesta

```json
{
  "owner": "ana",
  "name": "calculadora",
  "visibility": "private",
  "url": "https://stardust.example.com/p/ana/calculadora",
  "current_version": 4,
  "created_at": "2026-10-03T10:00:00Z",
  "updated_at": "2026-10-03T12:30:00Z",
  "version": {
    "n": 4,
    "format": "stardust",
    "source": "…",
    "ir": { "program": "calculadora", "entry": "…", "actors": [ … ] },
    "sha256": "…",
    "capabilities": { … },
    "note": "botón de borrar",
    "created_at": "2026-10-03T12:30:00Z",
    "created_by_token": "claude-portatil"
  }
}
```

- `GET /programs/{owner}/{name}` devuelve la versión actual. Con `?fields=meta` no incluye
  `source` ni `ir` (para listados ligeros).
- `GET …/versions` lista versiones sin `source` ni `ir`.
- `created_by_token` es el nombre del token que la creó. Sirve para distinguir si un cambio lo hizo
  una persona o qué agente.

### Errores

Todos los errores que no son de validación tienen esta forma:

```json
{ "error": { "code": "version_conflict", "message": "la versión actual es 5, no 3", "current_version": 5 } }
```

| HTTP | `code` |
|---|---|
| 400 | `bad_request` (JSON mal formado, falta un campo o tiene otro tipo, nombre no válido) |
| 401 | `unauthorized` (sin token, token inválido o revocado) |
| 403 | `forbidden` (no eres el propietario; usuario deshabilitado), `quota_exceeded` (máximo de programas) |
| 404 | `not_found` (también para programas `private` ajenos: no se revela que existen) |
| 409 | `program_exists`, `version_conflict` |
| 413 | `too_large` |
| 415 | `bad_request` (falta `Content-Type: application/json`) |
| 422 | `invalid_program` (con el informe) |
| 429 | `rate_limited` (con `retry_after`, en segundos, dentro de `error`) |

### Límites

| Límite | Valor inicial |
|---|---|
| Tamaño de fuente | 256 KiB |
| Programas por usuario | 100 |
| Versiones por programa | sin límite (cada una cabe en pocos KB) |
| Peticiones por token | 120 por minuto (`/check`: 300 por minuto) |
| `/run` (etapa 3) | 1 ejecución a la vez por usuario, 30 por minuto |

### `GET /api/v1`: guía para agentes

Devuelve texto plano breve, pensado para una IA que llega sin contexto: qué es StardustLang, el
flujo `check` → `install` → `update` con `base_version`, ejemplos con `curl` y un enlace a la
especificación del lenguaje ([`sintaxis-texto.md`](sintaxis-texto.md)). También se publica en
`/llms.txt`.

## Ejemplo: una IA instala y actualiza un programa

```sh
export SD=https://stardust.example.com/api/v1
export TOKEN=sd_…

# 1. Validar.
curl -s $SD/check -H "Authorization: Bearer $TOKEN" \
  --json "$(jq -n --rawfile s app.stardust '{source:$s}')"

# 2. Instalar (versión 1).
curl -s $SD/programs -H "Authorization: Bearer $TOKEN" \
  --json "$(jq -n --rawfile s app.stardust '{name:"contador", source:$s}')"

# 3. Más tarde: leer la versión actual, cambiar y actualizar sobre ella.
V=$(curl -s "$SD/programs/ana/contador?fields=meta" -H "Authorization: Bearer $TOKEN" | jq .current_version)
curl -s $SD/programs/ana/contador/versions -H "Authorization: Bearer $TOKEN" \
  --json "$(jq -n --rawfile s app.stardust --argjson v $V '{base_version:$v, source:$s}')"
```

### En el CLI `stardust`

Para no escribir `curl`, el CLI gana subcomandos que usan `STARDUST_SERVER` y `STARDUST_TOKEN`:

```sh
stardust remote check app.stardust
stardust remote install app.stardust [--name contador] [--link]
stardust remote update ana/contador app.stardust --base 4   # o --force
stardust remote get ana/contador [--version 2] > app.stardust
stardust remote versions ana/contador
stardust remote run app.stardust --stdin entrada.txt          # etapa 3
```

`install` y `update` imprimen el informe con el mismo formato que `stardust --check`
(`archivo:línea: mensaje`), así que un agente que ya usa `--check` no tiene que aprender nada nuevo.

## Playground y runner

### URLs

| URL | Origen | Qué es |
|---|---|---|
| `/` | app | Playground: lista de mis programas, editor y ventanas. |
| `/p/{owner}/{name}` | app | Página de un programa: lo ejecuta a pantalla completa (como un artefacto). |
| `/p/{owner}/{name}/v/{n}` | app | Una versión concreta. |
| `/run/` | runner | Página genérica del runner (VM + `runtime.js`). Solo se carga dentro de un iframe. |

### Cómo se ejecuta un programa

```text
 app (stardust.example.com)                      runner (run.example-apps.com)
 ─────────────────────────                        ─────────────────────────────
 GET /api/v1/programs/ana/x  (con la sesión)
 <iframe src="https://run…/run/"           ───▶  carga VM + runtime.js
         sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-downloads">
          ◀──── {type:"ready"} ──────────────────
 {type:"load", key:"ana/x", title, ir,     ───▶  comprueba event.origin == publicOrigin
  capabilities, trusted, theme}                   y event.source == window.parent
                                                  pide permiso si hace falta (ver «Permisos»)
                                                  createStardustRuntime({source: ir, …})
          ◀──── {type:"trace", line} ────────     datos en IndexedDB "stardust-run:ana/x"
          ◀──── {type:"started"} | {type:"error", message}
 {type:"theme", theme}                     ───▶  sigue el tema claro/oscuro de la app
 {type:"drop", key}                        ───▶  borra los datos (al borrar la app)
          ◀──── {type:"dropped", key}
```

- **El runner nunca se autentica.** Es una página estática y recibe la IR del programa por
  `postMessage` desde la app, que sí tiene la sesión. Así los programas `private` funcionan sin
  tickets ni cookies entre dominios, y el runner no puede leer nada del usuario aunque falle.
- **Los dos lados comprueban origen y ventana.** El runner solo acepta mensajes de su ventana padre
  con origen `STARDUST_PUBLIC_ORIGIN`, y solo se deja enmarcar por ese origen
  (`Content-Security-Policy: frame-ancestors <PUBLIC_ORIGIN>`). La app solo acepta mensajes del
  origen del runner que vengan de un iframe que ella creó.
- **`sandbox` sin `allow-top-navigation`:** un programa no puede llevarse la pestaña a otra página.
  `allow-same-origin` es necesario para IndexedDB; con el runner en otro origen no da acceso a la
  app.
- **El servidor separa los orígenes por `Host`:** en el del runner solo existen `/run/` y
  `/healthz`; en el de la app, `/run/` no existe. La página de la app no se deja enmarcar
  (`frame-ancestors 'none'`).
- **Datos de cada programa:** solo en el navegador de quien lo abre, en la base IndexedDB
  `stardust-run:{owner}/{name}` del origen del runner. **Nunca llegan al servidor.** Se conservan
  entre versiones (abrir una versión antigua usa los mismos datos); si una versión cambia el
  formato de sus ficheros, adaptarlo es cosa del programa. Borrar la app borra también estos datos
  (mensaje `drop`).

### Modelo de amenazas, en dos capas

1. **La VM.** Un programa StardustLang no ejecuta JavaScript: solo hace lo que permiten sus
   capacidades. `FILE` queda confinado a su VFS, `NET` a los prefijos de `net_allow`, y no tiene
   acceso al DOM salvo a través de la vista declarativa. Esta es la barrera principal.
2. **El origen separado.** Si hubiera un fallo en la VM o en `runtime.js` (p. ej. un widget que
   inserta HTML sin escapar), el código inyectado correría en el origen del runner, donde no hay
   sesión ni datos de la app. Por eso el runner va en otro dominio registrable
   ([`deploy-fly.md`](deploy-fly.md), «Dominios»).

Entre programas del mismo origen de runner no hay aislamiento de navegador: dos programas abiertos
en el mismo navegador comparten origen. Eso solo importa si falla la capa 1, y se acepta en la v1.
Endurecerlo después significa un subdominio por propietario (`ana.run.example-apps.com`), con
certificado comodín.

### Permisos

Antes de ejecutar un programa ajeno, el runner muestra lo que pide: `NET` con sus prefijos de
red, `FILE` y `CRYPTO`. `RENDER` e `IO_STREAM` no se preguntan. El permiso se guarda por
programa en el `localStorage` del runner (`stardust-consent:{owner}/{name}`) y se vuelve a pedir
si una versión pide una capacidad o un prefijo de red que no estaban concedidos. Un programa
propio (`trusted`: el usuario con sesión es el propietario) se ejecuta sin preguntar.

### El playground

Una sola página (`web/playground.template.html`) con dos modos:

- **Modo servidor**, si la sirve `stardust-server` (que inyecta su configuración en el marcador
  `STARDUST_CONFIG`):
  - Sin sesión, pide el token y lo cambia por la cookie (`POST /session`). El token no se guarda
    en el navegador.
  - La biblioteca es `GET /programs`, con la versión de cada programa y un 🔗 si tiene enlace.
  - Guardar instala o actualiza sobre la versión con la que se abrió el editor. Ante un `409`
    pregunta si sobrescribir (`force`); si no, deja seguir editando.
  - «ⓘ» abre las versiones (nota, fecha y token que la creó) con «Abrir» y «Volver a esta», la
    visibilidad y el enlace para copiar.
  - Cada ventana es un iframe del runner.
  - `/p/{owner}/{name}[/v/{n}]` muestra un solo programa a pantalla completa. Si es privado y no
    hay sesión, pide el token ahí mismo.
- **Modo local**, si se abre `playground.html` suelto: la Biblioteca en IndexedDB, como antes, y
  las apps corren en la propia página.
- **En los dos modos**, el editor acepta `.stardust` y JSON y valida mientras se escribe con la VM
  en WASM (`compile_program` / `check_program`), sin red y con los errores por línea. El servidor
  vuelve a validar al guardar. «Nueva app» parte de un contador en `.stardust`.

`web/stardust.css` (tema y widgets) se comparte entre el playground y el runner.
`web/build-playground.sh` genera `playground.html` y `runner.html`.

## Pruebas sin navegador: `POST /run` (etapa 3)

Una IA necesita ver si su programa **hace** lo que debe, no solo si valida. El CLI `stardust` ya
ejecuta programas sin navegador, así que el servidor lo reutiliza.

```json
{
  "source": "…",
  "format": "stardust",
  "stdin": "10\n",
  "timeout_ms": 3000,
  "trace": true
}
```

En vez de `source`, se puede pasar `"program": "ana/contador"` y, opcionalmente, `"version": 3`.

```json
{
  "exit": "ok",
  "faults": 0,
  "stdout": "55\n",
  "trace": ["[boot] Fibonacci …", "…"],
  "elapsed_ms": 41
}
```

`exit` puede ser `ok`, `faults` (algún actor falló; la VM aísla el fallo y sigue), `timeout` o
`killed` (se pasó de memoria).

**Aislamiento:** cada ejecución es un **proceso hijo** del binario `stardust`, nunca un hilo del
servidor. Un hilo de Rust no se puede matar, y un bucle infinito o una recursión desbocada colgaría
el servidor.

- Límite de tiempo real: `timeout_ms`, como máximo 5000. Al vencer, se mata el proceso.
- Límite de memoria: `RLIMIT_AS` de 128 MiB, fijado antes del `exec`.
- `--data` apunta a un directorio temporal nuevo, que se borra al acabar. `FILE` funciona, pero
  solo con ficheros de esa ejecución.
- **Sin red:** nueva opción `--no-net` del CLI, que ignora `net_allow` y deniega toda petición.
  Sin ella, `/run` permitiría usar el servidor como proxy.
- Salida recortada a 64 KiB por canal.
- Concurrencia: como mucho 2 ejecuciones simultáneas en todo el servidor (semáforo); las demás
  esperan hasta 2 s y si no, `429`.
- **Programas con vista (`RENDER`):** el CLI entra en su bucle interactivo leyendo etiquetas de
  botón por stdin. `/run` lo aprovecha: `stdin` es la secuencia de pulsaciones (como en
  `printf "7\n*\n8\n=\nq\n" | stardust calculadora.json`), y la salida muestra la vista en texto
  tras cada paso.

Esto exige que la imagen incluya también el binario `stardust` y que la máquina tenga **512 MB**
(ver [`deploy-fly.md`](deploy-fly.md)).

## Esquema de la base (SQLite)

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE users (
  id          INTEGER PRIMARY KEY,
  handle      TEXT NOT NULL UNIQUE,
  is_admin    INTEGER NOT NULL DEFAULT 0,
  disabled_at TEXT,
  created_at  TEXT NOT NULL
);

CREATE TABLE tokens (
  id           INTEGER PRIMARY KEY,
  user_id      INTEGER NOT NULL REFERENCES users(id),
  name         TEXT NOT NULL,
  prefix       TEXT NOT NULL,               -- "sd_ab12cd", para mostrar
  sha256       BLOB NOT NULL UNIQUE,
  created_at   TEXT NOT NULL,
  last_used_at TEXT,
  revoked_at   TEXT
);

CREATE TABLE sessions (
  sha256     BLOB PRIMARY KEY,              -- hash del id de la cookie
  user_id    INTEGER NOT NULL REFERENCES users(id),
  expires_at TEXT NOT NULL,
  token_id   INTEGER REFERENCES tokens(id)  -- migración 2: revocar el token cierra la sesión
);

CREATE TABLE programs (
  id              INTEGER PRIMARY KEY,
  owner_id        INTEGER NOT NULL REFERENCES users(id),
  name            TEXT NOT NULL,
  visibility      TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private','link')),
  current_version INTEGER NOT NULL,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL,
  UNIQUE (owner_id, name)
);

CREATE TABLE versions (
  program_id   INTEGER NOT NULL REFERENCES programs(id) ON DELETE CASCADE,
  n            INTEGER NOT NULL,
  format       TEXT NOT NULL CHECK (format IN ('stardust','json')),
  source       TEXT NOT NULL,
  ir           TEXT NOT NULL,               -- IR compilada (JSON); es lo que recibe el runner
  sha256       BLOB NOT NULL,               -- de `source`, para detectar "unchanged"
  capabilities TEXT NOT NULL,               -- JSON del resumen
  note         TEXT,
  token_id     INTEGER REFERENCES tokens(id),
  created_at   TEXT NOT NULL,
  PRIMARY KEY (program_id, n)
);

CREATE TABLE schema_version (v INTEGER NOT NULL);
```

- **Fechas** en texto ISO 8601 UTC.
- **Instalar o actualizar es una transacción** (`BEGIN IMMEDIATE`): comprueba `current_version`,
  inserta la versión y actualiza el programa. Con un solo escritor SQLite no hay carreras, y el
  `409` sale de la comprobación dentro de la transacción.
- **Se guarda la IR** además de la fuente: el runner y `/run` no recompilan, y un cambio futuro del
  compilador no altera lo que ejecutan versiones ya publicadas.
- **Migraciones** numeradas en `schema_version`, aplicadas al arrancar, y **solo aditivas** (ver el
  contrato en [`deploy-fly.md`](deploy-fly.md)).

## Estructura en el código

```text
Cargo.toml
  [features] server = ["dep:axum", "dep:tokio", "dep:rusqlite", "dep:getrandom"]
  [[bin]] name = "stardust-server", path = "src/bin/stardust-server.rs", required-features = ["server"]
src/server/
  mod.rs        configuración (variables de entorno), errores, router, arranque y parada limpia
  db.rs         conexión, migraciones
  auth.rs       tokens, usuarios, sesión web (cookie + CSRF por Origin), extractores, límite de peticiones
  programs.rs   check / install / update / rollback / get / list (handlers)
  validate.rs   fuente → (IR, informe, capacidades), usando lang::check_source y check::check
  web.rs        /healthz, páginas (/, /p/…, /run/) con su configuración, enrutado por Host, guía
web/
  playground.template.html, runner.template.html, stardust.css, runtime.js, build-playground.sh
  run.rs        /run: proceso hijo con límites (etapa 3)
src/bin/stardust-server.rs   main + subcomandos `user add/list/token/disable`
tests/server.rs              API de extremo a extremo con una base en un directorio temporal
```

SHA-256 y la comparación en tiempo constante vienen de `crypto.rs` (ya en el crate), y las
fechas ISO de `date.rs`.

### CLI de administración

```sh
stardust-server                              # sirve la API
stardust-server user add ana [--admin]       # crea el usuario e imprime su primer token
stardust-server user list
stardust-server user token ana [--name ci]   # token nuevo (si perdió el suyo)
stardust-server user disable ana             # deshabilita y revoca sus tokens
```

Actúan directamente sobre `$STARDUST_DATA/stardust.db`.

### Probar en local

```sh
cargo test --features server                       # unitarios + extremo a extremo
cargo run --features server --bin stardust-server -- user add ana
STARDUST_ADMIN_TOKEN=dev cargo run --features server --bin stardust-server
curl -s localhost:8080/api/v1                      # guía
```

- **`rusqlite` con `bundled`**: SQLite se compila dentro y la imagen no necesita la librería del
  sistema. Una sola conexión tras un `Mutex`, usada desde `spawn_blocking`; con un solo escritor
  no hace falta un pool.
- **`validate.rs` es la única puerta**: todo lo que entra en `versions` pasa por ella, y su test
  recorre `programs/*.json` y `programs/texto/*.stardust` como ya hace `check.rs`.
- El CLI `stardust` y el build WASM no cambian de dependencias: todo lo nuevo va tras el feature
  `server`. Los subcomandos `stardust remote …` usan `ureq`, que el CLI ya tiene.

## Plan por etapas

1. **Servidor mínimo** (hecho). Usuarios y tokens (CLI de admin), `/check`, install, update,
   rollback, get y list, `/healthz` y servir `playground.html`. Tests de API. Ficheros de
   despliegue (`Dockerfile`, `fly.toml`). *Una IA ya puede instalar y actualizar con `curl`.*
2. **Playground conectado** (hecho). Sesión con token, biblioteca del servidor, editor
   `.stardust` con validación en vivo, historial, `/p/{owner}/{name}`, runner en iframe con
   `postMessage` y permisos. *El «artefacto» de StardustLang, con su URL.* Falta configurar los
   dominios propios en Fly ([`deploy-fly.md`](deploy-fly.md), «Dominios»): mientras tanto
   funciona en modo desarrollo.
3. **Pruebas sin navegador.** `--no-net` en el CLI, `POST /run` con proceso aislado y
   `stardust remote …`. *Al terminar: una IA escribe, prueba e instala sin interfaz gráfica.*
4. **Después.** Servidor MCP (las mismas operaciones como herramientas), Litestream, tokens con
   permisos limitados, programas firmados con Ed25519 y llamadas entre programas instalados
   (`grants`/`exports`).

## Decisiones abiertas

1. **Llamadas entre programas instalados.** Hoy varias apps se cargan juntas en una VM y se
   llaman con `app/actor` y tokens de `grants`. Con el servidor, ¿un programa puede depender de
   otro instalado (`ana/geometria`)? Queda fuera de la v1: cada programa se ejecuta solo.
2. **Datos compartidos entre visitantes.** En la v1 los datos de un programa son locales al
   navegador de cada visitante. Un almacén compartido en el servidor (como la base de datos de los
   artefactos) es otro proyecto.
3. **Registro abierto.** En la v1 solo el admin crea usuarios. ¿Autoregistro con invitación?
4. **¿Quitar el modo local del playground** cuando el servidor esté estable, o mantenerlo para
   uso sin conexión?

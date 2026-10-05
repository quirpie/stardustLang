# Proyecto Stardust — StardustVM (Nano POC)

Núcleo de la **StardustVM**: una máquina virtual por actores que ejecuta **StardustLang**,
un bytecode estructurado (JSON) generable por IA. Este repositorio implementa el
**Nano POC** de la sección 6 del documento de arquitectura: calcular Fibonacci con
dos actores aislados que se comunican por mensajes asíncronos.

## Qué valida este POC

- ✅ **Aislamiento de fallos** — si el motor matemático falla (p. ej. división por
  cero o pánico), la VM lo captura, lo reinicia y el resto del sistema (la UI) sigue vivo.
- ✅ **Enrutamiento de mensajes asíncrono** — `SEND` no invoca; encola. El
  planificador drena la cola y registra cada salto (observabilidad).
- ✅ **StardustLang puro** — los actores solo contienen las 7 instrucciones base.
- ✅ **Modelo de capacidades** — solo un actor con la capacidad `IO_STREAM` puede
  hacer E/S; el motor matemático es "ciego al exterior".
- ✅ **Red asíncrona sin bloquear** — `NET_FETCH` no espera la respuesta: encola
  una petición y el host la resuelve entre ciclos, reinyectando el resultado como
  un `on_message`. El intérprete sigue síncrono; la red es un salto más del buzón.

## Sintaxis de texto (`.stardust`), el formato principal

Los programas se escriben en una sintaxis parecida a Python que se compila a la IR (el JSON
que ejecuta la VM). Es un 71 % más corta en tokens y es la que usan las personas y los LLMs:

```python
app doble

actor UI:
  start:
    r = 'Escribe un número'
  on m:
    r = number(m) * 2
  view:
    column:
      label(r)
      input('Doble')
```

```bash
./target/release/stardust programs/texto/fibonacci.stardust          # ejecuta
./target/release/stardust programs/texto/doble.stardust --check      # valida (errores por línea)
./target/release/stardust programs/texto/doble.stardust --emit-json  # muestra la IR generada
```

Especificación: [`docs/sintaxis-texto.md`](docs/sintaxis-texto.md). Ejemplos:
[`programs/texto/`](programs/texto/) — cada uno produce la misma salida que su `.json` original
([`tests/texto.rs`](tests/texto.rs)). El resto de este README describe la IR, que sigue siendo el
contrato de la VM.

## Ejecutar

```bash
cargo build --release

# Fibonacci (con traza de observabilidad de la VM):
echo "10" | ./target/release/stardust programs/fibonacci.json

# Solo la E/S del programa, sin traza:
echo "15" | ./target/release/stardust programs/fibonacci.json --quiet

# Demostración de aislamiento de fallos (motor con bug):
echo "10" | ./target/release/stardust programs/fibonacci_fault.json
```

Salida esperada (Fibonacci, limit=10):

```
[VM] route #001 Actor_IO -> Actor_Calculo :: 10
[VM] route #002 Actor_Calculo -> Actor_IO :: 0, 1, 1, 2, 3, 5, 8, 13, 21, 34
Serie de Fibonacci: 0, 1, 1, 2, 3, 5, 8, 13, 21, 34
```

## StardustLang: el conjunto de instrucciones

Un programa es un JSON con `program`, `entry` (actor que arranca) y `actors`.
Cada actor tiene `capabilities`, `on_start` (hook de inicialización que corre en
**todos** los actores al bootear; el `entry` primero) y `on_message`
(`bind` = variable donde se enlaza el payload + `body`).

Las **7 instrucciones base** (cómputo, Turing completo):

| # | `op`        | Forma |
|---|-------------|-------|
| 1 | `DEF_VAR`   | `{"op":"DEF_VAR","name":"a","value":0}` |
| 2 | `ASSIGN`    | `{"op":"ASSIGN","target":"a","value":{"var":"b"}}` |
| 3 | `IF_COND`   | `{"op":"IF_COND","cond":{"var":"c"},"then":[...],"else":[...]}` |
| 4 | `LOOP`      | `{"op":"LOOP","cond":{"var":"c"},"body":[...]}` (while) |
| 5 | `COMPARE`   | `{"op":"COMPARE","target":"c","operator":"<","left":{"var":"i"},"right":{"var":"n"}}` |
| 6 | `MATH`      | `{"op":"MATH","target":"c","operator":"+","left":{"var":"a"},"right":{"var":"b"}}` |
| 7 | `IO_STREAM` | `{"op":"IO_STREAM","mode":"in"/"out","target":"x","value":<expr>,"prompt":"..."}` |

**Expresiones** (`<expr>`), tres formas:
- literal corto: número entero/decimal (`0`, `3.14`), cadena (`"texto"`), booleano (`true`)
- referencia a variable: `{"var":"nombre"}`
- literal tipado: `{"lit":"2026-12-31","as":"date"}` (para tipos sin sintaxis corta)

- `COMPARE.operator`: `==`, `!=`, `<`, `>`, `<=`, `>=` → escribe un booleano en `target`.
- `MATH.operator`: `+`, `-`, `*`, `/`, `%`. El `+` con algún operando de tipo
  cadena concatena (permite construir la serie como texto).
- `IF_COND` / `LOOP` leen `cond` como booleano (normalmente producido por `COMPARE`).
- `IO_STREAM` requiere la capacidad `"IO_STREAM"`; si no, la VM aísla el actor.

### Sistema de tipos

Tipado dinámico. Tipos actuales: `Int` (i64), `Float` (f64), `Str`, `Bool`,
`Date` (`YYYY-MM-DD`, calendario gregoriano), `Record` (campos nombrados),
`List` (colección ordenada), `Bytes` (binario, transportado en base64) y `Null`.
Reglas centralizadas en [`src/value.rs`](src/value.rs):

- **Promoción numérica**: si un operando de `MATH` es `Float`, el resultado es
  `Float`; si ambos son `Int`, es `Int` (división entera). `COMPARE` promueve igual.
- **Aritmética de fechas**: `Date + Int` / `Int + Date` → `Date` (suma días);
  `Date - Int` → `Date`; `Date - Date` → `Int` (días de diferencia).
- **Concatenación**: `+` con cualquier `Str` produce texto (`"total: " + 40` → `"total: 40"`).
- **Entrada** (`IO_STREAM in`): se clasifica al tipo más específico —
  `int` > `float` > `date` > `str`.
- **Records** (payloads tipo API): se construyen con `{"record":{"base":{"var":"b"}}}`
  y se leen con `{"field":"base","from":{"var":"p"}}`. Un `on_message` puede declarar
  un contrato `expects` (campo→tipo) que la VM valida al recibir; un incumplimiento
  es un fallo aislado. Demo: [`programs/area_record.json`](programs/area_record.json).
- **Listas**: construir `{"list":[…]}`, indexar `{"at":<i>,"of":<lista>}` (índice
  negativo desde el final), longitud `{"len":<expr>}`; instrucciones `APPEND` (añade)
  y `FOREACH` (itera, con `var` e `index` opcional). `[1,2] + [3]` concatena. Demo:
  [`programs/arrays.json`](programs/arrays.json) — Fibonacci como lista real, no como texto.
- **Bytes** (binario): `{"to_bytes":<txt>}`, `{"from_bytes":<b>}`, `{"base64":<b>}`,
  literal `{"bytes":"<base64>"}`. `HASH`/`SIGN` operan sobre los bytes crudos.
- **Texto** (ops puras, char-based, índices negativos desde el final como `at`):
  `{"split":<t>,"on":<sep>}` y `{"join":<lista>,"with":<sep>}`; `{"slice":<t>,"from":<i>,"to":<j>}`
  (subcadena); `{"replace":<t>,"find":<a>,"with":<b>}` (todas); predicados `{"contains":…,"sub":…}`,
  `{"starts_with":…,"prefix":…}`, `{"ends_with":…,"suffix":…}`, `{"index_of":…,"sub":…}` (-1 si no está);
  `{"upper":…}`/`{"lower":…}`/`{"trim":…}`/`{"repeat":<t>,"times":<n>}`/`{"to_str":<expr>}` y
  `{"parse":<t>}` (texto → valor auto-tipado como la entrada de `IO_STREAM in`: lo que se teclea
  en un `input` llega como texto y hay que convertirlo antes de operar con `MATH`). Con
  `foreach`/`procedures` bastan para un conversor Markdown→HTML entero (ver la demo del editor).
- **Serialización etiquetada** (sin pérdidas): `SERIALIZE`/`DESERIALIZE` convierten
  un valor a/desde JSON con etiquetas de tipo (`{"$type":"date",…}`), de modo que
  `Date` y `Bytes` round-trippean **conservando su tipo** (JSON plano perdería una
  `Date` como texto). Es el códec de `Engine.snapshot()`/`restore()` en la persistencia
  web. Demo: [`programs/bytes.json`](programs/bytes.json).

**Añadir un tipo nuevo** (p. ej. `Duration`, `Money`) es un cambio localizado en
`value.rs`: variante en `Value` → cubrir en `Display`/`is_truthy`/`parse_input` →
casos en `arith`/`order`/`values_equal` → registrar en `construct`. `Date` es el
ejemplo de referencia de un tipo no numérico.

### Ficheros y persistencia (capacidad `FILE`)

Un actor con la capacidad `"FILE"` puede persistir datos, **confinado a un sandbox**
(raíz `--data <dir>`, por defecto `stardust-data/`): las rutas son relativas y se
rechazan las absolutas o con `..` (mínimo privilegio, sin acceso ambiental).
La escritura es **byte-nativa**: un `Bytes` se guarda crudo (imágenes, PDFs, blobs)
y el resto como texto UTF-8. Instrucciones: `FILE_WRITE`, `FILE_APPEND`, `FILE_READ`
(auto-tipa como texto, o binario con `"as":"bytes"`), `FILE_EXISTS`, `FILE_LIST`
(lista rutas, como `ls`) y `FILE_DELETE` (como `rm`).

El backend es único pero se adapta al host (una sola capacidad, dos respaldos): el
binario nativo usa el **disco real**; el navegador usa un **VFS en memoria** que el
host espeja a **IndexedDB** (el intérprete es síncrono; la asincronía de IndexedDB
vive solo en la frontera JS).

```bash
# Contador que persiste entre ejecuciones (el "Actor de Estado" del piloto §7):
./target/release/stardust programs/contador.json --data stardust-data   # imprime 1, luego 2, 3…

# Sistema de archivos completo: texto + binario, ls, read-as-bytes, hash, rm:
./target/release/stardust programs/archivos.json --data /tmp/stardust-fs
```

Demos: [`programs/contador.json`](programs/contador.json) — `Actor_Store` (con `FILE`)
lee/incrementa/escribe un contador; `Actor_IO` solo muestra (la lógica de persistencia
no toca la UI). [`programs/archivos.json`](programs/archivos.json) — ciclo completo
de un sistema de archivos con ficheros de texto y binarios.

### Criptografía (capacidad `CRYPTO`)

`HASH` (SHA-256, función pura sin capacidad) y `SIGN`/`VERIFY` (HMAC-SHA256 con la
clave del host, gated por `"CRYPTO"`). SHA-256/HMAC están implementados sin
dependencias ([`src/crypto.rs`](src/crypto.rs), validados contra vectores RFC), así
que funcionan igual en nativo y WASM. El programa **nunca ve la clave** (`--key <s>`):
la capacidad concede "poder firmar", no la clave. Demo:
[`programs/crypto.json`](programs/crypto.json) — firma un token y demuestra que
manipular el mensaje invalida la firma (tokens infalsificables). Esto es lo que
endurecería los tokens del broker cross-app (hoy texto plano).

### Red (capacidad `NET`)

Un actor con la capacidad `"NET"` puede llamar a APIs HTTP con `NET_FETCH`,
**confinado a una allowlist** (`net_allow` a nivel de programa): una URL que no
empiece por ningún prefijo permitido se rechaza (mínimo privilegio, como el
sandbox de `FILE`; en multi-app cada app tiene su propia allowlist). Reglas de
red por invocación: `--net-timeout <segs>` (por defecto 30) y `--net-max-body
<bytes>` (por defecto 16 MiB).

La red **no bloquea el intérprete** (que es síncrono, y en el navegador no puede
esperar a `fetch`): `NET_FETCH` **encola** una petición saliente y no devuelve
nada inline; el host la resuelve **entre ciclos de mensajes** (bloqueante en
nativo, `fetch` en la frontera JS) y **reinyecta la respuesta como un
`on_message`** al actor que la pidió. Es el mismo patrón asíncrono petición/
respuesta que una llamada a otra app (`reply_to`), y el gemelo del espejo de
`FILE` a IndexedDB, con una pata extra: la respuesta, correlacionada por `corr`.

La respuesta llega como record `{"kind":"net_response","corr":Int,"tag":<eco>,
"url":Str,"ok":Bool,"status":Int,"error":Str|null,"headers":Record,"body":Bytes}`.
El cuerpo es **byte-nativo**: JSON de una API → `DESERIALIZE`, binario → `Bytes`
crudo. Un fallo de transporte (DNS/TLS/timeout) no colapsa la VM: viene con
`ok:false` y `error`, un mensaje más que el actor maneja (aislamiento de fallos).

```bash
# Pide un "zen" a la API de GitHub y lo imprime (host nativo, cliente ureq):
./target/release/stardust programs/red.json --data /tmp/stardust-net
```

Demo: [`programs/red.json`](programs/red.json) — un actor con `NET`+`IO_STREAM`
llama a `https://api.github.com/zen` y muestra el `status` y el cuerpo. El
transporte es inyectable ([`NetTransport`](src/vm.rs)), así que los tests corren
deterministas sin salir a la red ([`tests/net.rs`](tests/net.rs)).

**Actores remotos** (`SEND` a `actor://host:port/Actor`) — la misma capacidad `NET`
cubre la mensajería **entre nodos**: un `SEND` a una dirección `actor://` sale del
proceso y la respuesta del actor remoto vuelve como `on_message`. El transporte es
**HTTP** (un POST del mensaje de cable, cuya respuesta es el mensaje de vuelta), de
modo que **una misma forma de dirección funciona en ambos hosts**: el nativo hace el
POST con `ureq`, el navegador con `fetch` (el mismo `pumpNet` que ya usa `NET_FETCH`,
sin cambios). El nodo receptor corre con `--serve <host:puerto>` (un pequeño servidor
HTTP con CORS): inyecta cada mensaje a su actor destino (remitente `@caller`) y
responde con lo que ese actor mande vía `reply_to`. Así el llamante no necesita su
propio listener. Es el modelo de actores, ahora distribuido y agnóstico del host.

Cada mensaje viaja **firmado con Ed25519** (firma asimétrica; ver
[`src/wire.rs`](src/wire.rs)). Cada nodo tiene una **identidad** (par de claves): el
emisor firma con su clave privada e incluye su clave pública; el `--serve` verifica
la firma y comprueba que esa clave pública esté en su lista de **autorizadas**
(estilo `authorized_keys`). No hay secreto compartido: el servidor decide quién
puede hablar por su clave pública, y comprometer un nodo no permite firmar por otro.
El servidor atiende **un hilo por conexión** con un `Mutex` alrededor del estado: el
I/O es concurrente pero el procesamiento de mensajes queda serializado (un mensaje a
la vez, como el modelo de actores), de modo que un cliente lento no bloquea al resto.

```bash
# Identidad de un nodo (imprime su clave pública para autorizarla):
./target/release/stardust --print-identity --identity <hex64-semilla>

# Nodo B: solo acepta mensajes firmados por <pk-de-A>.
./target/release/stardust programs/saludador.json --serve 127.0.0.1:9945 --authorize <pk-de-A>
# Nodo A: firma con su identidad y llama al actor remoto.
./target/release/stardust programs/cliente_remoto.json --identity <hex64-semilla-de-A>
```

Sin flags, los nodos usan una **identidad de demostración** compartida (los demos
funcionan out-of-the-box); en un despliegue real cada nodo tendría la suya. La firma
del cable (Ed25519) es independiente de la clave HMAC de las ops `SIGN`/`VERIFY` de
actor (`--key`).

**Sockets** (`SOCK_SEND` a `sock://host:port`) — un stream TCP crudo request/response
(byte-nativo) para hablar con servicios que no son StardustVM. Envía `body`, lee la
respuesta y la entrega como `on_message` (record `sock_response`). Mismo gating
(`NET` + `net_allow`) y misma frontera asíncrona (se resuelve entre ciclos). A
diferencia de `actor://`, el TCP crudo **no es alcanzable desde el navegador** (el
sandbox no lo permite), así que `sock://` es solo del host nativo. Demos:
[`programs/saludador.json`](programs/saludador.json) +
[`programs/cliente_remoto.json`](programs/cliente_remoto.json).

### Procedimientos locales (funciones + recursión)

Un actor declara `procedures`: funciones locales con **parámetros**, **retorno**
(`returns`) y **scope aislado** por llamada. Se invocan con
`{"op":"CALL","proc":"fib","args":[<expr>],"into":"var"}`. El scope aislado habilita
**recursión** (cada llamada tiene sus propios locales), con un tope de profundidad
que convierte la recursión infinita en un fallo aislado. Sirven para no duplicar
lógica. Demo: [`programs/procedures.json`](programs/procedures.json) — `fib` y
`factorial` recursivos.

### Aclaración de especificación: el paso de mensajes

El documento dice que cada actor "solo ejecuta las 7 instrucciones base" **y** que
los actores "envían mensajes" entre sí. Para resolver esa tensión, en este POC el
paso de mensajes es el **sustrato de la StardustVM**, no una instrucción de cómputo:

- **`SEND`** — `{"op":"SEND","to":"Actor_X","value":<expr>}` encola un mensaje
  asíncrono. Es una primitiva del runtime (como el buzón), no una de las 7 ops.
- La **recepción** se declara en `on_message.bind` (no es una instrucción).

Así las 7 instrucciones siguen siendo el núcleo de cómputo puro, y la mensajería
es el andamiaje de actores. *(Decisión abierta a revisión — ver "Siguientes pasos".)*

## Interfaces gráficas (UI-IR)

Las UIs se describen con un **árbol de widgets semántico** (no comandos de dibujo),
agnóstico del backend. Un actor con la capacidad `"RENDER"` y un campo `view`
entra en un bucle **MVU** (Model-View-Update):

1. la VM renderiza `view` (función del estado del actor),
2. el usuario activa un widget,
3. ese evento entra como **mensaje** al actor (queda en la traza — observabilidad),
4. `on_message` actualiza el estado y se re-renderiza.

Widgets (`view`): `label` (texto enlazado a estado), `button` (emite `send` como
mensaje al pulsarlo; su `label` es un `<expr>`, con `to` opcional para enrutar a otro
actor), `row`, `column`, `grid` (con `columns`), `list` (repite una plantilla por cada
elemento de una lista del estado — el `FOREACH` de la vista), `input` (campo de texto:
`submit` emite un mensaje con el texto tecleado en `$input`), `image` (previsualiza
`Bytes`), `filedrop` (captura de archivos en web: emite `drop` con `$name`/`$bytes`),
`textarea` (editor multilínea: `submit` con el contenido en `$input`) y `html` (pinta
una cadena como HTML; el programa genera el marcado, escapando primero). Ejemplo mínimo:

```json
"view": { "type": "column", "children": [
  { "type": "label",  "text": { "var": "display" } },
  { "type": "button", "label": "7", "send": 7, "to": "Actor_Calculo" }
]}
```

**Un solo núcleo de render.** El árbol de widgets se resuelve contra el estado en Rust
([`src/render.rs`](src/render.rs): `build(view, mem) → RenderNode`, con el texto ya evaluado
y cada evento con su payload concreto) y se sirve **ya resuelto** a los hosts, de modo que
la lógica de binding (listas, variables de host) vive en un solo sitio. Las variables de
interacción `$input`/`$name`/`$bytes` viajan como marcadores `{"$host":…}` que el host
rellena antes de reinyectar el evento. Backends que consumen ese árbol:
- **Terminal** ([`src/ui.rs`](src/ui.rs)) — `RenderNode` → texto; la StardustVM corre nativa.
- **Web genérico** ([`web/app.template.html`](web/app.template.html)) — pide el árbol con
  `Engine.render()` y lo pinta a DOM (un `case` por widget, sin lógica de negocio); cada
  interacción vuelve con `Engine.event()`. **El mismo runtime de Rust compilado a `wasm32`**,
  y **un solo renderer para cualquier programa**: la calculadora, el explorador y un cliente
  de red se pintan con el mismo HTML. Es un host universal: además de pintar, espeja las
  escrituras `FILE` a IndexedDB (`take_dirty()`) y bombea la red (`take_outbound → fetch →
  deliver`) para `NET_FETCH`/`actor://`. Ensamblar: `web/build-app.sh <programa.json>`.
- **Web (JS, didáctico)** ([`web/renderer.html`](web/renderer.html)) — primera versión con
  un intérprete de StardustLang portado a JS. Se conserva como referencia.

Todos muestran el bus de mensajes en vivo (observabilidad) y el estado privado de cada
actor. La calculadora son dos actores aislados: `Actor_UI` (muestra) y `Actor_Calculo`
(calcula), comunicados solo por mensajes.

**Persistencia web** ([`web/persist.html`](web/persist.html)) — un contador cuyo estado
sobrevive a las recargas. El navegador no tiene FS, así que el host usa **persistencia
ortogonal**: serializa la memoria de los actores (`Engine.snapshot()`, etiquetada y sin
pérdidas) a `localStorage` y la restaura al cargar (`Engine.restore()`). Reconstruir:
[`web/build-persist.sh`](web/build-persist.sh).

**Sistema de archivos web** ([`web/explorador.html`](web/explorador.html)) — un explorador
que sube (`filedrop`), previsualiza (`image`: imágenes, PDFs, texto) y borra ficheros,
respaldado por **IndexedDB**. Es **StardustLang puro** ([`programs/explorador.json`](programs/explorador.json)):
un único actor `Sistema` con capacidades `["FILE","RENDER"]` y una `view` declarativa; no
lleva JavaScript a medida. Cada escritura pasa por la capacidad `FILE`
(`FILE_WRITE`/`FILE_DELETE`/`FILE_LIST` sobre el VFS de la VM, visibles en el bus); el host
lee `take_dirty()` y espeja solo los cambios a IndexedDB, y remonta el volumen al recargar
(`Engine.remount()`). Se ensambla con el **renderer web genérico**, no con una plantilla a
medida (ver más abajo). Reconstruir: `web/build-app.sh programs/explorador.json`.

**Escritorio de tiles** ([`web/playground.html`](web/playground.html)) — un escritorio estilo
tiling-WM (Hyprland): un panel lateral lista las apps guardadas (con botón "Nueva app" para
pegar un programa), y cada app abierta es una **ventana en mosaico** (layout master-stack: la
1ª ocupa la columna izquierda, el resto se apilan a la derecha) con botones de cerrar y
maximizar. Cada ventana es un **`Engine` independiente** con su propio disco persistente
(`stardust-run:<app>`), pintado con [`web/runtime.js`](web/runtime.js). El almacenamiento de las
apps va por la VM (VFS del actor `Biblioteca`, capacidad `FILE`, espejado a IndexedDB; ver
[`programs/biblioteca.json`](programs/biblioteca.json)). Ejecutar es territorio del host: la
StardustVM no tiene una instrucción "corre otro programa" (sería una VM anidada), así que el
escritorio instancia un `Engine` por ventana. Reconstruir: `web/build-playground.sh`.

**Editor de Markdown** ([`web/markdown.html`](web/markdown.html)) — edita (`textarea`), guarda
(`FILE`, persistido) y previsualiza Markdown. El **Markdown→HTML se hace entero en StardustLang**
([`programs/markdown.json`](programs/markdown.json)) con las ops de texto: escapa `< > &` con
`replace`, parte en líneas con `split`, detecta `#`/`##`/`- ` con `starts_with`+`slice`, aplica
`**negrita**`/`` `código` `` alternando `split`, y une con `join`; el widget `html` pinta el
resultado. Reconstruir: `web/build-app.sh programs/markdown.json`.

**Cliente de red web** ([`web/net-demo.html`](web/net-demo.html)) — un botón llama a
una API HTTP con la capacidad `NET` ([`programs/red_ui.json`](programs/red_ui.json)).
La petición no bloquea el intérprete (que en el navegador no puede esperar a `fetch`):
el actor `Cliente` ejecuta `NET_FETCH`, que **encola** la petición; el host la recoge
con `take_outbound()` (gemelo de `take_dirty()`), hace `fetch` y devuelve el resultado
con `deliver()`, que **reinyecta la respuesta como un `on_message`**. Toda la ida y
vuelta es visible en el bus. Sírvelo por HTTP (los módulos ES no cargan desde
`file://`). Reconstruir: [`web/build-net-demo.sh`](web/build-net-demo.sh).

**Actor remoto desde el navegador** ([`web/remote-demo.html`](web/remote-demo.html)) — un
actor `Cliente` en el navegador hace `SEND` a `actor://127.0.0.1:9945/Saludador`, un actor
que corre en un proceso nativo con `--serve` ([`programs/remoto_ui.json`](programs/remoto_ui.json)
+ [`programs/saludador.json`](programs/saludador.json)). **La misma dirección `actor://`
funciona en ambos hosts**: el navegador la resuelve con `fetch` (POST), el nativo con `ureq`,
reusando el mismo `pumpNet` sin cambios. Arranca antes el nodo:
`stardust programs/saludador.json --serve 127.0.0.1:9945`, sírvelo por HTTP y pulsa «Saludar».
Reconstruir: [`web/build-remote-demo.sh`](web/build-remote-demo.sh).

**Escritorio multi-app** ([`web/desktop.html`](web/desktop.html)) — dos apps con GUI
(calculadora + [`programs/fibonacci_ui.json`](programs/fibonacci_ui.json)) corriendo
como **dos StardustVM independientes** lado a lado en una pestaña, con un bus del sistema
compartido etiquetado por app. Demuestra aislamiento entre apps (memorias separadas)
en un mismo runtime WASM. Reconstruir con [`web/build-desktop.sh`](web/build-desktop.sh).

**Demo:** [`programs/calculadora.json`](programs/calculadora.json) — aislamiento
real UI/lógica en dos actores con contratos estrictos:
- `Actor_UI` (`RENDER`): solo `display`; renderiza y enruta los tokens a la lógica.
- `Actor_Calculo` (ciego): dueño de `entry`/`acc`/`op`; recibe un token, computa,
  y emite de vuelta el valor a mostrar. Ni una línea de UI.

Cada pulsación produce un viaje de ida y vuelta observable en la traza
(`UI -> Actor_Calculo -> Actor_UI`). Si la lógica falla, la VM la aísla y reinicia
sin colapsar la UI.

```bash
# Se teclea la etiqueta del botón (7, *, =, C…); 'q' para salir:
./target/release/stardust programs/calculadora.json --quiet
# En un solo comando (7 × 8 = 56):
printf "7\n*\n8\n=\nq\n" | ./target/release/stardust programs/calculadora.json --quiet
```

## Comunicación entre apps (APIs entre módulos)

Varias apps se hospedan en un mismo runtime y sus actores se llaman entre sí como
si fueran APIs. Un actor de una app es, de hecho, una API: su contrato de mensaje.

```bash
# El cliente calcula el área usando el módulo de OTRA app:
printf "10\n8\n" | ./target/release/stardust programs/cliente.json programs/geometria.json
```

- **Direccionamiento**: `app/actor`. Un `SEND` a `"geometria/Areas"` cruza la
  frontera; a `"Actor_X"` (sin `/`) es local a la app del emisor. `to` es un
  `<expr>`, así que puede ser dinámico (`{"var":"cliente"}`) para responder.
- **Manifiesto**: cada app declara `exports` (actores públicos) y `grants`
  (token → actores que desbloquea). Lo no exportado es **invisible** desde fuera.
- **Capacidades**: un `SEND` cross-app presenta `cap` (token). El broker autoriza
  solo si el actor está exportado **y** el token lo desbloquea; si no, **deniega**
  el salto (no es un fallo del actor, es política de frontera). Seguridad por
  capacidades (no cookies ambientales): el token nombra *y* autoriza.
- **Respuesta correlacionada**: al autorizar una llamada, el broker concede una
  capacidad de respuesta **transitoria** (el servicio solo puede responder a quien
  lo llamó). El servicio obtiene la dirección del llamante con `on_message.reply_to`.

Apps de ejemplo: [`geometria.json`](programs/geometria.json) (servicio que exporta
`Areas`) y [`cliente.json`](programs/cliente.json) (la consume con su token).
[`cliente_malo.json`](programs/cliente_malo.json) presenta un token inválido y el
broker lo rechaza.

## Validación para generadores (`--check`)

`stardust --check <programa.json>` valida **sin ejecutar** e imprime un informe JSON
`{ok, errors:[{path, message, hint}], warnings}` ([`src/check.rs`](src/check.rs)). A diferencia
del error de `serde`, cada problema trae la **ruta exacta** (`actors[1].on_message.body[3].left`)
y una **pista** con la forma correcta, y detecta lo que `serde` deja pasar en silencio (campos
desconocidos como un `"body"` en un `IF_COND`) más comprobaciones semánticas: `entry` o destino
de `SEND` inexistentes, capacidad no declarada, variable leída que nunca se define, `LOOP` cuya
condición no cambia, `CALL` a un procedimiento inexistente, URL fuera de `net_allow`… En el
navegador está disponible como `check_program()` del módulo WASM. Es el bucle de validación del
paso 2 de "Siguientes pasos".

## Servidor de programas (`stardust-server`)

Instala, versiona y valida programas por HTTP, sin interfaz gráfica: una IA o un script puede
instalar (`POST /api/v1/programs`) y actualizar (`POST …/versions` con `base_version`) un
programa, con usuarios y tokens de API. Los programas siguen ejecutándose en el navegador; el
servidor valida con el mismo código de la VM y devuelve los errores por línea.

```sh
cargo run --features server --bin stardust-server -- user add ana   # imprime su token
cargo run --features server --bin stardust-server                   # http://localhost:8080
curl -s localhost:8080/api/v1                                        # guía para agentes
```

Diseño y API: [`docs/servidor.md`](docs/servidor.md). Despliegue en Fly.io:
[`docs/deploy-fly.md`](docs/deploy-fly.md).

## Skill para Gemma en el teléfono (Google AI Edge Gallery)

[`skills/stardustlang/`](skills/stardustlang/) es una *JS skill* para
[AI Edge Gallery](https://github.com/google-ai-edge/gallery/tree/main/skills): un modelo on-device
(p. ej. **Gemma-4-E2B-it**) escribe StardustLang, lo valida con la **StardustVM real** y muestra la app
funcionando en el chat.

- [`SKILL.md`](skills/stardustlang/SKILL.md) — instrucciones compactas para un modelo pequeño: pasos,
  subconjunto del lenguaje en formato compacto (unos 2 KB, para no agotar el contexto del modelo), reglas y un ejemplo validado.
- `scripts/index.html` — webview oculto que el modelo invoca con `run_js`
  (`data = {"code": "<programa .stardust>"}`; también acepta la IR en JSON). Corre la StardustVM WASM:
  `compile_program()` (errores por línea), arranque y una prueba de
  cada botón/input en una VM limpia. Responde con un texto corto que empieza por `VÁLIDO` o
  `INVÁLIDO` (con rutas y pistas, máx. 3 errores) para que el modelo se corrija. Las respuestas son mínimas y el modelo no repite el JSON (el código se ve en el webview): Gemma on-device trabaja con una ventana de contexto pequeña.
- `assets/webview.html` — la app validada, funcionando en el chat (`?p=<programa en base64url>`).

```bash
skills/build-skill.sh                          # ensambla dist/skills/stardustlang/
adb push dist/skills/stardustlang /sdcard/Download/   # y en la app: Skills → (+) → Import local skill
```

También se puede publicar `dist/skills/` en un hosting web (GitHub Pages, con el `.nojekyll` que
genera el script) y usar «Load skill from URL».

## Arquitectura del código

| Archivo | Responsabilidad |
|---------|-----------------|
| `src/lib.rs`         | Raíz de la librería; núcleo común a los hosts nativo y WASM. |
| `src/program.rs`     | Esquema de StardustLang (deserialización serde del JSON). |
| `src/lang/`          | Sintaxis de texto `.stardust`: lexer, parser y bajada a la IR (`compile`, `check_source`). |
| `src/check.rs`       | Validador estático con errores localizados y pistas (`--check`, `check_program`). |
| `src/value.rs`       | Sistema de tipos: valores + aritmética/comparación/parseo centralizados. |
| `src/date.rs`        | Tipo `Date` autocontenido (calendario gregoriano, sin dependencias). |
| `src/error.rs`       | `RuntimeError` compartido (cada variante = fallo aislado). |
| `src/message.rs`     | El sobre de mensaje entre actores (compartido). |
| `src/interpreter.rs` | Flujo de control: ejecuta instrucciones sobre la memoria del actor. |
| `src/vm.rs`          | Planificador, enrutador, aislamiento, bucle MVU, host multi-app (nativo). |
| `src/ui.rs`          | Renderer de terminal: widgets → texto (nativo). |
| `src/wasm.rs`        | Motor WASM (`Engine`): expone la StardustVM al navegador vía wasm-bindgen. |
| `src/main.rs`        | CLI: carga apps `.json` y las ejecuta. |

El núcleo (tipos + intérprete + contratos) es **común**; `vm`/`ui` (E/S de consola)
se excluyen del build a WASM, y `wasm` provee un `Engine` que reutiliza el mismo
intérprete. Compilar a WASM: `./web/build-wasm.sh`.

## Modelo de ejecución

1. **Boot**: se ejecuta el `on_start` de cada actor (el `entry` primero) para
   inicializar su estado. Los mensajes encolados no se procesan hasta terminar el boot.
2. **Planificador**: mientras haya mensajes en la cola, se extrae uno y se ejecuta
   el `on_message` del destino bajo `catch_unwind`.
3. **Fallo**: si el manejador entra en pánico o devuelve error (div/0, capacidad
   denegada, variable indefinida, bucle infinito…), la VM lo registra, limpia la
   memoria del actor (reinicio) y **continúa** con la cola. El proceso no muere.

## Siguientes pasos

1. **[hecho]** Núcleo de la StardustVM con las 7 instrucciones + Fibonacci.
2. **Prompt de generación por IA** → ver [`PROMPT.md`](PROMPT.md): estructura para
   que un LLM emita StardustLang válido a partir de lenguaje natural.
3. **MVP avanzado** (sección 7): app web de planes de trabajo con 3 actores
   (Interfaz / Lógica / Estado) y reestructuración semántica.
4. Decisiones de diseño abiertas: ¿`SEND`/`RECV` como instrucciones formales?
   ¿concurrencia real (hilos) vs. planificador cooperativo? ¿bytecode binario?

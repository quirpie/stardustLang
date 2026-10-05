//! Esquema de StardustLang: el bytecode estructurado (JSON) que la IA debe emitir.
//!
//! Un programa es un grafo de actores aislados. Cada actor solo contiene las
//! 7 instrucciones base de cómputo; el paso de mensajes (`SEND` + `on_message`)
//! es el sustrato provisto por la StardustVM (ver README, sección "Aclaración de
//! especificación").

use std::collections::BTreeMap;

use serde::Deserialize;

/// Programa completo (= una "app"): entrada + red de actores + manifiesto público.
#[derive(Debug, Deserialize)]
pub struct Program {
    pub program: String,
    /// Actor de entrada declarado por la app (documental; la VM bootea todos los
    /// `on_start` en orden). Se conserva como parte del contrato de StardustLang.
    #[allow(dead_code)]
    pub entry: String,
    pub actors: Vec<Actor>,
    /// Manifiesto: actores llamables desde otras apps (superficie "API").
    /// Lo no exportado es invisible cruzando la frontera (mínimo privilegio).
    #[serde(default)]
    pub exports: Vec<String>,
    /// Capacidades aceptadas: token -> actores exportados que ese token desbloquea.
    /// Un `SEND` cross-app debe presentar un token válido para el actor destino.
    #[serde(default)]
    pub grants: BTreeMap<String, Vec<String>>,
    /// Allowlist de red: prefijos de URL alcanzables por los actores con capacidad
    /// `NET`. Mínimo privilegio, como el sandbox de `FILE`: una URL que no empiece
    /// por ningún prefijo se rechaza (lista vacía = ninguna red permitida).
    #[serde(default)]
    pub net_allow: Vec<String>,
}

/// Un actor aislado con memoria privada y capacidades declaradas.
#[derive(Debug, Deserialize, Clone)]
pub struct Actor {
    pub name: String,
    /// Permisos del actor. Solo quien declare `"IO_STREAM"` puede ejecutar E/S.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Instrucciones ejecutadas una sola vez, solo por el actor `entry`, al bootear.
    #[serde(default)]
    pub on_start: Vec<Instruction>,
    /// Manejador que se dispara cada vez que llega un mensaje al buzón del actor.
    #[serde(default)]
    pub on_message: MessageHandler,
    /// Procedimientos locales reutilizables (funciones con parámetros, retorno y
    /// scope aislado). Invocables con `CALL` desde los manejadores del actor.
    #[serde(default)]
    pub procedures: BTreeMap<String, Procedure>,
    /// Vista declarativa (UI-IR). Si está presente y el actor tiene la capacidad
    /// `"RENDER"`, la VM entra en un bucle interactivo MVU sobre este actor: la
    /// vista es función del estado, y cada pulsación de botón es un mensaje.
    #[serde(default)]
    pub view: Option<Widget>,
}

/// Un procedimiento local: función con parámetros, retorno y scope aislado.
/// El cuerpo corre sobre una memoria local fresca sembrada con los argumentos;
/// el valor de `returns` (si se indica) es el resultado de la llamada.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct Procedure {
    #[serde(default)]
    pub params: Vec<String>,
    #[serde(default)]
    pub returns: Option<String>,
    #[serde(default)]
    pub body: Vec<Instruction>,
}

/// Manejador de mensajes entrantes.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct MessageHandler {
    /// Nombre de la variable local donde se enlaza el payload recibido.
    #[serde(default)]
    pub bind: Option<String>,
    /// Variable donde enlazar la dirección del remitente (`app/actor`), para
    /// que un servicio pueda responder a quien lo llamó (respuesta correlacionada).
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Contrato de entrada opcional (campo -> tipo). Si está presente, la VM
    /// valida el payload entrante antes de ejecutar; un incumplimiento es un
    /// fallo aislado. Es la base del esquema para APIs entre módulos.
    #[serde(default)]
    pub expects: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub body: Vec<Instruction>,
}

/// Una expresión: de dónde sale un valor. Puede ser:
///   * una referencia a variable: `{"var": "x"}`
///   * un literal tipado: `{"lit": "2026-12-31", "as": "date"}`
///   * un constructor de record: `{"record": {"base": {"var":"b"}, "altura": 3}}`
///   * un acceso a campo: `{"field": "area", "from": {"var": "resp"}}`
///   * un literal corto: número (`0`, `3.14`), cadena (`"txt"`) o booleano (`true`)
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum Expr {
    Var {
        var: String,
    },
    /// Literal tipado explícito para tipos sin sintaxis corta (fechas, etc.).
    Typed {
        lit: serde_json::Value,
        #[serde(rename = "as")]
        as_type: String,
    },
    /// Construye un `Record` evaluando cada campo (payload de una API).
    Record {
        record: BTreeMap<String, Expr>,
    },
    /// Lee un campo de un `Record`.
    Field {
        field: String,
        from: Box<Expr>,
    },
    /// Construye una lista evaluando cada elemento: `{"list": [0, {"var":"x"}]}`.
    List {
        list: Vec<Expr>,
    },
    /// Elemento en una posición: `{"at": <idx>, "of": <lista>}`. Índice negativo
    /// cuenta desde el final (estilo Python).
    At {
        at: Box<Expr>,
        of: Box<Expr>,
    },
    /// Longitud de una lista (o texto/record/bytes): `{"len": <expr>}`.
    Len {
        len: Box<Expr>,
    },
    /// Literal binario desde base64: `{"bytes": "<base64>"}`.
    Bytes {
        bytes: String,
    },
    /// Codifica un texto (UTF-8) a `Bytes`: `{"to_bytes": <expr>}`.
    ToBytes {
        to_bytes: Box<Expr>,
    },
    /// Decodifica `Bytes` a texto (UTF-8): `{"from_bytes": <expr>}`.
    FromBytes {
        from_bytes: Box<Expr>,
    },
    /// Codifica `Bytes` a base64 (texto): `{"base64": <expr>}`.
    Base64 {
        base64: Box<Expr>,
    },
    /// Parte un texto por un separador: `{"split": <txt>, "on": <sep>}` -> lista de
    /// textos. `sep` vacío parte en caracteres.
    Split {
        split: Box<Expr>,
        on: Box<Expr>,
    },
    /// Une una lista en texto con un separador: `{"join": <lista>, "with": <sep>}`.
    Join {
        join: Box<Expr>,
        with: Box<Expr>,
    },
    /// Subcadena por índices de carácter: `{"slice": <txt>, "from": <i>, "to": <j>}`.
    /// `to` opcional (por defecto, el final). Índices negativos cuentan desde el
    /// final, como `at` (consistencia con el resto del lenguaje).
    Slice {
        slice: Box<Expr>,
        from: Box<Expr>,
        #[serde(default)]
        to: Option<Box<Expr>>,
    },
    /// Reemplaza **todas** las apariciones: `{"replace": <txt>, "find": <a>, "with": <b>}`.
    Replace {
        replace: Box<Expr>,
        find: Box<Expr>,
        with: Box<Expr>,
    },
    /// ¿Contiene una subcadena? `{"contains": <txt>, "sub": <x>}` -> booleano.
    Contains {
        contains: Box<Expr>,
        sub: Box<Expr>,
    },
    /// ¿Empieza por el prefijo? `{"starts_with": <txt>, "prefix": <p>}` -> booleano.
    StartsWith {
        starts_with: Box<Expr>,
        prefix: Box<Expr>,
    },
    /// ¿Termina en el sufijo? `{"ends_with": <txt>, "suffix": <s>}` -> booleano.
    EndsWith {
        ends_with: Box<Expr>,
        suffix: Box<Expr>,
    },
    /// Índice (de carácter) de la primera aparición, o -1: `{"index_of": <txt>, "sub": <x>}`.
    IndexOf {
        index_of: Box<Expr>,
        sub: Box<Expr>,
    },
    /// Texto en mayúsculas: `{"upper": <txt>}`.
    Upper {
        upper: Box<Expr>,
    },
    /// Texto en minúsculas: `{"lower": <txt>}`.
    Lower {
        lower: Box<Expr>,
    },
    /// Recorta espacios en ambos extremos: `{"trim": <txt>}`.
    Trim {
        trim: Box<Expr>,
    },
    /// Repite un texto `n` veces: `{"repeat": <txt>, "times": <n>}`.
    Repeat {
        repeat: Box<Expr>,
        times: Box<Expr>,
    },
    /// Convierte cualquier valor a su texto (Display): `{"to_str": <expr>}`.
    ToStr {
        to_str: Box<Expr>,
    },
    /// Texto → valor auto-tipado (`int` > `float` > `date` > `str`), como la
    /// entrada de `IO_STREAM in`. Para lo que teclea el usuario en un `input`
    /// (`$input` llega como texto). Un valor que no es texto se devuelve igual.
    Parse {
        parse: Box<Expr>,
    },
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}

/// Las 7 instrucciones base de StardustLang + `SEND` (sustrato de mensajería).
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "op")]
pub enum Instruction {
    /// 1. Definir una variable en la memoria privada del actor.
    #[serde(rename = "DEF_VAR")]
    DefVar {
        name: String,
        #[serde(default)]
        value: Option<Expr>,
    },
    /// 2. Asignar un valor a una variable existente.
    #[serde(rename = "ASSIGN")]
    Assign { target: String, value: Expr },
    /// 3. Condicional simple.
    #[serde(rename = "IF_COND")]
    IfCond {
        cond: Expr,
        #[serde(default)]
        then: Vec<Instruction>,
        #[serde(rename = "else", default)]
        otherwise: Vec<Instruction>,
    },
    /// 4. Bucle condicional (`while cond`).
    #[serde(rename = "LOOP")]
    Loop {
        cond: Expr,
        #[serde(default)]
        body: Vec<Instruction>,
    },
    /// 5. Operaciones lógicas (`==`, `!=`, `<`, `>`, `<=`, `>=`) -> booleano en `target`.
    #[serde(rename = "COMPARE")]
    Compare {
        target: String,
        operator: String,
        left: Expr,
        right: Expr,
    },
    /// 6. Operaciones aritméticas (`+`, `-`, `*`, `/`, `%`) -> resultado en `target`.
    ///    `+` con algún operando de tipo cadena actúa como concatenación.
    #[serde(rename = "MATH")]
    Math {
        target: String,
        operator: String,
        left: Expr,
        right: Expr,
    },
    /// 7. Entrada/salida con el usuario final. Requiere la capacidad `IO_STREAM`.
    #[serde(rename = "IO_STREAM")]
    IoStream {
        /// `"in"` (lee una línea a `target`) o `"out"` (imprime `prompt` y/o `value`).
        mode: String,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        value: Option<Expr>,
        #[serde(default)]
        prompt: Option<String>,
    },
    /// Persistencia: escribe `value` en un fichero. Byte-nativo: un `Bytes` se
    /// guarda crudo (imágenes, PDFs); el resto, su texto en UTF-8. Requiere la
    /// capacidad `FILE`; la ruta se confina al sandbox (relativa, sin `..`).
    #[serde(rename = "FILE_WRITE")]
    FileWrite { path: Expr, value: Expr },
    /// Añade `value` al final de un fichero (lo crea si no existe).
    #[serde(rename = "FILE_APPEND")]
    FileAppend { path: Expr, value: Expr },
    /// Lee un fichero a `into`. Por defecto auto-tipa el contenido como texto
    /// (int/float/date/str); con `"as":"bytes"` devuelve el binario tal cual.
    #[serde(rename = "FILE_READ")]
    FileRead {
        path: Expr,
        into: String,
        #[serde(rename = "as", default)]
        as_type: Option<String>,
    },
    /// Indica en `into` (booleano) si el fichero existe.
    #[serde(rename = "FILE_EXISTS")]
    FileExists { path: Expr, into: String },
    /// Lista en `into` (lista de texto) las rutas de los ficheros bajo `path`
    /// (prefijo/carpeta; omitido = todo el sandbox). Emula `ls`.
    #[serde(rename = "FILE_LIST")]
    FileList {
        #[serde(default)]
        path: Option<Expr>,
        into: String,
    },
    /// Borra un fichero del sandbox (no falla si no existe). Emula `rm`.
    #[serde(rename = "FILE_DELETE")]
    FileDelete { path: Expr },
    /// Hash SHA-256 de `value` (como texto) -> hex en `into`. Función pura, sin capacidad.
    #[serde(rename = "HASH")]
    Hash { value: Expr, into: String },
    /// Firma HMAC-SHA256 de `value` con la clave del host -> hex en `into`.
    /// Requiere la capacidad `CRYPTO`; el programa nunca ve la clave.
    #[serde(rename = "SIGN")]
    Sign { value: Expr, into: String },
    /// Verifica que `signature` corresponde a `value` -> booleano en `into`.
    /// Requiere la capacidad `CRYPTO`.
    #[serde(rename = "VERIFY")]
    Verify {
        value: Expr,
        signature: Expr,
        into: String,
    },
    /// Serializa `value` a texto JSON **etiquetado** (sin pérdidas) en `into`.
    #[serde(rename = "SERIALIZE")]
    Serialize { value: Expr, into: String },
    /// Deserializa un texto JSON etiquetado (`value`) de vuelta a un valor en `into`.
    #[serde(rename = "DESERIALIZE")]
    Deserialize { value: Expr, into: String },
    /// Invoca un procedimiento local: evalúa `args`, ejecuta el procedimiento en
    /// un scope aislado y (si hay `into`) guarda su retorno en esa variable.
    #[serde(rename = "CALL")]
    Call {
        #[serde(rename = "proc")]
        procedure: String,
        #[serde(default)]
        args: Vec<Expr>,
        #[serde(default)]
        into: Option<String>,
    },
    /// Añade un valor al final de la lista almacenada en `target` (la crea si
    /// no existe o es `null`).
    #[serde(rename = "APPEND")]
    Append { target: String, value: Expr },
    /// Itera una lista, enlazando cada elemento a `var` (y su posición a `index`
    /// si se indica) y ejecutando `body`.
    #[serde(rename = "FOREACH")]
    ForEach {
        #[serde(rename = "in")]
        source: Expr,
        var: String,
        #[serde(default)]
        index: Option<String>,
        #[serde(default)]
        body: Vec<Instruction>,
    },
    /// Sustrato de la StardustVM: encola un mensaje asíncrono hacia otro actor.
    /// `to` es un `<expr>` que resuelve a texto: un nombre local (`"Actor_X"`) o
    /// una dirección cross-app (`"app/actor"`), o `{"var":"caller"}` para responder.
    /// `cap` es el token de capacidad para cruzar la frontera de otra app.
    #[serde(rename = "SEND")]
    Send {
        to: Expr,
        value: Expr,
        #[serde(default)]
        cap: Option<String>,
    },
    /// Petición de red asíncrona. Requiere la capacidad `NET`; la URL se confina
    /// a la allowlist del programa (`net_allow`). No devuelve nada inline: encola
    /// una petición saliente cuya **respuesta llega como `on_message`** al actor,
    /// como record `{"kind":"net_response","corr":Int,"tag":<tag>,"url":Str,
    /// "ok":Bool,"status":Int,"error":Str|null,"headers":Record,"body":Bytes}`.
    /// `method` por defecto es `"GET"`; `tag` es un valor opaco que se devuelve
    /// tal cual en la respuesta (para casar peticiones con sus respuestas).
    #[serde(rename = "NET_FETCH")]
    NetFetch {
        #[serde(default)]
        method: Option<String>,
        url: Expr,
        #[serde(default)]
        headers: Option<Expr>,
        #[serde(default)]
        body: Option<Expr>,
        #[serde(default)]
        tag: Option<Expr>,
    },
    /// Stream TCP crudo (request/response) a `sock://host:port`. Requiere `NET` y
    /// que `addr` esté en la allowlist. Byte-nativo: `body` se envía crudo. **No
    /// devuelve inline**: la respuesta llega como `on_message` (record
    /// `{"kind":"sock_response","corr":Int,"tag":<eco>,"addr":Str,"ok":Bool,
    /// "error":Str|null,"body":Bytes}`).
    #[serde(rename = "SOCK_SEND")]
    SockSend {
        addr: Expr,
        body: Expr,
        #[serde(default)]
        tag: Option<Expr>,
    },
}

/// UI-IR: árbol de widgets **semántico** (no comandos de dibujo), agnóstico del
/// backend. Un renderer (terminal, egui, web…) lo traduce a píxeles; la IA y la
/// VM razonan a este nivel para preservar la observabilidad.
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Widget {
    /// Texto/valor mostrado. `text` es una expresión: enlaza a estado (`{"var":..}`).
    Label { text: Expr },
    /// Botón: al activarse envía `send` (un `<expr>`) como mensaje. Por defecto
    /// al propio `Actor_UI`; con `to` se enruta a otro actor (p. ej. la lógica).
    /// `label` es un `<expr>` (normalmente un texto literal, pero puede ligarse al
    /// estado para dar etiquetas distintas por elemento dentro de una `list`).
    Button {
        label: Expr,
        send: Expr,
        #[serde(default)]
        to: Option<String>,
    },
    /// Contenedor horizontal.
    Row {
        #[serde(default)]
        children: Vec<Widget>,
    },
    /// Contenedor vertical.
    Column {
        #[serde(default)]
        children: Vec<Widget>,
    },
    /// Cuadrícula de `columns` columnas (ideal para un teclado de calculadora).
    Grid {
        columns: usize,
        #[serde(default)]
        children: Vec<Widget>,
    },
    /// Lista ligada a estado: repite `item` por cada elemento de `bind` (un
    /// `<expr>` que resuelve a una lista), enlazando el elemento a `as` (y su
    /// posición a `index`) en el scope de la plantilla. `empty` se muestra si la
    /// lista está vacía. Es el `FOREACH` de la vista: sustituye al bucle que el
    /// host reconstruía a mano para pintar colecciones.
    List {
        bind: Expr,
        #[serde(rename = "as")]
        as_var: String,
        #[serde(default)]
        index: Option<String>,
        item: Box<Widget>,
        #[serde(default)]
        empty: Option<Box<Widget>>,
    },
    /// Campo de texto. Al confirmar (Enter) emite `submit` como mensaje; el texto
    /// tecleado está disponible ahí como la variable reservada `$input`. `src`
    /// (opcional) es el valor inicial mostrado (texto del estado). Por defecto va
    /// al propio `Actor_UI`; con `to` se enruta a otro actor.
    Input {
        #[serde(default)]
        src: Option<Expr>,
        #[serde(default)]
        placeholder: Option<String>,
        /// Si está presente, se pinta un botón con esta etiqueta que confirma el
        /// campo (igual que Enter). Útil para hacer visible la acción de un campo.
        #[serde(default)]
        label: Option<String>,
        submit: Expr,
        #[serde(default)]
        to: Option<String>,
    },
    /// Vista de datos binarios: `src` resuelve a `Bytes` (una imagen, p. ej.).
    /// `alt` (texto) describe el contenido. El backend decide cómo mostrarlo (el
    /// web como `<img>`; la terminal, un marcador textual).
    Image {
        src: Expr,
        #[serde(default)]
        alt: Option<Expr>,
    },
    /// Zona de captura de archivos (capacidad del host web). Por cada archivo
    /// soltado o elegido emite `drop` como mensaje, con las variables reservadas
    /// `$name` (texto) y `$bytes` (`Bytes`) disponibles. `to` enruta el mensaje.
    #[serde(rename = "filedrop")]
    FileDrop {
        #[serde(default)]
        label: Option<String>,
        drop: Expr,
        #[serde(default)]
        to: Option<String>,
    },
    /// Editor de texto multilínea. `src` es el valor inicial (texto del estado).
    /// Al enviar (botón o Ctrl/Cmd+Enter) emite `submit`, con el contenido en la
    /// variable reservada `$input`; `label` es la etiqueta del botón (def. "Guardar").
    Textarea {
        #[serde(default)]
        src: Option<Expr>,
        #[serde(default)]
        placeholder: Option<String>,
        #[serde(default)]
        label: Option<String>,
        submit: Expr,
        #[serde(default)]
        to: Option<String>,
    },
    /// Previsualiza una cadena como **HTML**: `{"type":"html","src":<expr>}`. El
    /// programa es responsable de generar HTML seguro (escapar `< > &`); el backend
    /// web lo inserta como marcado, el de terminal lo muestra como texto.
    Html {
        src: Expr,
    },
}

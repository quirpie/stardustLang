//! Intérprete de las instrucciones de StardustLang sobre la memoria privada de un actor.
//!
//! Es deliberadamente pequeño y sin `unsafe`. Las reglas de tipo (aritmética,
//! comparación, parseo) viven en el módulo [`crate::value`]; aquí solo está el
//! flujo de control. Cualquier condición anómala se reporta como
//! [`RuntimeError`], que la StardustVM trata como un fallo aislado del actor.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::path::{Component, Path};

use crate::crypto;
use crate::error::RuntimeError;
use crate::message::Message;
use crate::program::{Expr, Instruction, Procedure};
use crate::value::{self, Value};

/// Tope de iteraciones por bucle: red de seguridad contra bucles infinitos.
/// Al superarlo, la VM aísla y reinicia el actor (ver sección 8 del doc).
const LOOP_GUARD: u64 = 5_000_000;

/// Tope de profundidad de recursión de `CALL` (protege el stack del host).
const MAX_CALL_DEPTH: u32 = 512;

/// Backend de la capacidad `FILE`. El intérprete es **síncrono**, así que ambos
/// hosts exponen un almacén síncrono de bytes:
///   * el binario nativo usa el disco real, confinado a una raíz (sandbox);
///   * el navegador usa un VFS en memoria (ruta → bytes) que JS espeja a
///     IndexedDB en la frontera asíncrona, emulando un sistema de archivos.
/// Es byte-nativo: guarda binario (imágenes, PDFs) sin pérdidas.
pub enum FsBackend<'a> {
    /// Sin sistema de archivos (cualquier operación `FILE` falla).
    None,
    /// Disco real confinado a esta raíz.
    Native(&'a Path),
    /// VFS en memoria + registro de rutas mutadas por la VM, para que el host
    /// sepa qué espejar tras un `dispatch` (`(ruta, borrada?)`).
    Memory {
        files: &'a mut BTreeMap<String, Vec<u8>>,
        dirty: &'a mut Vec<(String, bool)>,
    },
}

impl<'a> FsBackend<'a> {
    /// Reborrow para pasar el backend a un sub-contexto (p. ej. un `CALL`) sin
    /// mover los préstamos mutables.
    fn reborrow(&mut self) -> FsBackend<'_> {
        match self {
            FsBackend::None => FsBackend::None,
            FsBackend::Native(p) => FsBackend::Native(p),
            FsBackend::Memory { files, dirty } => FsBackend::Memory { files, dirty },
        }
    }

    fn require(&self) -> Result<(), RuntimeError> {
        match self {
            FsBackend::None => Err(RuntimeError::Io(
                "no hay sistema de archivos disponible".into(),
            )),
            _ => Ok(()),
        }
    }

    fn write(&mut self, rel: &str, bytes: Vec<u8>, append: bool) -> Result<(), RuntimeError> {
        self.require()?;
        match self {
            FsBackend::None => unreachable!(),
            FsBackend::Native(root) => {
                let full = root.join(rel);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| RuntimeError::Io(e.to_string()))?;
                }
                if append {
                    use std::io::Write as _;
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&full)
                        .map_err(|e| RuntimeError::Io(e.to_string()))?;
                    f.write_all(&bytes)
                        .map_err(|e| RuntimeError::Io(e.to_string()))?;
                } else {
                    std::fs::write(&full, &bytes).map_err(|e| RuntimeError::Io(e.to_string()))?;
                }
            }
            FsBackend::Memory { files, dirty } => {
                if append {
                    files.entry(rel.to_string()).or_default().extend(bytes);
                } else {
                    files.insert(rel.to_string(), bytes);
                }
                dirty.push((rel.to_string(), false));
            }
        }
        Ok(())
    }

    fn read(&self, rel: &str) -> Result<Vec<u8>, RuntimeError> {
        self.require()?;
        match self {
            FsBackend::None => unreachable!(),
            FsBackend::Native(root) => std::fs::read(root.join(rel))
                .map_err(|e| RuntimeError::Io(format!("no se pudo leer '{rel}': {e}"))),
            FsBackend::Memory { files, .. } => files
                .get(rel)
                .cloned()
                .ok_or_else(|| RuntimeError::Io(format!("no existe el fichero '{rel}'"))),
        }
    }

    fn exists(&self, rel: &str) -> Result<bool, RuntimeError> {
        self.require()?;
        match self {
            FsBackend::None => unreachable!(),
            FsBackend::Native(root) => Ok(root.join(rel).exists()),
            FsBackend::Memory { files, .. } => Ok(files.contains_key(rel)),
        }
    }

    fn delete(&mut self, rel: &str) -> Result<(), RuntimeError> {
        self.require()?;
        match self {
            FsBackend::None => unreachable!(),
            FsBackend::Native(root) => {
                let full = root.join(rel);
                if full.exists() {
                    std::fs::remove_file(&full).map_err(|e| RuntimeError::Io(e.to_string()))?;
                }
            }
            FsBackend::Memory { files, dirty } => {
                if files.remove(rel).is_some() {
                    dirty.push((rel.to_string(), true));
                }
            }
        }
        Ok(())
    }

    /// Rutas de los ficheros bajo `prefix` (`""` = todos), en orden determinista.
    fn list(&self, prefix: &str) -> Result<Vec<String>, RuntimeError> {
        self.require()?;
        match self {
            FsBackend::None => unreachable!(),
            FsBackend::Native(root) => {
                let mut out = Vec::new();
                walk_dir(root, root, &mut out);
                out.retain(|p| prefix.is_empty() || p.starts_with(prefix));
                out.sort();
                Ok(out)
            }
            FsBackend::Memory { files, .. } => Ok(files
                .keys()
                .filter(|k| prefix.is_empty() || k.starts_with(prefix))
                .cloned()
                .collect()),
        }
    }
}

/// Recorre `dir` recursivamente acumulando rutas de fichero relativas a `base`
/// (separador `/`). Los errores de E/S se ignoran (directorio inexistente = vacío).
fn walk_dir(base: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_dir(base, &path, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Verifica que el actor posee una capacidad.
fn require_cap(capabilities: &[String], cap: &str) -> Result<(), RuntimeError> {
    if capabilities.iter().any(|c| c == cap) {
        Ok(())
    } else {
        Err(RuntimeError::CapabilityDenied(cap.into()))
    }
}

/// Normaliza una ruta relativa y la **confina** al sandbox: rechaza rutas
/// absolutas y componentes `..`. Devuelve la ruta canónica con separador `/`.
fn sanitize_rel(raw: &str) -> Result<String, RuntimeError> {
    let mut parts = Vec::new();
    for comp in Path::new(raw).components() {
        match comp {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => {
                return Err(RuntimeError::Io(format!(
                    "ruta fuera del sandbox (absoluta o con '..'): '{raw}'"
                )))
            }
        }
    }
    if parts.is_empty() {
        return Err(RuntimeError::Io(format!("ruta de fichero vacía: '{raw}'")));
    }
    Ok(parts.join("/"))
}

/// Como [`sanitize_rel`] pero admite el prefijo vacío (para listar todo).
fn sanitize_prefix(raw: &str) -> Result<String, RuntimeError> {
    if raw.trim().is_empty() {
        return Ok(String::new());
    }
    let mut parts = Vec::new();
    for comp in Path::new(raw).components() {
        match comp {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => {
                return Err(RuntimeError::Io(format!(
                    "prefijo fuera del sandbox (absoluto o con '..'): '{raw}'"
                )))
            }
        }
    }
    Ok(parts.join("/"))
}

/// Evalúa una `<expr>` y exige que sea texto (una ruta de fichero).
fn eval_str(expr: &Expr, memory: &HashMap<String, Value>) -> Result<String, RuntimeError> {
    match eval(expr, memory)? {
        Value::Str(s) => Ok(s),
        other => Err(RuntimeError::TypeError(format!(
            "la ruta de fichero debe ser texto, no {other}"
        ))),
    }
}

/// Una petición de red saliente que el intérprete **encola** (no ejecuta). El
/// host la drena tras cada ciclo de mensajes, hace el I/O asíncrono y entrega la
/// respuesta como un `on_message` al actor `from`. Es el gemelo de `vfs_dirty`
/// (capacidad `FILE`), con una pata extra: la respuesta correlacionada por `corr`.
#[derive(Debug, Clone)]
pub struct NetRequest {
    /// Actor que pidió la petición y que recibirá la respuesta.
    pub from: String,
    /// Id de correlación (para casar respuestas con su petición).
    pub corr: u64,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Valor opaco eco: se devuelve tal cual en la respuesta.
    pub tag: Value,
}

/// Respuesta cruda de una petición de red, independiente del cliente HTTP y del
/// host. Un `status` de 0 con `error` presente indica un fallo de transporte.
#[derive(Debug, Clone)]
pub struct NetResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub error: Option<String>,
}

impl NetResponse {
    /// Constructor de conveniencia para un fallo de transporte (DNS/TLS/timeout).
    pub fn transport_error(msg: impl Into<String>) -> Self {
        NetResponse {
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
            error: Some(msg.into()),
        }
    }
}

/// Un `SEND` a una dirección `actor://host:port/Actor`: mensajería entre nodos.
/// El host lo resuelve como un **request/reply sobre una conexión TCP** (envía el
/// mensaje al nodo remoto y lee su respuesta), y entrega esa respuesta como un
/// `on_message` al actor `from`. Reusa el mismo modelo de buzón, ahora distribuido.
#[derive(Debug, Clone)]
pub struct RemoteSend {
    pub from: String,
    pub corr: u64,
    /// Dirección completa `actor://host:port/Actor`.
    pub addr: String,
    pub cap: Option<String>,
    pub payload: Value,
}

/// Una petición a `sock://host:port`: un stream TCP crudo (request/response).
/// El host abre la conexión, escribe `body`, lee la respuesta y la entrega como
/// `on_message` (record `sock_response`). Byte-nativo: envía/recibe binario.
#[derive(Debug, Clone)]
pub struct SockRequest {
    pub from: String,
    pub corr: u64,
    /// Dirección `sock://host:port`.
    pub addr: String,
    pub body: Vec<u8>,
    pub tag: Value,
}

/// Trabajo saliente que el intérprete **encola** (no ejecuta) y el host resuelve
/// entre ciclos de mensajes: HTTP (`NET_FETCH`), mensaje a un actor remoto
/// (`SEND` a `actor://`) o un stream TCP (`SOCK_SEND` a `sock://`).
#[derive(Debug, Clone)]
pub enum Outbound {
    Http(NetRequest),
    Remote(RemoteSend),
    Sock(SockRequest),
}

/// Ensambla el record `net_response` que se entrega como `on_message` al actor
/// que pidió la petición. Host-agnóstico (solo toca [`Value`]): lo comparten el
/// host nativo (cliente HTTP) y el motor WASM (fetch en la frontera JS), para que
/// el contrato de la respuesta sea **idéntico** en ambos.
pub fn build_net_record(req: NetRequest, resp: NetResponse) -> Value {
    let mut rec: BTreeMap<String, Value> = BTreeMap::new();
    rec.insert("kind".into(), Value::Str("net_response".into()));
    rec.insert("corr".into(), Value::Int(req.corr as i64));
    rec.insert("tag".into(), req.tag);
    rec.insert("url".into(), Value::Str(req.url));
    rec.insert(
        "ok".into(),
        Value::Bool(resp.error.is_none() && (200..300).contains(&resp.status)),
    );
    rec.insert("status".into(), Value::Int(resp.status as i64));
    rec.insert(
        "error".into(),
        match resp.error {
            Some(e) => Value::Str(e),
            None => Value::Null,
        },
    );
    rec.insert(
        "headers".into(),
        Value::Record(
            resp.headers
                .into_iter()
                .map(|(k, v)| (k, Value::Str(v)))
                .collect(),
        ),
    );
    rec.insert("body".into(), Value::Bytes(resp.body));
    Value::Record(rec)
}

/// Record `remote_error` que se entrega cuando un `SEND` a `actor://` falla (o el
/// nodo remoto no respondió). El actor lo maneja como un mensaje más. Compartido
/// por ambos hosts (nativo: HTTP con `ureq`; navegador: HTTP con `fetch`).
pub fn remote_error_record(req: &RemoteSend, error: Option<String>) -> Value {
    let mut rec: BTreeMap<String, Value> = BTreeMap::new();
    rec.insert("kind".into(), Value::Str("remote_error".into()));
    rec.insert("corr".into(), Value::Int(req.corr as i64));
    rec.insert("addr".into(), Value::Str(req.addr.clone()));
    rec.insert(
        "error".into(),
        Value::Str(error.unwrap_or_else(|| "sin respuesta".into())),
    );
    Value::Record(rec)
}

/// Ensambla el record `sock_response` que se entrega como `on_message` tras un
/// `SOCK_SEND`. Más simple que el HTTP (no hay status/headers): solo el cuerpo.
pub fn build_sock_record(req: SockRequest, body: Vec<u8>, error: Option<String>) -> Value {
    let mut rec: BTreeMap<String, Value> = BTreeMap::new();
    rec.insert("kind".into(), Value::Str("sock_response".into()));
    rec.insert("corr".into(), Value::Int(req.corr as i64));
    rec.insert("tag".into(), req.tag);
    rec.insert("addr".into(), Value::Str(req.addr));
    rec.insert("ok".into(), Value::Bool(error.is_none()));
    rec.insert(
        "error".into(),
        match error {
            Some(e) => Value::Str(e),
            None => Value::Null,
        },
    );
    rec.insert("body".into(), Value::Bytes(body));
    Value::Record(rec)
}

/// Contexto de ejecución de un manejador: todo lo que el intérprete puede tocar.
pub struct ExecCtx<'a> {
    pub actor: &'a str,
    pub capabilities: &'a [String],
    pub memory: &'a mut HashMap<String, Value>,
    /// Cola global de mensajes de la VM (los `SEND` se encolan aquí).
    pub outbox: &'a mut std::collections::VecDeque<Message>,
    /// Buzón de salida: `NET_FETCH`, `SEND` a `actor://` y `SOCK_SEND` se encolan
    /// aquí. El host los ejecuta entre ciclos y reinyecta la respuesta como mensaje.
    pub outbound: &'a mut Vec<Outbound>,
    /// Allowlist de red (prefijos permitidos: `https://…`, `actor://…`, `sock://…`),
    /// análoga al sandbox de `FILE`.
    pub net_allow: &'a [String],
    /// Secuencia de correlación de red (compartida por todo el run).
    pub net_seq: &'a mut u64,
    /// Conjunto de nombres de actores válidos (para validar destinos de `SEND`).
    pub known_actors: &'a [String],
    /// Procedimientos locales del actor (para `CALL`).
    pub procedures: &'a BTreeMap<String, Procedure>,
    /// Backend de ficheros para la capacidad `FILE` (disco real o VFS en memoria).
    pub fs: FsBackend<'a>,
    /// Clave del host para `SIGN`/`VERIFY` (el programa nunca la ve).
    pub crypto_key: &'a [u8],
    /// Profundidad de recursión actual.
    pub depth: u32,
    /// Traza de observabilidad de la VM.
    pub trace: &'a mut Vec<String>,
    pub route_counter: &'a mut usize,
}

pub fn exec_block(instrs: &[Instruction], ctx: &mut ExecCtx) -> Result<(), RuntimeError> {
    for ins in instrs {
        exec(ins, ctx)?;
    }
    Ok(())
}

fn exec(ins: &Instruction, ctx: &mut ExecCtx) -> Result<(), RuntimeError> {
    match ins {
        Instruction::DefVar { name, value } => {
            let v = match value {
                Some(e) => eval(e, ctx.memory)?,
                None => Value::Null,
            };
            ctx.memory.insert(name.clone(), v);
        }
        Instruction::Assign { target, value } => {
            let v = eval(value, ctx.memory)?;
            ctx.memory.insert(target.clone(), v);
        }
        Instruction::IfCond {
            cond,
            then,
            otherwise,
        } => {
            if eval(cond, ctx.memory)?.is_truthy() {
                exec_block(then, ctx)?;
            } else {
                exec_block(otherwise, ctx)?;
            }
        }
        Instruction::Loop { cond, body } => {
            let mut guard: u64 = 0;
            while eval(cond, ctx.memory)?.is_truthy() {
                exec_block(body, ctx)?;
                guard += 1;
                if guard >= LOOP_GUARD {
                    return Err(RuntimeError::InfiniteLoop);
                }
            }
        }
        Instruction::Compare {
            target,
            operator,
            left,
            right,
        } => {
            let l = eval(left, ctx.memory)?;
            let r = eval(right, ctx.memory)?;
            let res = value::compare(operator, &l, &r)?;
            ctx.memory.insert(target.clone(), Value::Bool(res));
        }
        Instruction::Math {
            target,
            operator,
            left,
            right,
        } => {
            let l = eval(left, ctx.memory)?;
            let r = eval(right, ctx.memory)?;
            let res = value::arith(operator, &l, &r)?;
            ctx.memory.insert(target.clone(), res);
        }
        Instruction::IoStream {
            mode,
            target,
            value,
            prompt,
        } => {
            if !ctx.capabilities.iter().any(|c| c == "IO_STREAM") {
                return Err(RuntimeError::CapabilityDenied("IO_STREAM".into()));
            }
            match mode.as_str() {
                "out" => {
                    if let Some(p) = prompt {
                        print!("{p}");
                    }
                    if let Some(e) = value {
                        let v = eval(e, ctx.memory)?;
                        print!("{v}");
                    }
                    println!();
                    io::stdout().flush().map_err(|e| RuntimeError::Io(e.to_string()))?;
                }
                "in" => {
                    if let Some(p) = prompt {
                        print!("{p}");
                        io::stdout().flush().map_err(|e| RuntimeError::Io(e.to_string()))?;
                    }
                    let mut line = String::new();
                    io::stdin()
                        .read_line(&mut line)
                        .map_err(|e| RuntimeError::Io(e.to_string()))?;
                    // La entrada se clasifica al tipo más específico (int/float/date/str).
                    let v = value::parse_input(&line);
                    let tgt = target
                        .clone()
                        .ok_or_else(|| RuntimeError::Io("IO_STREAM 'in' sin 'target'".into()))?;
                    ctx.memory.insert(tgt, v);
                }
                other => {
                    return Err(RuntimeError::Io(format!("modo IO_STREAM inválido: '{other}'")))
                }
            }
        }
        Instruction::FileWrite { path, value } => {
            require_cap(ctx.capabilities, "FILE")?;
            let rel = sanitize_rel(&eval_str(path, ctx.memory)?)?;
            // Byte-nativo: `Bytes` se guarda crudo; el resto, su texto en UTF-8.
            let bytes = eval(value, ctx.memory)?.to_bytes();
            ctx.fs.write(&rel, bytes, false)?;
        }
        Instruction::FileAppend { path, value } => {
            require_cap(ctx.capabilities, "FILE")?;
            let rel = sanitize_rel(&eval_str(path, ctx.memory)?)?;
            let bytes = eval(value, ctx.memory)?.to_bytes();
            ctx.fs.write(&rel, bytes, true)?;
        }
        Instruction::FileRead { path, into, as_type } => {
            require_cap(ctx.capabilities, "FILE")?;
            let rel = sanitize_rel(&eval_str(path, ctx.memory)?)?;
            let bytes = ctx.fs.read(&rel)?;
            // `as:"bytes"` devuelve binario tal cual; por defecto, auto-tipa el
            // contenido como texto (compatibilidad con la persistencia previa).
            let v = match as_type.as_deref() {
                Some("bytes") => Value::Bytes(bytes),
                _ => value::parse_input(&String::from_utf8_lossy(&bytes)),
            };
            ctx.memory.insert(into.clone(), v);
        }
        Instruction::FileExists { path, into } => {
            require_cap(ctx.capabilities, "FILE")?;
            let rel = sanitize_rel(&eval_str(path, ctx.memory)?)?;
            let exists = ctx.fs.exists(&rel)?;
            ctx.memory.insert(into.clone(), Value::Bool(exists));
        }
        Instruction::FileList { path, into } => {
            require_cap(ctx.capabilities, "FILE")?;
            let prefix = match path {
                Some(e) => sanitize_prefix(&eval_str(e, ctx.memory)?)?,
                None => String::new(),
            };
            let names = ctx.fs.list(&prefix)?;
            ctx.memory.insert(
                into.clone(),
                Value::List(names.into_iter().map(Value::Str).collect()),
            );
        }
        Instruction::FileDelete { path } => {
            require_cap(ctx.capabilities, "FILE")?;
            let rel = sanitize_rel(&eval_str(path, ctx.memory)?)?;
            ctx.fs.delete(&rel)?;
        }
        Instruction::Hash { value, into } => {
            let bytes = eval(value, ctx.memory)?.to_bytes();
            let hex = crypto::to_hex(&crypto::sha256(&bytes));
            ctx.memory.insert(into.clone(), Value::Str(hex));
        }
        Instruction::Sign { value, into } => {
            if !ctx.capabilities.iter().any(|c| c == "CRYPTO") {
                return Err(RuntimeError::CapabilityDenied("CRYPTO".into()));
            }
            let bytes = eval(value, ctx.memory)?.to_bytes();
            let sig = crypto::to_hex(&crypto::hmac_sha256(ctx.crypto_key, &bytes));
            ctx.memory.insert(into.clone(), Value::Str(sig));
        }
        Instruction::Verify {
            value,
            signature,
            into,
        } => {
            if !ctx.capabilities.iter().any(|c| c == "CRYPTO") {
                return Err(RuntimeError::CapabilityDenied("CRYPTO".into()));
            }
            let bytes = eval(value, ctx.memory)?.to_bytes();
            let provided = match eval(signature, ctx.memory)? {
                Value::Str(s) => s,
                other => {
                    return Err(RuntimeError::TypeError(format!(
                        "la firma debe ser texto, no {other}"
                    )))
                }
            };
            let expected = crypto::to_hex(&crypto::hmac_sha256(ctx.crypto_key, &bytes));
            let ok = crypto::ct_eq(expected.as_bytes(), provided.as_bytes());
            ctx.memory.insert(into.clone(), Value::Bool(ok));
        }
        Instruction::Serialize { value, into } => {
            let v = eval(value, ctx.memory)?;
            let json = value::to_tagged(&v).to_string();
            ctx.memory.insert(into.clone(), Value::Str(json));
        }
        Instruction::Deserialize { value, into } => {
            let text = match eval(value, ctx.memory)? {
                Value::Str(s) => s,
                other => {
                    return Err(RuntimeError::TypeError(format!(
                        "DESERIALIZE requiere texto JSON, recibió {other}"
                    )))
                }
            };
            let j: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| RuntimeError::BadLiteral(format!("JSON inválido: {e}")))?;
            ctx.memory.insert(into.clone(), value::from_tagged(&j));
        }
        Instruction::Call {
            procedure,
            args,
            into,
        } => {
            if ctx.depth >= MAX_CALL_DEPTH {
                return Err(RuntimeError::CallDepthExceeded);
            }
            let proc = ctx
                .procedures
                .get(procedure)
                .ok_or_else(|| RuntimeError::UnknownProc(procedure.clone()))?;
            if args.len() != proc.params.len() {
                return Err(RuntimeError::ArityMismatch {
                    proc: procedure.clone(),
                    expected: proc.params.len(),
                    got: args.len(),
                });
            }
            // Scope local fresco: parámetros = argumentos evaluados en la memoria actual.
            let mut local: HashMap<String, Value> = HashMap::new();
            for (name, arg) in proc.params.iter().zip(args.iter()) {
                let v = eval(arg, ctx.memory)?;
                local.insert(name.clone(), v);
            }
            // Ejecutar el cuerpo en el scope aislado (reborrow del resto del contexto).
            {
                let mut subctx = ExecCtx {
                    actor: ctx.actor,
                    capabilities: ctx.capabilities,
                    memory: &mut local,
                    outbox: &mut *ctx.outbox,
                    outbound: &mut *ctx.outbound,
                    net_allow: ctx.net_allow,
                    net_seq: &mut *ctx.net_seq,
                    known_actors: ctx.known_actors,
                    procedures: ctx.procedures,
                    fs: ctx.fs.reborrow(),
                    crypto_key: ctx.crypto_key,
                    depth: ctx.depth + 1,
                    trace: &mut *ctx.trace,
                    route_counter: &mut *ctx.route_counter,
                };
                exec_block(&proc.body, &mut subctx)?;
            }
            // Guardar el retorno del procedimiento en la variable `into`.
            if let Some(into_var) = into {
                let result = proc
                    .returns
                    .as_ref()
                    .and_then(|r| local.get(r).cloned())
                    .unwrap_or(Value::Null);
                ctx.memory.insert(into_var.clone(), result);
            }
        }
        Instruction::Append { target, value } => {
            let v = eval(value, ctx.memory)?;
            let entry = ctx
                .memory
                .entry(target.clone())
                .or_insert_with(|| Value::List(Vec::new()));
            match entry {
                Value::List(items) => items.push(v),
                Value::Null => *entry = Value::List(vec![v]),
                other => {
                    return Err(RuntimeError::TypeError(format!(
                        "APPEND requiere una lista en '{target}', es {other}"
                    )))
                }
            }
        }
        Instruction::ForEach {
            source,
            var,
            index,
            body,
        } => {
            let items = match eval(source, ctx.memory)? {
                Value::List(items) => items,
                other => {
                    return Err(RuntimeError::TypeError(format!(
                        "FOREACH requiere una lista, recibió {other}"
                    )))
                }
            };
            // Se itera sobre una copia: mutar la lista original durante el bucle
            // no altera el recorrido.
            for (i, elem) in items.into_iter().enumerate() {
                ctx.memory.insert(var.clone(), elem);
                if let Some(idx) = index {
                    ctx.memory.insert(idx.clone(), Value::Int(i as i64));
                }
                exec_block(body, ctx)?;
            }
        }
        Instruction::Send { to, value, cap } => {
            // El destino es un texto: nombre local o dirección `app/actor`.
            let raw = match eval(to, ctx.memory)? {
                Value::Str(s) => s,
                other => {
                    return Err(RuntimeError::TypeError(format!(
                        "SEND 'to' debe ser texto (dirección), no {other}"
                    )))
                }
            };
            // Dirección remota `actor://host:port/Actor`: sale del proceso por la
            // red (capacidad `NET` + allowlist), no se busca entre los actores locales.
            if raw.starts_with("actor://") {
                require_cap(ctx.capabilities, "NET")?;
                if !ctx.net_allow.iter().any(|p| raw.starts_with(p)) {
                    return Err(RuntimeError::Io(format!(
                        "dirección remota fuera de la allowlist de red (net_allow): '{raw}'"
                    )));
                }
                let payload = eval(value, ctx.memory)?;
                *ctx.net_seq += 1;
                let corr = *ctx.net_seq;
                *ctx.route_counter += 1;
                ctx.trace.push(format!(
                    "route #{:03} {} -> {} :: {} (corr {})",
                    ctx.route_counter, ctx.actor, raw, payload, corr
                ));
                ctx.outbound.push(Outbound::Remote(RemoteSend {
                    from: ctx.actor.to_string(),
                    corr,
                    addr: raw,
                    cap: cap.clone(),
                    payload,
                }));
                return Ok(());
            }

            // `@caller` es el remitente sintético de un mensaje remoto entrante
            // (modo `--serve`): responder ahí no es local, lo captura el host para
            // devolverlo por el socket. Se deja pasar sin validar actores locales.
            let target = if raw == "@caller" {
                raw
            } else if raw.contains('/') {
                raw
            } else if let Some((app, _)) = ctx.actor.split_once('/') {
                // Resolución local: se prefija con la app del emisor.
                format!("{app}/{raw}")
            } else {
                raw
            };
            if target != "@caller" && !ctx.known_actors.iter().any(|a| *a == target) {
                return Err(RuntimeError::UnknownActor(target));
            }
            let payload = eval(value, ctx.memory)?;
            *ctx.route_counter += 1;
            ctx.trace.push(format!(
                "route #{:03} {} -> {} :: {}",
                ctx.route_counter, ctx.actor, target, payload
            ));
            ctx.outbox.push_back(Message {
                from: ctx.actor.to_string(),
                to: target,
                payload,
                cap: cap.clone(),
            });
        }
        Instruction::NetFetch {
            method,
            url,
            headers,
            body,
            tag,
        } => {
            require_cap(ctx.capabilities, "NET")?;
            let url = match eval(url, ctx.memory)? {
                Value::Str(s) => s,
                other => {
                    return Err(RuntimeError::TypeError(format!(
                        "NET_FETCH 'url' debe ser texto, no {other}"
                    )))
                }
            };
            // Allowlist: mínimo privilegio, como el sandbox de `FILE`. Una URL que
            // no empiece por ningún prefijo permitido se rechaza (fallo aislado).
            if !ctx.net_allow.iter().any(|p| url.starts_with(p)) {
                return Err(RuntimeError::Io(format!(
                    "URL fuera de la allowlist de red (net_allow): '{url}'"
                )));
            }
            let method = method
                .clone()
                .unwrap_or_else(|| "GET".to_string())
                .to_uppercase();
            let headers = match headers {
                Some(e) => match eval(e, ctx.memory)? {
                    Value::Record(m) => m
                        .into_iter()
                        .map(|(k, v)| {
                            let v = match v {
                                Value::Str(s) => s,
                                other => other.to_string(),
                            };
                            (k, v)
                        })
                        .collect(),
                    other => {
                        return Err(RuntimeError::TypeError(format!(
                            "NET_FETCH 'headers' debe ser un record, no {other}"
                        )))
                    }
                },
                None => Vec::new(),
            };
            let body = match body {
                Some(e) => eval(e, ctx.memory)?.to_bytes(),
                None => Vec::new(),
            };
            let tag = match tag {
                Some(e) => eval(e, ctx.memory)?,
                None => Value::Null,
            };
            *ctx.net_seq += 1;
            let corr = *ctx.net_seq;
            *ctx.route_counter += 1;
            ctx.trace.push(format!(
                "route #{:03} {} -> net:{} :: {} (corr {})",
                ctx.route_counter, ctx.actor, url, method, corr
            ));
            ctx.outbound.push(Outbound::Http(NetRequest {
                from: ctx.actor.to_string(),
                corr,
                method,
                url,
                headers,
                body,
                tag,
            }));
        }
        Instruction::SockSend { addr, body, tag } => {
            require_cap(ctx.capabilities, "NET")?;
            let addr = match eval(addr, ctx.memory)? {
                Value::Str(s) => s,
                other => {
                    return Err(RuntimeError::TypeError(format!(
                        "SOCK_SEND 'addr' debe ser texto, no {other}"
                    )))
                }
            };
            if !addr.starts_with("sock://") {
                return Err(RuntimeError::Io(format!(
                    "SOCK_SEND 'addr' debe empezar por 'sock://': '{addr}'"
                )));
            }
            if !ctx.net_allow.iter().any(|p| addr.starts_with(p)) {
                return Err(RuntimeError::Io(format!(
                    "dirección fuera de la allowlist de red (net_allow): '{addr}'"
                )));
            }
            let body = eval(body, ctx.memory)?.to_bytes();
            let tag = match tag {
                Some(e) => eval(e, ctx.memory)?,
                None => Value::Null,
            };
            *ctx.net_seq += 1;
            let corr = *ctx.net_seq;
            *ctx.route_counter += 1;
            ctx.trace.push(format!(
                "route #{:03} {} -> {} :: SOCK {} bytes (corr {})",
                ctx.route_counter,
                ctx.actor,
                addr,
                body.len(),
                corr
            ));
            ctx.outbound.push(Outbound::Sock(SockRequest {
                from: ctx.actor.to_string(),
                corr,
                addr,
                body,
                tag,
            }));
        }
    }
    Ok(())
}

/// Resuelve una expresión al valor concreto que representa.
/// Público para que el renderer de UI evalúe los enlaces de datos de la vista.
/// Evalúa una expresión exigiendo que resulte un entero (para índices de `slice`,
/// contadores de `repeat`, etc.).
fn eval_int(e: &Expr, memory: &HashMap<String, Value>) -> Result<i64, RuntimeError> {
    match eval(e, memory)? {
        Value::Int(n) => Ok(n),
        other => Err(RuntimeError::TypeError(format!(
            "se esperaba un entero, recibió {other}"
        ))),
    }
}

pub fn eval(expr: &Expr, memory: &HashMap<String, Value>) -> Result<Value, RuntimeError> {
    match expr {
        Expr::Var { var } => memory
            .get(var)
            .cloned()
            .ok_or_else(|| RuntimeError::UndefinedVar(var.clone())),
        Expr::Typed { lit, as_type } => value::construct(as_type, lit),
        Expr::Record { record } => {
            let mut m = std::collections::BTreeMap::new();
            for (k, e) in record {
                m.insert(k.clone(), eval(e, memory)?);
            }
            Ok(Value::Record(m))
        }
        Expr::Field { field, from } => match eval(from, memory)? {
            Value::Record(m) => m.get(field).cloned().ok_or_else(|| {
                RuntimeError::UndefinedVar(format!("campo '{field}' (no está en el record)"))
            }),
            other => Err(RuntimeError::TypeError(format!(
                "no se puede leer el campo '{field}' de {other} (no es un record)"
            ))),
        },
        Expr::List { list } => {
            let mut items = Vec::with_capacity(list.len());
            for e in list {
                items.push(eval(e, memory)?);
            }
            Ok(Value::List(items))
        }
        Expr::At { at, of } => match (eval(of, memory)?, eval(at, memory)?) {
            (Value::List(items), Value::Int(i)) => {
                let n = items.len() as i64;
                let real = if i < 0 { i + n } else { i }; // índice negativo desde el final
                if real < 0 || real >= n {
                    return Err(RuntimeError::TypeError(format!(
                        "índice {i} fuera de rango (longitud {n})"
                    )));
                }
                Ok(items[real as usize].clone())
            }
            (c, idx) => Err(RuntimeError::TypeError(format!(
                "'at' requiere una lista y un índice entero, recibió {c} y {idx}"
            ))),
        },
        Expr::Len { len } => match eval(len, memory)? {
            Value::List(items) => Ok(Value::Int(items.len() as i64)),
            Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
            Value::Record(m) => Ok(Value::Int(m.len() as i64)),
            Value::Bytes(b) => Ok(Value::Int(b.len() as i64)),
            other => Err(RuntimeError::TypeError(format!(
                "'len' requiere lista, texto, record o bytes, recibió {other}"
            ))),
        },
        Expr::Bytes { bytes } => crypto::base64_decode(bytes)
            .map(Value::Bytes)
            .ok_or_else(|| RuntimeError::BadLiteral(format!("base64 inválido: '{bytes}'"))),
        Expr::ToBytes { to_bytes } => match eval(to_bytes, memory)? {
            Value::Str(s) => Ok(Value::Bytes(s.into_bytes())),
            other => Ok(Value::Bytes(other.to_string().into_bytes())),
        },
        Expr::FromBytes { from_bytes } => match eval(from_bytes, memory)? {
            Value::Bytes(b) => Ok(Value::Str(String::from_utf8_lossy(&b).into_owned())),
            other => Err(RuntimeError::TypeError(format!(
                "'from_bytes' requiere bytes, recibió {other}"
            ))),
        },
        Expr::Base64 { base64 } => match eval(base64, memory)? {
            Value::Bytes(b) => Ok(Value::Str(crypto::base64_encode(&b))),
            other => Err(RuntimeError::TypeError(format!(
                "'base64' requiere bytes, recibió {other}"
            ))),
        },
        // --- Operaciones de texto (puras, sin capacidad). Char-based: los índices
        //     son de carácter y los negativos cuentan desde el final, como `at`. Los
        //     operandos no-texto se coercionan a su Display (p. ej. `join` de enteros). ---
        Expr::Split { split, on } => {
            let s = eval(split, memory)?.to_string();
            let sep = eval(on, memory)?.to_string();
            let parts: Vec<Value> = if sep.is_empty() {
                s.chars().map(|c| Value::Str(c.to_string())).collect()
            } else {
                s.split(sep.as_str()).map(|p| Value::Str(p.to_string())).collect()
            };
            Ok(Value::List(parts))
        }
        Expr::Join { join, with } => {
            let sep = eval(with, memory)?.to_string();
            match eval(join, memory)? {
                Value::List(items) => Ok(Value::Str(
                    items.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(&sep),
                )),
                other => Err(RuntimeError::TypeError(format!(
                    "'join' requiere una lista, recibió {other}"
                ))),
            }
        }
        Expr::Slice { slice, from, to } => {
            let s = eval(slice, memory)?.to_string();
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len() as i64;
            let norm = |i: i64| -> usize {
                let r = if i < 0 { i + n } else { i };
                r.clamp(0, n) as usize
            };
            let start = norm(eval_int(from, memory)?);
            let end = match to {
                Some(e) => norm(eval_int(e, memory)?),
                None => chars.len(),
            };
            let end = end.max(start);
            Ok(Value::Str(chars[start..end].iter().collect()))
        }
        Expr::Replace { replace, find, with } => {
            let s = eval(replace, memory)?.to_string();
            let f = eval(find, memory)?.to_string();
            let w = eval(with, memory)?.to_string();
            if f.is_empty() {
                return Ok(Value::Str(s)); // evita expansión infinita
            }
            Ok(Value::Str(s.replace(f.as_str(), w.as_str())))
        }
        Expr::Contains { contains, sub } => match eval(contains, memory)? {
            // En una lista: pertenencia (antes se convertía a texto y `1 in [10]` daba verdadero).
            Value::List(items) => {
                let x = eval(sub, memory)?;
                Ok(Value::Bool(items.iter().any(|i| crate::value::values_equal(i, &x))))
            }
            other => {
                let sub = eval(sub, memory)?.to_string();
                Ok(Value::Bool(other.to_string().contains(sub.as_str())))
            }
        },
        Expr::StartsWith { starts_with, prefix } => {
            let s = eval(starts_with, memory)?.to_string();
            let p = eval(prefix, memory)?.to_string();
            Ok(Value::Bool(s.starts_with(p.as_str())))
        }
        Expr::EndsWith { ends_with, suffix } => {
            let s = eval(ends_with, memory)?.to_string();
            let suf = eval(suffix, memory)?.to_string();
            Ok(Value::Bool(s.ends_with(suf.as_str())))
        }
        Expr::IndexOf { index_of, sub } => {
            let s = eval(index_of, memory)?.to_string();
            let sub = eval(sub, memory)?.to_string();
            let idx = match s.find(sub.as_str()) {
                Some(b) => s[..b].chars().count() as i64,
                None => -1,
            };
            Ok(Value::Int(idx))
        }
        Expr::Upper { upper } => Ok(Value::Str(eval(upper, memory)?.to_string().to_uppercase())),
        Expr::Lower { lower } => Ok(Value::Str(eval(lower, memory)?.to_string().to_lowercase())),
        Expr::Trim { trim } => Ok(Value::Str(eval(trim, memory)?.to_string().trim().to_string())),
        Expr::Repeat { repeat, times } => {
            let s = eval(repeat, memory)?.to_string();
            let n = eval_int(times, memory)?.max(0) as usize;
            if s.len().saturating_mul(n) > 8_000_000 {
                return Err(RuntimeError::TypeError(
                    "'repeat' produciría un texto demasiado grande".into(),
                ));
            }
            Ok(Value::Str(s.repeat(n)))
        }
        Expr::ToStr { to_str } => Ok(Value::Str(eval(to_str, memory)?.to_string())),
        Expr::Parse { parse } => Ok(match eval(parse, memory)? {
            Value::Str(s) => crate::value::parse_input(&s),
            other => other,
        }),
        Expr::Int(n) => Ok(Value::Int(*n)),
        Expr::Float(x) => Ok(Value::Float(*x)),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Str(s) => Ok(Value::Str(s.clone())),
    }
}

#[cfg(test)]
mod string_ops_tests {
    use super::*;
    use crate::value::Value;
    use std::collections::HashMap;

    fn ev(json: &str) -> Value {
        let expr: crate::program::Expr = serde_json::from_str(json).unwrap();
        eval(&expr, &HashMap::new()).unwrap()
    }

    #[test]
    fn split_join_roundtrip() {
        assert_eq!(
            ev(r#"{"split":"a,b,c","on":","}"#),
            Value::List(vec![
                Value::Str("a".into()),
                Value::Str("b".into()),
                Value::Str("c".into())
            ])
        );
        assert_eq!(
            ev(r#"{"join":{"split":"a,b,c","on":","},"with":"-"}"#),
            Value::Str("a-b-c".into())
        );
        // separador vacío -> caracteres
        assert_eq!(ev(r#"{"len":{"split":"héy","on":""}}"#), Value::Int(3));
    }

    #[test]
    fn slice_char_based_y_negativos() {
        assert_eq!(ev(r#"{"slice":"hola","from":1,"to":3}"#), Value::Str("ol".into()));
        assert_eq!(ev(r#"{"slice":"hola","from":-2}"#), Value::Str("la".into()));
        // char-based con Unicode (no bytes)
        assert_eq!(ev(r#"{"slice":"áéí","from":1,"to":2}"#), Value::Str("é".into()));
        // fuera de rango -> se recorta a vacío
        assert_eq!(ev(r#"{"slice":"hi","from":5}"#), Value::Str("".into()));
    }

    #[test]
    fn replace_contains_index_prefijos() {
        assert_eq!(ev(r#"{"replace":"a.b.a","find":"a","with":"X"}"#), Value::Str("X.b.X".into()));
        assert_eq!(ev(r#"{"contains":"hola mundo","sub":"mundo"}"#), Value::Bool(true));
        assert_eq!(ev(r##"{"starts_with":"# titulo","prefix":"# "}"##), Value::Bool(true));
        assert_eq!(ev(r#"{"ends_with":"nota.md","suffix":".md"}"#), Value::Bool(true));
        assert_eq!(ev(r#"{"index_of":"hola","sub":"l"}"#), Value::Int(2));
        assert_eq!(ev(r#"{"index_of":"hola","sub":"z"}"#), Value::Int(-1));
    }

    #[test]
    fn transformaciones() {
        assert_eq!(ev(r#"{"upper":"hola"}"#), Value::Str("HOLA".into()));
        assert_eq!(ev(r#"{"lower":"HoLa"}"#), Value::Str("hola".into()));
        assert_eq!(ev(r#"{"trim":"  x  "}"#), Value::Str("x".into()));
        assert_eq!(ev(r#"{"repeat":"ab","times":3}"#), Value::Str("ababab".into()));
        // to_str coacciona cualquier valor
        assert_eq!(ev(r#"{"to_str":42}"#), Value::Str("42".into()));
    }
}

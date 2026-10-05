//! Motor WASM: expone la StardustVM al navegador. Reutiliza **el mismo** intérprete,
//! sistema de tipos y contratos que el binario de terminal — una sola
//! implementación del runtime, dos hosts.
//!
//! El renderer web (DOM) mantiene el árbol de widgets (que ya tiene en el JSON del
//! programa) y delega toda la lógica aquí: `dispatch` inyecta el evento de un
//! botón como mensaje, drena la cola, y devuelve la traza + el estado resultante.

use std::collections::{BTreeMap, HashMap, VecDeque};

use wasm_bindgen::prelude::*;

use crate::error::RuntimeError;
use crate::interpreter::{
    build_net_record, eval, exec_block, remote_error_record, ExecCtx, FsBackend, NetResponse,
    Outbound,
};
use crate::message::Message;
use crate::program::{Actor, Expr, Program};
use crate::value::{self, Value};
use crate::wire;

struct ActorRt {
    def: Actor,
    memory: HashMap<String, Value>,
}

#[wasm_bindgen]
pub struct Engine {
    actors: HashMap<String, ActorRt>,
    order: Vec<String>,
    ui_actor: String,
    counter: usize,
    /// Sistema de archivos virtual (ruta → bytes): el "disco" que la capacidad
    /// `FILE` ve. El host lo espeja a IndexedDB en la frontera asíncrona.
    vfs: BTreeMap<String, Vec<u8>>,
    /// Rutas mutadas por la VM desde el último `take_dirty` (`(ruta, borrada?)`),
    /// para que el host sepa qué registros de IndexedDB actualizar/eliminar.
    vfs_dirty: Vec<(String, bool)>,
    /// Allowlist de red (capacidad `NET`): prefijos de URL alcanzables.
    net_allow: Vec<String>,
    /// Secuencia de correlación de red.
    net_seq: u64,
    /// Trabajo de red encolado (por ahora en el navegador solo HTTP), aún no
    /// entregado al host JS. Se recoge con `take_outbound()` (gemelo de `take_dirty`).
    net_outbox: Vec<Outbound>,
    /// Trabajo ya entregado al host, esperando su `deliver()` (por `corr`). Guarda
    /// el `Outbound` completo para que `deliver` sepa si construir un `net_response`
    /// (HTTP) o desenvolver la respuesta de un actor remoto.
    net_pending: HashMap<u64, Outbound>,
    /// Clave HMAC del host para las ops `SIGN`/`VERIFY` de actor (capacidad `CRYPTO`).
    crypto_key: Vec<u8>,
    /// Identidad Ed25519 (semilla privada) para firmar los mensajes a otros nodos
    /// (`actor://`). Va embebida; el nodo `--serve` debe autorizar su clave pública.
    identity: [u8; 32],
    /// Traza del boot inicial (los `on_start` que corrió el constructor), para que
    /// el host pueda mostrarla o detectar fallos de arranque (`boot_trace()`).
    boot_trace: Vec<String>,
}

#[wasm_bindgen]
impl Engine {
    /// Carga un programa StardustLang (JSON) y ejecuta el `on_start` de cada actor.
    #[wasm_bindgen(constructor)]
    pub fn new(program_json: &str) -> Result<Engine, JsValue> {
        let prog: Program =
            serde_json::from_str(program_json).map_err(|e| JsValue::from_str(&e.to_string()))?;
        let net_allow = prog.net_allow.clone();
        let mut actors = HashMap::new();
        let mut order = Vec::new();
        for a in prog.actors {
            order.push(a.name.clone());
            actors.insert(
                a.name.clone(),
                ActorRt {
                    def: a,
                    memory: HashMap::new(),
                },
            );
        }
        let ui_actor = order
            .iter()
            .find(|n| {
                let d = &actors[*n].def;
                d.view.is_some() && d.capabilities.iter().any(|c| c == "RENDER")
            })
            .cloned()
            .unwrap_or_default();

        let mut engine = Engine {
            actors,
            order,
            ui_actor,
            counter: 0,
            vfs: BTreeMap::new(),
            vfs_dirty: Vec::new(),
            net_allow,
            net_seq: 0,
            net_outbox: Vec::new(),
            net_pending: HashMap::new(),
            crypto_key: b"stardust-dev-secret-key".to_vec(),
            identity: wire::DEMO_SEED,
            boot_trace: Vec::new(),
        };
        let mut trace = Vec::new();
        engine.boot(&mut trace);
        engine.boot_trace = trace;
        Ok(engine)
    }

    /// Traza del boot que ejecutó el constructor, como JSON `["línea", …]`.
    pub fn boot_trace(&self) -> String {
        serde_json::to_string(&self.boot_trace).unwrap_or_else(|_| "[]".into())
    }

    /// Nombre del actor que posee la vista (Actor_UI).
    pub fn ui_actor(&self) -> String {
        self.ui_actor.clone()
    }

    /// Árbol de UI **ya resuelto** contra el estado del `Actor_UI`: texto
    /// evaluado y eventos con su payload concreto (ver [`crate::render`]). El
    /// pintor web solo lo traduce a DOM, sin reinterpretar expresiones — toda la
    /// lógica de binding vive en el núcleo Rust, igual que en la terminal.
    /// Devuelve `"null"` si el programa no tiene vista.
    pub fn render(&self) -> String {
        let empty = HashMap::new();
        let (view, mem) = match self.actors.get(&self.ui_actor) {
            Some(a) => (a.def.view.as_ref(), &a.memory),
            None => (None, &empty),
        };
        match view {
            Some(v) => serde_json::to_string(&crate::render::build(v, mem))
                .unwrap_or_else(|_| "null".into()),
            None => "null".into(),
        }
    }

    /// Re-ejecuta los `on_start` de todos los actores y devuelve `{trace,state}`.
    /// El host lo llama tras **montar** el VFS (p. ej. desde IndexedDB), para que
    /// un `FILE_LIST` en `on_start` refleje el disco ya poblado en vez del vacío
    /// que existía al construir el `Engine`.
    pub fn remount(&mut self) -> String {
        let mut trace = Vec::new();
        self.boot(&mut trace);
        serde_json::json!({ "trace": trace, "state": self.state_value() }).to_string()
    }

    /// Estado actual (memoria privada de cada actor) como JSON.
    pub fn state_json(&self) -> String {
        self.state_value().to_string()
    }

    /// Snapshot **etiquetado y sin pérdidas** del estado de los actores, para
    /// persistir (p. ej. en localStorage). Conserva fechas y binarios (a
    /// diferencia de `state_json`, que es solo para mostrar).
    pub fn snapshot(&self) -> String {
        let mut obj = serde_json::Map::new();
        for name in &self.order {
            let mem: serde_json::Map<String, serde_json::Value> = self.actors[name]
                .memory
                .iter()
                .map(|(k, v)| (k.clone(), value::to_tagged(v)))
                .collect();
            obj.insert(
                name.clone(),
                serde_json::json!({ "memory": serde_json::Value::Object(mem) }),
            );
        }
        serde_json::Value::Object(obj).to_string()
    }

    /// Restaura la memoria de los actores desde un `snapshot` etiquetado.
    pub fn restore(&mut self, snapshot: &str) -> Result<(), JsValue> {
        let parsed: serde_json::Value =
            serde_json::from_str(snapshot).map_err(|e| JsValue::from_str(&e.to_string()))?;
        if let Some(obj) = parsed.as_object() {
            for (name, actor_state) in obj {
                if let Some(rt) = self.actors.get_mut(name) {
                    if let Some(mem) = actor_state.get("memory").and_then(|m| m.as_object()) {
                        rt.memory = mem
                            .iter()
                            .map(|(k, v)| (k.clone(), value::from_tagged(v)))
                            .collect();
                    }
                }
            }
        }
        Ok(())
    }

    /// Monta un fichero en el VFS desde el host (p. ej. al restaurar desde
    /// IndexedDB al arrancar). Es infraestructura, no una acción del programa:
    /// no pasa por la capacidad `FILE` ni se registra como cambio.
    pub fn vfs_put(&mut self, path: &str, data: &[u8]) {
        self.vfs.insert(path.to_string(), data.to_vec());
    }

    /// Bytes de un fichero del VFS (para renderizar un preview en el host), o
    /// `undefined` si no existe.
    pub fn vfs_get(&self, path: &str) -> Option<Vec<u8>> {
        self.vfs.get(path).cloned()
    }

    /// Elimina un fichero del VFS desde el host (sin registrarlo como cambio).
    pub fn vfs_forget(&mut self, path: &str) {
        self.vfs.remove(path);
    }

    /// Índice del VFS como JSON `[{"path":..,"size":..}]`, orden determinista.
    pub fn vfs_list(&self) -> String {
        let arr: Vec<serde_json::Value> = self
            .vfs
            .iter()
            .map(|(p, b)| serde_json::json!({ "path": p, "size": b.len() }))
            .collect();
        serde_json::Value::Array(arr).to_string()
    }

    /// Consume el registro de rutas mutadas por la VM desde la última llamada,
    /// como JSON `[{"path":..,"deleted":bool}]`. El host lo usa tras un
    /// `dispatch` para saber qué espejar a IndexedDB (escribir o borrar).
    pub fn take_dirty(&mut self) -> String {
        let arr: Vec<serde_json::Value> = self
            .vfs_dirty
            .drain(..)
            .map(|(p, deleted)| serde_json::json!({ "path": p, "deleted": deleted }))
            .collect();
        serde_json::Value::Array(arr).to_string()
    }

    /// Procesa el evento de un botón: evalúa su `send` (un `<expr>`) contra la
    /// memoria del Actor_UI, lo inyecta como mensaje a `target`, drena la cola y
    /// devuelve `{ "trace": [...], "state": {...} }`.
    pub fn dispatch(&mut self, target: &str, send_json: &str) -> Result<String, JsValue> {
        let expr: Expr =
            serde_json::from_str(send_json).map_err(|e| JsValue::from_str(&e.to_string()))?;
        // El `send` se evalúa contra la memoria del Actor_UI; si no hay UI (o falta),
        // se usa una memoria vacía en vez de panicar al indexar.
        let empty = HashMap::new();
        let ui_mem = self
            .actors
            .get(&self.ui_actor)
            .map(|a| &a.memory)
            .unwrap_or(&empty);
        let payload = eval(&expr, ui_mem).map_err(|e| JsValue::from_str(&e.to_string()))?;

        let mut trace = Vec::new();
        let mut queue = VecDeque::new();
        self.counter += 1;
        trace.push(format!(
            "route #{:03} UI -> {} :: {}",
            self.counter, target, payload
        ));
        queue.push_back(Message {
            from: "UI".to_string(),
            to: target.to_string(),
            payload,
            cap: None,
        });
        self.drain(&mut queue, &mut trace);

        Ok(serde_json::json!({
            "trace": trace,
            "state": self.state_value(),
        })
        .to_string())
    }

    /// Inyecta como mensaje a `target` un payload **ya resuelto** (JSON
    /// etiquetado, tal como lo produce `render()` tras rellenar los marcadores
    /// `{"$host":…}` con el dato de la interacción). A diferencia de `dispatch`,
    /// no evalúa una expresión: el árbol resuelto ya hizo ese trabajo en el
    /// núcleo. `target` vacío enruta al `Actor_UI`.
    pub fn event(&mut self, target: &str, payload_json: &str) -> Result<String, JsValue> {
        let j: serde_json::Value =
            serde_json::from_str(payload_json).map_err(|e| JsValue::from_str(&e.to_string()))?;
        let payload = value::from_tagged(&j);
        let target = if target.is_empty() {
            self.ui_actor.clone()
        } else {
            target.to_string()
        };

        let mut trace = Vec::new();
        let mut queue = VecDeque::new();
        self.counter += 1;
        trace.push(format!(
            "route #{:03} UI -> {} :: {}",
            self.counter, target, payload
        ));
        queue.push_back(Message {
            from: "UI".to_string(),
            to: target,
            payload,
            cap: None,
        });
        self.drain(&mut queue, &mut trace);

        Ok(serde_json::json!({
            "trace": trace,
            "state": self.state_value(),
        })
        .to_string())
    }

    /// Recoge las peticiones de red pendientes (encoladas por `NET_FETCH`) para
    /// que el host JS las ejecute con `fetch`. Es el **gemelo de `take_dirty()`**:
    /// devuelve un array JSON `[{corr, method, url, headers, body_b64}]` y cada
    /// petición pasa a "pendiente" hasta que llegue su `deliver()`. El bucle del
    /// host: tras `new`/`dispatch`/`deliver`, llamar a `take_outbound()` y, por
    /// cada entrada, hacer `fetch` y devolver el resultado con `deliver()`.
    pub fn take_outbound(&mut self) -> String {
        let mut arr = Vec::new();
        for item in std::mem::take(&mut self.net_outbox) {
            // Cada trabajo se describe como una petición HTTP uniforme para que el
            // mismo `pumpNet` (fetch) lo resuelva sin ramificar:
            //   * `NET_FETCH`  -> su método/URL/headers/cuerpo.
            //   * `SEND` a `actor://host:port/Actor` -> POST a `http://host:port/`
            //     con el mensaje de cable; misma forma que el host nativo.
            //   * `sock://` (TCP crudo) no es alcanzable desde el navegador: se omite.
            let (corr, desc) = match &item {
                Outbound::Http(req) => {
                    let headers: serde_json::Map<String, serde_json::Value> = req
                        .headers
                        .iter()
                        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                        .collect();
                    (
                        req.corr,
                        serde_json::json!({
                            "corr": req.corr,
                            "method": req.method,
                            "url": req.url,
                            "headers": headers,
                            "body_b64": crate::crypto::base64_encode(&req.body),
                        }),
                    )
                }
                Outbound::Remote(req) => {
                    let rest = &req.addr["actor://".len()..];
                    let Some((hostport, actor)) = rest.split_once('/').filter(|(_, a)| !a.is_empty())
                    else {
                        continue; // dirección mal formada: se descarta
                    };
                    // Sobre firmado con nuestra identidad Ed25519: el nodo `--serve`
                    // lo rechaza si nuestra clave pública no está autorizada.
                    let envelope = wire::wrap_signed(
                        &self.identity,
                        &wire::inner("@caller", actor, &req.cap, &req.payload),
                    );
                    (
                        req.corr,
                        serde_json::json!({
                            "corr": req.corr,
                            "method": "POST",
                            "url": format!("http://{hostport}/"),
                            // `text/plain` = petición simple: evita el preflight CORS.
                            "headers": { "content-type": "text/plain" },
                            "body_b64": crate::crypto::base64_encode(envelope.as_bytes()),
                        }),
                    )
                }
                Outbound::Sock(_) => continue,
            };
            arr.push(desc);
            self.net_pending.insert(corr, item);
        }
        serde_json::Value::Array(arr).to_string()
    }

    /// El host JS entrega el resultado de un `fetch` (identificado por `corr`),
    /// tanto de un `NET_FETCH` como de un `SEND` a un actor remoto (ambos viajan
    /// como POST). Según el trabajo pendiente construye el record `net_response`
    /// (HTTP, vía `build_net_record`) o **desenvuelve el payload** que respondió el
    /// actor remoto. Lo reinyecta como `on_message`, drena y devuelve
    /// `{trace, state}`. Un fallo se señala pasando `error` (con `status` 0).
    pub fn deliver(
        &mut self,
        corr: u32,
        status: u16,
        headers_json: &str,
        body: &[u8],
        error: Option<String>,
    ) -> Result<String, JsValue> {
        let item = self
            .net_pending
            .remove(&(corr as u64))
            .ok_or_else(|| JsValue::from_str(&format!("deliver: corr desconocido {corr}")))?;

        let (to, payload) = match item {
            Outbound::Http(req) => {
                // Headers como objeto JSON `{nombre: valor}`.
                let headers = serde_json::from_str::<serde_json::Value>(headers_json)
                    .ok()
                    .and_then(|v| {
                        v.as_object().map(|o| {
                            o.iter()
                                .map(|(k, v)| {
                                    (k.clone(), v.as_str().unwrap_or_default().to_string())
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .unwrap_or_default();
                let to = req.from.clone();
                let resp = NetResponse { status, headers, body: body.to_vec(), error };
                (to, build_net_record(req, resp))
            }
            Outbound::Remote(req) => {
                let to = req.from.clone();
                // Éxito HTTP: se **verifica la firma** del sobre de respuesta y se
                // desenvuelve su `payload`. Si no, un record `remote_error`.
                let payload = if error.is_none() && (200..300).contains(&status) {
                    match wire::unwrap_verified(body, None) {
                        Ok(inner) => wire::payload_of(&inner).unwrap_or_else(|| {
                            remote_error_record(&req, Some("respuesta remota ilegible".into()))
                        }),
                        Err(e) => {
                            remote_error_record(&req, Some(format!("respuesta no autenticada: {e}")))
                        }
                    }
                } else {
                    remote_error_record(&req, error.or_else(|| Some(format!("HTTP {status}"))))
                };
                (to, payload)
            }
            Outbound::Sock(_) => {
                return Err(JsValue::from_str(
                    "deliver: sock:// no está soportado en el navegador",
                ))
            }
        };

        let mut trace = Vec::new();
        let mut queue = VecDeque::new();
        self.counter += 1;
        trace.push(format!(
            "route #{:03} @net -> {} :: {}",
            self.counter, to, payload
        ));
        queue.push_back(Message {
            from: "@net".to_string(),
            to,
            payload,
            cap: None,
        });
        self.drain(&mut queue, &mut trace);

        Ok(serde_json::json!({
            "trace": trace,
            "state": self.state_value(),
        })
        .to_string())
    }
}

// --- Interior (no expuesto a JS) ------------------------------------------

impl Engine {
    fn boot(&mut self, trace: &mut Vec<String>) {
        let mut queue = VecDeque::new();
        for name in self.order.clone() {
            if !self.actors[&name].def.on_start.is_empty() {
                // Igual que el host nativo: un fallo en on_start se registra (y el
                // actor queda con memoria limpia) en vez de perderse en silencio.
                if let Err(e) = self.run_handler(&name, None, &mut queue, trace) {
                    self.actors.get_mut(&name).unwrap().memory.clear();
                    trace.push(format!("FALLO AISLADO en on_start de '{name}': {e}"));
                }
            }
        }
        self.drain(&mut queue, trace);
    }

    fn drain(&mut self, queue: &mut VecDeque<Message>, trace: &mut Vec<String>) {
        while let Some(msg) = queue.pop_front() {
            let to = msg.to.clone();
            if !self.actors.contains_key(&to) {
                trace.push(format!("descartado: actor inexistente '{to}'"));
                continue;
            }
            if let Err(e) = self.run_handler(&to, Some(msg), queue, trace) {
                // Aislamiento de fallos: se reinicia el actor y el sistema sigue.
                self.actors.get_mut(&to).unwrap().memory.clear();
                trace.push(format!("FALLO AISLADO en '{to}': {e}"));
            }
        }
    }

    fn run_handler(
        &mut self,
        name: &str,
        incoming: Option<Message>,
        queue: &mut VecDeque<Message>,
        trace: &mut Vec<String>,
    ) -> Result<(), RuntimeError> {
        let actor = self.actors.get_mut(name).unwrap();
        let capabilities = actor.def.capabilities.clone();
        let (body, bind) = match &incoming {
            Some(_) => (
                actor.def.on_message.body.clone(),
                actor.def.on_message.bind.clone(),
            ),
            None => (actor.def.on_start.clone(), None),
        };

        if let Some(msg) = &incoming {
            if let Some(schema) = &actor.def.on_message.expects {
                value::check_schema(&msg.payload, schema)?;
            }
            if let Some(rt) = &actor.def.on_message.reply_to {
                actor
                    .memory
                    .insert(rt.clone(), Value::Str(msg.from.clone()));
            }
        }
        if let (Some(var), Some(msg)) = (bind, &incoming) {
            actor.memory.insert(var, msg.payload.clone());
        }

        let mut ctx = ExecCtx {
            actor: name,
            capabilities: &capabilities,
            memory: &mut actor.memory,
            outbox: queue,
            // Los `NET_FETCH` se encolan aquí; el host JS los recoge con
            // `take_outbound()`, hace `fetch` y responde con `deliver()`.
            outbound: &mut self.net_outbox,
            net_allow: &self.net_allow,
            net_seq: &mut self.net_seq,
            known_actors: &self.order,
            procedures: &actor.def.procedures,
            // El navegador no tiene disco: la capacidad `FILE` opera sobre el VFS
            // en memoria, que el host espeja a IndexedDB.
            fs: FsBackend::Memory {
                files: &mut self.vfs,
                dirty: &mut self.vfs_dirty,
            },
            crypto_key: &self.crypto_key,
            depth: 0,
            trace,
            route_counter: &mut self.counter,
        };
        exec_block(&body, &mut ctx)
    }

    fn state_value(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        for name in &self.order {
            let a = &self.actors[name];
            let mut mem = serde_json::Map::new();
            for (k, v) in &a.memory {
                mem.insert(k.clone(), value_to_json(v));
            }
            obj.insert(
                name.clone(),
                serde_json::json!({
                    "capabilities": a.def.capabilities,
                    "memory": serde_json::Value::Object(mem),
                }),
            );
        }
        serde_json::Value::Object(obj)
    }
}

fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Int(n) => J::from(*n),
        Value::Float(f) => J::from(*f),
        Value::Str(s) => J::from(s.clone()),
        Value::Bool(b) => J::from(*b),
        Value::Date(d) => J::from(d.to_string()),
        Value::Record(m) => {
            J::Object(m.iter().map(|(k, v)| (k.clone(), value_to_json(v))).collect())
        }
        Value::List(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Bytes(b) => J::from(format!("<bytes:{}>", b.len())),
        Value::Null => J::Null,
    }
}

/// Valida un programa StardustLang sin ejecutarlo (ver [`crate::check`]): devuelve
/// el informe `{ok, errors:[{path,message,hint}], warnings:[…]}` como JSON. Es lo
/// que usa un generador automático (LLM) para corregirse antes de cargar el programa.
#[wasm_bindgen]
pub fn check_program(src: &str) -> String {
    serde_json::to_string(&crate::check::check(src)).unwrap_or_else(|_| "{\"ok\":false}".into())
}

/// Compila texto `.stardust` y lo valida (ver [`crate::lang`]): devuelve
/// `{ok, errors:[{line,col,message,hint}], warnings:[…], ir}` como JSON. Con `ok`,
/// `ir` es el programa listo para `new Engine(JSON.stringify(ir))`.
#[wasm_bindgen]
pub fn compile_program(src: &str) -> String {
    serde_json::to_string(&crate::lang::check_source(src)).unwrap_or_else(|_| "{\"ok\":false}".into())
}

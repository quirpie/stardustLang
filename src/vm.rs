//! StardustVM: planificador por actores, enrutador de mensajes asíncrono y
//! aislamiento de fallos.
//!
//! Modelo de ejecución (Nano POC):
//!   * Cada actor tiene memoria privada y un buzón lógico.
//!   * El `SEND` no invoca al destino: **encola** un mensaje (asincronía).
//!   * El planificador drena la cola en orden; cada manejador se ejecuta bajo
//!     `catch_unwind`. Si un actor entra en pánico o devuelve un error de
//!     ejecución, la VM lo aísla, lo reinicia (memoria fresca) y **continúa**
//!     con el resto del sistema — la UI no colapsa por un fallo del cálculo.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::net::ToSocketAddrs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::RuntimeError;
use crate::interpreter::{
    build_net_record, build_sock_record, exec_block, remote_error_record, ExecCtx, FsBackend,
    NetRequest, Outbound, RemoteSend, SockRequest,
};
// Re-exportado para ergonomía: `NetTransport` (aquí) devuelve `NetResponse`.
pub use crate::interpreter::NetResponse;
use crate::message::Message;
use crate::program::{Actor, Program};
use crate::value::{self, Value};
use crate::wire;

/// Estado vivo de un actor dentro de la VM.
struct ActorRuntime {
    def: Actor,
    memory: HashMap<String, Value>,
    restarts: u32,
}

pub struct Vm {
    actors: HashMap<String, ActorRuntime>,
    order: Vec<String>,
    program_name: String,
    verbose: bool,
    /// Política de frontera por app: actores exportados.
    exports: HashMap<String, HashSet<String>>,
    /// Política de frontera por app: token de capacidad -> actores que desbloquea.
    grants: HashMap<String, BTreeMap<String, Vec<String>>>,
    /// Capacidades de respuesta transitorias: (app_que_responde, dirección_permitida).
    reply_grants: HashSet<(String, String)>,
    /// Raíz del sandbox de ficheros (para actores con capacidad `FILE`).
    sandbox: Option<PathBuf>,
    /// Clave del host para `SIGN`/`VERIFY` (capacidad `CRYPTO`, a nivel de actor).
    crypto_key: Vec<u8>,
    /// Identidad Ed25519 del nodo (semilla privada) para firmar los mensajes a
    /// otros nodos (`actor://`). Separada de `crypto_key`: es la clave del *cable*.
    identity: [u8; 32],
    /// Claves públicas autorizadas a enviarnos mensajes (modo `--serve`), estilo
    /// `authorized_keys`. Un mensaje firmado por una clave fuera de esta lista se
    /// rechaza. Por defecto: solo la clave pública de la identidad de demostración.
    authorized: Vec<[u8; 32]>,
    /// Allowlist de red por actor (dirección -> prefijos de URL alcanzables).
    /// Se resuelve por app: cada actor hereda la `net_allow` de su programa, de
    /// modo que en multi-app cada app tiene su propia frontera de red.
    net_allow: HashMap<String, Vec<String>>,
    /// Transporte de red del host. Por defecto un cliente HTTP real; los tests
    /// inyectan uno determinista con [`Vm::with_net_transport`]. `Send` para poder
    /// compartir la VM entre hilos en el modo `--serve` (una conexión por hilo).
    net_transport: Box<dyn NetTransport + Send>,
    /// Config del transporte HTTP por defecto (timeout y tope de cuerpo). Se guarda
    /// para reconstruirlo cuando cambian la clave o los límites, sin depender del
    /// orden de los builders.
    net_timeout: Duration,
    net_max_body: usize,
    /// `true` si el usuario inyectó un transporte propio: entonces `with_key` /
    /// `with_net_limits` no lo sobrescriben.
    custom_transport: bool,
    /// Respuesta capturada para el llamante remoto (modo `--serve`): un `SEND` a
    /// "@caller" durante el manejo de un mensaje entrante se guarda aquí para
    /// devolverlo por el socket.
    rpc_reply: Option<Value>,
}

impl Vm {
    /// Fija la raíz del sandbox de ficheros para los actores con capacidad `FILE`.
    pub fn with_sandbox(mut self, sandbox: Option<PathBuf>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Fija la clave del host para las ops `SIGN`/`VERIFY` de actor (capacidad
    /// `CRYPTO`). No tiene que ver con la firma del cable entre nodos, que usa la
    /// identidad Ed25519 ([`Vm::with_identity`]).
    pub fn with_key(mut self, key: Vec<u8>) -> Self {
        if !key.is_empty() {
            self.crypto_key = key;
        }
        self
    }

    /// Fija la **identidad Ed25519** del nodo (semilla privada de 32 bytes) con la
    /// que firma los mensajes que envía a otros nodos.
    pub fn with_identity(mut self, seed: [u8; 32]) -> Self {
        self.identity = seed;
        self.rebuild_transport();
        self
    }

    /// Fija las **claves públicas autorizadas** a enviarnos mensajes (modo
    /// `--serve`). Reemplaza la autorización por defecto (la clave de demo).
    pub fn with_authorized(mut self, keys: Vec<[u8; 32]>) -> Self {
        self.authorized = keys;
        self
    }

    /// Reemplaza el transporte de red del host (capacidad `NET`). Útil en tests
    /// para inyectar respuestas deterministas sin salir a la red.
    pub fn with_net_transport(mut self, transport: Box<dyn NetTransport + Send>) -> Self {
        self.net_transport = transport;
        self.custom_transport = true;
        self
    }

    /// Ajusta los límites del cliente HTTP por defecto: `timeout` global por
    /// petición y tamaño máximo del cuerpo de respuesta (red de seguridad).
    pub fn with_net_limits(mut self, timeout: Duration, max_body: usize) -> Self {
        self.net_timeout = timeout;
        self.net_max_body = max_body;
        self.rebuild_transport();
        self
    }

    /// Carga una o varias apps en un mismo host. Con una sola app los nombres
    /// quedan planos (sin frontera). Con varias, cada actor pasa a direccionarse
    /// `app/actor`; las llamadas entre apps cruzan la frontera con capacidades.
    pub fn load_apps(programs: Vec<Program>, verbose: bool) -> Self {
        let multi = programs.len() > 1;
        let mut actors = HashMap::new();
        let mut order = Vec::new();
        let mut exports = HashMap::new();
        let mut grants = HashMap::new();
        let mut names = Vec::new();
        let mut net_allow: HashMap<String, Vec<String>> = HashMap::new();

        for program in programs {
            let app = program.program.clone();
            let app_net_allow = program.net_allow.clone();
            // En modo mono-app los nombres quedan "planos" (compatibilidad total).
            let addr_of = |name: &str| {
                if multi {
                    format!("{app}/{name}")
                } else {
                    name.to_string()
                }
            };
            exports.insert(app.clone(), program.exports.iter().cloned().collect());
            grants.insert(app.clone(), program.grants.clone());
            for a in program.actors {
                let addr = addr_of(&a.name);
                // Cada actor hereda la allowlist de red de su app.
                net_allow.insert(addr.clone(), app_net_allow.clone());
                order.push(addr.clone());
                actors.insert(
                    addr,
                    ActorRuntime {
                        def: a,
                        memory: HashMap::new(),
                        restarts: 0,
                    },
                );
            }
            names.push(app);
        }

        Vm {
            actors,
            order,
            program_name: names.join(" + "),
            verbose,
            exports,
            grants,
            reply_grants: HashSet::new(),
            sandbox: None,
            crypto_key: b"stardust-dev-secret-key".to_vec(),
            identity: wire::DEMO_SEED,
            authorized: vec![wire::public_key(&wire::DEMO_SEED)],
            net_allow,
            net_transport: Box::new(HttpTransport::new(
                Duration::from_secs(30),
                16 * 1024 * 1024,
                wire::DEMO_SEED,
            )),
            net_timeout: Duration::from_secs(30),
            net_max_body: 16 * 1024 * 1024,
            custom_transport: false,
            rpc_reply: None,
        }
    }

    /// Reconstruye el transporte HTTP por defecto con la identidad y límites
    /// actuales (a menos que el usuario haya inyectado uno propio).
    fn rebuild_transport(&mut self) {
        if !self.custom_transport {
            self.net_transport = Box::new(HttpTransport::new(
                self.net_timeout,
                self.net_max_body,
                self.identity,
            ));
        }
    }

    /// Ejecuta el programa hasta vaciar la cola de mensajes.
    /// Devuelve el número de fallos aislados (0 = ejecución limpia).
    pub fn run(&mut self) -> u32 {
        let mut queue: VecDeque<Message> = VecDeque::new();
        let mut trace: Vec<String> = Vec::new();
        let mut counter: usize = 0;
        let mut printed: usize = 0;
        let mut faults: u32 = 0;
        // Buzón de salida de red y su secuencia de correlación. Se clona la
        // allowlist a un local para no chocar con los préstamos de `&mut self`.
        let mut net_outbox: Vec<Outbound> = Vec::new();
        let mut net_seq: u64 = 0;
        let net_allow = self.net_allow.clone();

        self.log(format!(
            "StardustVM :: cargando programa '{}' con {} actor(es): [{}]",
            self.program_name,
            self.order.len(),
            self.order.join(", ")
        ));

        // --- Boot: on_start de TODOS los actores (hook de inicialización del
        //     modelo de actores), en orden de declaración. Los mensajes que se
        //     encolen no se procesan hasta que todo `on_start` haya corrido. ---
        for name in self.order.clone() {
            if self.actors[&name].def.on_start.is_empty() {
                continue;
            }
            self.log(format!("boot :: ejecutando on_start de '{name}'"));
            let boot_result = run_handler(
                self.actors.get_mut(&name).unwrap(),
                &name,
                None,
                &self.order,
                self.sandbox.as_deref(),
                &self.crypto_key,
                &mut queue,
                &mut net_outbox,
                &net_allow,
                &mut net_seq,
                &mut trace,
                &mut counter,
            );
            printed = self.flush_trace(&trace, printed);
            if let Err(e) = boot_result {
                faults += 1;
                self.actors.get_mut(&name).unwrap().memory.clear();
                self.log(format!("FALLO AISLADO en on_start de '{name}': {e}"));
            }
        }

        // --- Planificador: drena los mensajes iniciales ---
        self.drain(
            &mut queue,
            &mut trace,
            &mut counter,
            &mut printed,
            &mut faults,
            &mut net_outbox,
            &net_allow,
            &mut net_seq,
        );

        // --- UI (opcional): si algún actor tiene vista + capacidad RENDER,
        //     entramos en su bucle interactivo MVU. ---
        let ui_actor = self.order.iter().find(|n| {
            let a = &self.actors[*n];
            a.def.view.is_some() && a.def.capabilities.iter().any(|c| c == "RENDER")
        });
        if let Some(name) = ui_actor.cloned() {
            self.ui_loop(
                name,
                &mut queue,
                &mut trace,
                &mut counter,
                &mut printed,
                &mut faults,
                &mut net_outbox,
                &net_allow,
                &mut net_seq,
            );
        }

        self.log(format!(
            "StardustVM :: fin. mensajes enrutados: {counter}, fallos aislados: {faults}"
        ));
        faults
    }

    /// Modo nodo: ejecuta los `on_start` y luego **escucha en HTTP** mensajes de
    /// actores remotos, **un hilo por conexión**. Cada petición trae un sobre
    /// firmado (HMAC con la clave del host): se verifica —y se rechaza si no
    /// cuadra—, se inyecta al actor destino (remitente `@caller`), y la respuesta,
    /// también firmada, vuelve por la misma conexión. El procesamiento va bajo un
    /// `Mutex` (modelo de actores: un mensaje a la vez, memoria consistente), pero
    /// el I/O de red corre en paralelo, así un cliente lento o colgado no bloquea
    /// al resto. Consume la VM y bloquea hasta Ctrl-C.
    pub fn serve(mut self, addr: &str) -> std::io::Result<()> {
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        // Boot: on_start de todos los actores (como en `run`), sin bucle de UI.
        {
            let mut trace: Vec<String> = Vec::new();
            let mut counter = 0usize;
            let mut printed = 0usize;
            let mut faults = 0u32;
            let mut net_outbox: Vec<Outbound> = Vec::new();
            let mut net_seq: u64 = 0;
            let net_allow = self.net_allow.clone();
            let mut queue: VecDeque<Message> = VecDeque::new();
            for name in self.order.clone() {
                if self.actors[&name].def.on_start.is_empty() {
                    continue;
                }
                let _ = run_handler(
                    self.actors.get_mut(&name).unwrap(),
                    &name,
                    None,
                    &self.order,
                    self.sandbox.as_deref(),
                    &self.crypto_key,
                    &mut queue,
                    &mut net_outbox,
                    &net_allow,
                    &mut net_seq,
                    &mut trace,
                    &mut counter,
                );
            }
            self.drain(
                &mut queue, &mut trace, &mut counter, &mut printed, &mut faults, &mut net_outbox,
                &net_allow, &mut net_seq,
            );
            self.flush_trace(&trace, printed);
        }

        let identity = self.identity;
        let authorized = self.authorized.clone();
        let verbose = self.verbose;
        let listener = TcpListener::bind(addr)?;
        if verbose {
            println!(
                "[VM] SERVE :: '{}' escuchando en {addr} (firma Ed25519, {} clave(s) autorizada(s), un hilo por conexión)",
                self.program_name,
                authorized.len()
            );
        }
        // La VM se comparte entre hilos tras un `Mutex`: el I/O es concurrente, el
        // procesamiento de mensajes serializado.
        let vm = Arc::new(Mutex::new(self));
        for conn in listener.incoming() {
            let stream = match conn {
                Ok(s) => s,
                Err(_) => continue,
            };
            let vm = Arc::clone(&vm);
            let authorized = authorized.clone();
            std::thread::spawn(move || handle_connection(stream, vm, identity, authorized, verbose));
        }
        Ok(())
    }

    /// Procesa un mensaje entrante (modo `--serve`): lo inyecta con remitente
    /// `@caller`, drena, y devuelve lo que el actor destino respondió (o `Null`).
    fn process_incoming(&mut self, to: String, cap: Option<String>, payload: Value) -> Value {
        let mut trace: Vec<String> = Vec::new();
        let mut counter = 0usize;
        let mut printed = 0usize;
        let mut faults = 0u32;
        let mut net_outbox: Vec<Outbound> = Vec::new();
        let mut net_seq: u64 = 0;
        let net_allow = self.net_allow.clone();
        if self.verbose {
            println!("[VM] SERVE :: entrante -> {to} :: {payload}");
        }
        self.rpc_reply = None;
        let mut q: VecDeque<Message> = VecDeque::new();
        q.push_back(Message {
            from: "@caller".to_string(),
            to,
            payload,
            cap,
        });
        self.drain(
            &mut q, &mut trace, &mut counter, &mut printed, &mut faults, &mut net_outbox,
            &net_allow, &mut net_seq,
        );
        self.flush_trace(&trace, printed);
        self.rpc_reply.take().unwrap_or(Value::Null)
    }

    #[allow(clippy::too_many_arguments)]
    fn drain(
        &mut self,
        queue: &mut VecDeque<Message>,
        trace: &mut Vec<String>,
        counter: &mut usize,
        printed: &mut usize,
        faults: &mut u32,
        net_outbox: &mut Vec<Outbound>,
        net_allow: &HashMap<String, Vec<String>>,
        net_seq: &mut u64,
    ) {
        // Se alternan dos fases: (1) drenar la cola de mensajes hasta vaciarla;
        // (2) si quedó trabajo de red encolado, ejecutarlo (bloqueante, **entre**
        // ciclos, no dentro de un manejador) y reinyectar cada respuesta como un
        // `on_message`. Se repite hasta que ambas colas queden vacías.
        loop {
            while let Some(msg) = queue.pop_front() {
                let to = msg.to.clone();
                // Captura de la respuesta remota (modo `--serve`): un `SEND` a
                // "@caller" no se enruta localmente; es la respuesta a devolver por
                // el socket del llamante.
                if to == "@caller" {
                    self.rpc_reply = Some(msg.payload.clone());
                    continue;
                }
                if !self.actors.contains_key(&to) {
                    self.log(format!("descartado: mensaje a actor inexistente '{to}'"));
                    continue;
                }
                let from = msg.from.clone();
                let payload = msg.payload.clone();

                // --- Frontera entre apps: autorización por capacidades ---
                if !self.authorize_hop(&from, &to, &msg.cap) {
                    continue; // denegado y registrado; el mensaje se descarta
                }

                let result = run_handler(
                    self.actors.get_mut(&to).unwrap(),
                    &to,
                    Some(msg),
                    &self.order,
                    self.sandbox.as_deref(),
                    &self.crypto_key,
                    queue,
                    net_outbox,
                    net_allow,
                    net_seq,
                    trace,
                    counter,
                );
                *printed = self.flush_trace(trace, *printed);
                if let Err(e) = result {
                    *faults += 1;
                    let restarts = {
                        let a = self.actors.get_mut(&to).unwrap();
                        a.memory.clear(); // reinicio: memoria fresca
                        a.restarts += 1;
                        a.restarts
                    };
                    self.log(format!(
                        "FALLO AISLADO en '{to}' (mensaje de '{from}', payload {payload}): {e}"
                    ));
                    self.log(format!(
                        "  -> '{to}' reiniciado (reinicio #{restarts}); el resto del sistema sigue vivo"
                    ));
                }
            }

            if net_outbox.is_empty() {
                break;
            }
            // Frontera asíncrona del host: se resuelve el trabajo de red pendiente
            // (HTTP, actor remoto o socket) y cada respuesta vuelve al sistema como
            // un mensaje normal (de `@net`). El intérprete nunca se bloqueó.
            for item in std::mem::take(net_outbox) {
                let (to, payload) = match item {
                    Outbound::Http(req) => {
                        let to = req.from.clone();
                        self.log(format!("NET :: {} {} (corr {})", req.method, req.url, req.corr));
                        let resp = self.net_transport.fetch(&req);
                        (to, build_net_record(req, resp))
                    }
                    Outbound::Remote(req) => {
                        let to = req.from.clone();
                        self.log(format!("NET :: actor {} (corr {})", req.addr, req.corr));
                        let reply = self.net_transport.remote(&req);
                        // La respuesta del actor remoto se entrega tal cual; un
                        // error de transporte llega como record `remote_error`.
                        let payload = match (reply.payload, reply.error) {
                            (Some(p), _) => p,
                            (None, err) => remote_error_record(&req, err),
                        };
                        (to, payload)
                    }
                    Outbound::Sock(req) => {
                        let to = req.from.clone();
                        self.log(format!("NET :: sock {} (corr {})", req.addr, req.corr));
                        let resp = self.net_transport.sock(&req);
                        (to, build_sock_record(req, resp.body, resp.error))
                    }
                };
                *counter += 1;
                trace.push(format!("route #{:03} @net -> {} :: {}", counter, to, payload));
                *printed = self.flush_trace(trace, *printed);
                queue.push_back(Message {
                    from: "@net".to_string(),
                    to,
                    payload,
                    cap: None,
                });
            }
        }
    }

    /// Decide si un mensaje puede cruzar (o no) la frontera de una app.
    /// Local o intra-app: siempre permitido. Cross-app: exige que el actor
    /// destino esté exportado y que el token de capacidad lo desbloquee, o que
    /// exista una capacidad de respuesta transitoria concedida por una llamada
    /// previa. Al autorizar una llamada, concede a la app destino permiso para
    /// responder al llamante.
    fn authorize_hop(&mut self, from: &str, to: &str, cap: &Option<String>) -> bool {
        let (from_app, to_app) = match (from.split_once('/'), to.split_once('/')) {
            (Some((fa, _)), Some((ta, _))) => (fa, ta),
            _ => return true, // mono-app: sin frontera
        };
        if from_app == to_app {
            return true; // mismo app: confianza interna
        }
        let to_short = to.split_once('/').map(|(_, s)| s).unwrap_or(to);

        let exported = self
            .exports
            .get(to_app)
            .is_some_and(|s| s.contains(to_short));
        let cap_ok = cap.as_ref().is_some_and(|tok| {
            self.grants
                .get(to_app)
                .and_then(|g| g.get(tok))
                .is_some_and(|acts| acts.iter().any(|a| a == to_short))
        });
        let reply_ok = self
            .reply_grants
            .contains(&(from_app.to_string(), to.to_string()));

        if exported && cap_ok {
            // Llamada autorizada: habilita la respuesta correlacionada.
            self.reply_grants
                .insert((to_app.to_string(), from.to_string()));
            self.log(format!(
                "BROKER :: autorizado {from} -> {to} (cap ok); respuesta habilitada"
            ));
            true
        } else if reply_ok {
            true
        } else {
            let reason = if !exported {
                "actor no exportado"
            } else {
                "capacidad ausente o inválida"
            };
            self.log(format!("BROKER :: DENEGADO {from} -> {to} ({reason})"));
            false
        }
    }

    /// Bucle interactivo MVU sobre el `Actor_UI`:
    ///   1. renderiza la vista (función del estado del actor)
    ///   2. lee la etiqueta del botón que pulsa el usuario
    ///   3. ese evento entra como **mensaje** al actor (queda en la traza)
    ///   4. `on_message` actualiza el estado -> se re-renderiza
    /// Termina con 'q' o EOF.
    #[allow(clippy::too_many_arguments)]
    fn ui_loop(
        &mut self,
        name: String,
        queue: &mut VecDeque<Message>,
        trace: &mut Vec<String>,
        counter: &mut usize,
        printed: &mut usize,
        faults: &mut u32,
        net_outbox: &mut Vec<Outbound>,
        net_allow: &HashMap<String, Vec<String>>,
        net_seq: &mut u64,
    ) {
        use std::io::{self, BufRead, Write};

        self.log(format!(
            "UI :: bucle interactivo de '{name}' (teclea una etiqueta de botón, o una \
             secuencia como '12+3='; 'q' para salir)"
        ));
        let stdin = io::stdin();

        loop {
            // 1. Resolver la vista contra el estado (núcleo común) y pintarla.
            let rendered = {
                let actor = &self.actors[&name];
                let node = crate::render::build(actor.def.view.as_ref().unwrap(), &actor.memory);
                crate::ui::paint(&node)
            };
            println!();
            for line in &rendered.lines {
                println!("{line}");
            }
            print!("> ");
            io::stdout().flush().ok();

            // 2. Leer la elección del usuario.
            let mut input = String::new();
            if stdin.lock().read_line(&mut input).unwrap_or(0) == 0 {
                break; // EOF
            }
            let choice = input.trim();
            if choice.eq_ignore_ascii_case("q") {
                break;
            }
            if choice.is_empty() {
                continue; // solo Enter: re-render
            }

            // 2b. Resolver la entrada a una secuencia de activaciones:
            //   1) coincidencia exacta con una etiqueta (permite etiquetas
            //      multi-carácter: "Fibonacci", "+1", "=");
            //   2) si no, cada carácter como un botón de un solo carácter, para
            //      poder teclear un número o una expresión de golpe ("12+3=").
            let resolve =
                |label: &str| rendered.actions.iter().find(|a| a.label == label).cloned();
            let mut presses: Vec<crate::ui::Activatable> = Vec::new();
            if let Some(a) = resolve(choice) {
                presses.push(a);
            } else if let Some(seq) = choice
                .chars()
                .map(|c| resolve(&c.to_string()))
                .collect::<Option<Vec<_>>>()
            {
                presses = seq;
            } else {
                self.log(format!("(entrada no reconocida como botón ni secuencia: '{choice}')"));
                continue;
            }

            // 3+4. Cada activación entra como mensaje y se procesa; el estado se
            //      actualiza entre activaciones y se re-renderiza al siguiente ciclo.
            for action in presses {
                // Los campos `Input` piden su valor por teclado (rellena `$input`).
                let mut host: HashMap<String, Value> = HashMap::new();
                if action.prompts {
                    print!("{}: ", action.label);
                    io::stdout().flush().ok();
                    let mut val = String::new();
                    if stdin.lock().read_line(&mut val).unwrap_or(0) == 0 {
                        break;
                    }
                    host.insert("input".to_string(), Value::Str(val.trim().to_string()));
                }
                let payload = crate::render::fill_host(&action.event.send, &host);
                let target = action.event.to.clone().unwrap_or_else(|| name.clone());
                *counter += 1;
                trace.push(format!("route #{:03} UI -> {} :: {}", counter, target, payload));
                queue.push_back(Message {
                    from: "UI".to_string(),
                    to: target,
                    payload,
                    cap: None,
                });
                *printed = self.flush_trace(trace, *printed);
                self.drain(
                    queue, trace, counter, printed, faults, net_outbox, net_allow, net_seq,
                );
            }
        }
        self.log("UI :: bucle finalizado".to_string());
    }

    fn flush_trace(&self, trace: &[String], from: usize) -> usize {
        if self.verbose {
            for line in &trace[from..] {
                println!("[VM] {line}");
            }
        }
        trace.len()
    }

    fn log(&self, msg: String) {
        if self.verbose {
            println!("[VM] {msg}");
        }
    }
}

/// Ejecuta un manejador (on_start u on_message) bajo aislamiento de pánico.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn run_handler(
    actor: &mut ActorRuntime,
    name: &str,
    incoming: Option<Message>,
    known_actors: &[String],
    sandbox: Option<&Path>,
    crypto_key: &[u8],
    queue: &mut VecDeque<Message>,
    net_outbox: &mut Vec<Outbound>,
    net_allow: &HashMap<String, Vec<String>>,
    net_seq: &mut u64,
    trace: &mut Vec<String>,
    counter: &mut usize,
) -> Result<(), RuntimeError> {
    // Se clonan las piezas pequeñas para no arrastrar préstamos de `actor.def`
    // durante la mutación de la memoria.
    let capabilities = actor.def.capabilities.clone();
    let (body, bind) = match &incoming {
        Some(_) => (
            actor.def.on_message.body.clone(),
            actor.def.on_message.bind.clone(),
        ),
        None => (actor.def.on_start.clone(), None),
    };

    // Validación del contrato de entrada (esquema) antes de tocar el estado.
    if let Some(msg) = &incoming {
        if let Some(schema) = &actor.def.on_message.expects {
            crate::value::check_schema(&msg.payload, schema)?;
        }
        // Enlaza la dirección del remitente para poder responderle.
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
        outbound: net_outbox,
        // Allowlist del actor en curso (mínimo privilegio por app).
        net_allow: net_allow.get(name).map(Vec::as_slice).unwrap_or(&[]),
        net_seq,
        known_actors,
        procedures: &actor.def.procedures,
        fs: match sandbox {
            Some(root) => FsBackend::Native(root),
            None => FsBackend::None,
        },
        crypto_key,
        depth: 0,
        trace,
        route_counter: counter,
    };

    // Aislamiento: un pánico del actor no propaga; se convierte en error.
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| exec_block(&body, &mut ctx)));
    match outcome {
        Ok(res) => res,
        Err(_) => Err(RuntimeError::Io("pánico del actor capturado".into())),
    }
}

/// Respuesta de un `SEND` a un actor remoto: el payload que devolvió el nodo
/// remoto (si respondió), o un error de transporte. `None` en `payload` con
/// `error` `None` significa "entregado, sin respuesta" (envío unidireccional).
#[derive(Debug, Clone, Default)]
pub struct RemoteReply {
    pub payload: Option<Value>,
    pub error: Option<String>,
}

/// Transporte de red del host: resuelve el trabajo saliente y devuelve su
/// respuesta. El binario usa clientes reales ([`HttpTransport`]: HTTP + TCP); los
/// tests inyectan uno determinista. Nunca debe entrar en pánico: un fallo se
/// reporta en la respuesta, de modo que la red nunca tumba la VM. `remote` y
/// `sock` traen impl por defecto (error) para que un transporte solo-HTTP compile.
pub trait NetTransport {
    /// Petición HTTP (`NET_FETCH`).
    fn fetch(&self, req: &NetRequest) -> NetResponse;
    /// Mensaje a un actor remoto (`SEND` a `actor://`): request/reply sobre TCP.
    fn remote(&self, _req: &RemoteSend) -> RemoteReply {
        RemoteReply {
            payload: None,
            error: Some("transporte sin soporte de actores remotos".into()),
        }
    }
    /// Stream TCP crudo (`SOCK_SEND` a `sock://`): devuelve el cuerpo o un error.
    fn sock(&self, _req: &SockRequest) -> NetResponse {
        NetResponse::transport_error("transporte sin soporte de sockets")
    }
}

/// Cliente HTTP real del host nativo (bloqueante), con timeout, tope de tamaño y
/// la **identidad Ed25519** (semilla privada) para firmar los mensajes a otros
/// nodos (`actor://`) y verificar sus respuestas.
pub struct HttpTransport {
    timeout: Duration,
    max_body: usize,
    identity: [u8; 32],
}

impl HttpTransport {
    pub fn new(timeout: Duration, max_body: usize, identity: [u8; 32]) -> Self {
        HttpTransport { timeout, max_body, identity }
    }
}

impl Default for HttpTransport {
    fn default() -> Self {
        HttpTransport {
            timeout: Duration::from_secs(30),
            max_body: 16 * 1024 * 1024,
            identity: wire::DEMO_SEED,
        }
    }
}

impl NetTransport for HttpTransport {
    fn fetch(&self, req: &NetRequest) -> NetResponse {
        let mut request = ureq::request(&req.method, &req.url).timeout(self.timeout);
        for (k, v) in &req.headers {
            request = request.set(k, v);
        }
        let outcome = if req.body.is_empty() {
            request.call()
        } else {
            request.send_bytes(&req.body)
        };
        let resp = match outcome {
            Ok(resp) => resp,
            // Un status HTTP de error (4xx/5xx) sí trae respuesta.
            Err(ureq::Error::Status(_, resp)) => resp,
            Err(ureq::Error::Transport(t)) => return NetResponse::transport_error(t.to_string()),
        };
        let status = resp.status();
        let headers = resp
            .headers_names()
            .into_iter()
            .filter_map(|n| resp.header(&n).map(|v| (n.clone(), v.to_string())))
            .collect();
        // Cuerpo byte-nativo (imágenes, JSON, texto): crudo, con tope de tamaño.
        let mut body = Vec::new();
        let _ = resp
            .into_reader()
            .take(self.max_body as u64)
            .read_to_end(&mut body);
        NetResponse {
            status,
            headers,
            body,
            error: None,
        }
    }

    fn remote(&self, req: &RemoteSend) -> RemoteReply {
        // `actor://host:port/Actor` -> (host:port, Actor). El transporte es **HTTP**:
        // un POST del mensaje de cable a `http://host:port/`, cuya respuesta es el
        // mensaje de vuelta. Misma forma que en el navegador (`fetch`), de modo que
        // un mismo programa funciona en ambos hosts.
        let rest = &req.addr["actor://".len()..];
        let (hostport, actor) = match rest.split_once('/') {
            Some((hp, a)) if !a.is_empty() => (hp, a),
            _ => {
                return RemoteReply {
                    payload: None,
                    error: Some(format!("dirección de actor mal formada: '{}'", req.addr)),
                }
            }
        };
        // Sobre firmado con nuestra clave privada: el nodo remoto lo rechazará si
        // nuestra clave pública no está en su lista de autorizadas.
        let envelope = wire::wrap_signed(
            &self.identity,
            &wire::inner("@caller", actor, &req.cap, &req.payload),
        );
        let url = format!("http://{hostport}/");
        // `text/plain` evita el preflight CORS en el navegador; aquí es indiferente.
        let outcome = ureq::post(&url)
            .timeout(self.timeout)
            .set("content-type", "text/plain")
            .send_string(&envelope);
        match outcome {
            Ok(resp) => {
                let mut body = Vec::new();
                let _ = resp
                    .into_reader()
                    .take(self.max_body as u64)
                    .read_to_end(&mut body);
                // La respuesta viene firmada: se verifica su integridad (cualquier
                // firma válida; fijar la identidad del servidor sería una extensión).
                match wire::unwrap_verified(&body, None) {
                    Ok(inner) => RemoteReply { payload: wire::payload_of(&inner), error: None },
                    Err(e) => RemoteReply {
                        payload: None,
                        error: Some(format!("respuesta remota no autenticada: {e}")),
                    },
                }
            }
            Err(ureq::Error::Status(code, _)) => RemoteReply {
                payload: None,
                error: Some(format!("el nodo remoto respondió HTTP {code}")),
            },
            Err(ureq::Error::Transport(t)) => RemoteReply {
                payload: None,
                error: Some(t.to_string()),
            },
        }
    }

    fn sock(&self, req: &SockRequest) -> NetResponse {
        let hostport = &req.addr["sock://".len()..];
        match tcp_exchange(hostport, &req.body, self.timeout, self.max_body) {
            Ok(Some(body)) => NetResponse { status: 200, headers: Vec::new(), body, error: None },
            Ok(None) => NetResponse { status: 200, headers: Vec::new(), body: Vec::new(), error: None },
            Err(e) => NetResponse::transport_error(e),
        }
    }
}

/// Atiende una conexión HTTP entrante del modo `--serve`, en su propio hilo: lee
/// la petición, **verifica el sobre firmado** (rechazo si la firma no cuadra),
/// procesa el mensaje bajo el `Mutex` de la VM y responde firmado. Un fallo
/// (firma inválida, cliente lento, socket caído) afecta solo a esta conexión.
fn handle_connection(
    mut stream: std::net::TcpStream,
    vm: std::sync::Arc<std::sync::Mutex<Vm>>,
    identity: [u8; 32],
    authorized: Vec<[u8; 32]>,
    verbose: bool,
) {
    use std::io::{BufRead, BufReader, Write};

    let timeout = Duration::from_secs(30);
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    let mut reader = match stream.try_clone() {
        Ok(s) => BufReader::new(s),
        Err(_) => return,
    };

    // --- Petición HTTP: línea de petición + cabeceras + cuerpo. ---
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let method = request_line
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    let mut content_length = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 {
            break;
        }
        if h == "\r\n" || h == "\n" {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    // Preflight CORS (lo manda el navegador): responder y cerrar.
    if method == "OPTIONS" {
        let _ = stream.write_all(
            b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\n\
              Access-Control-Allow-Methods: POST, OPTIONS\r\n\
              Access-Control-Allow-Headers: content-type\r\nContent-Length: 0\r\n\
              Connection: close\r\n\r\n",
        );
        return;
    }
    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }

    // Verificar el sobre firmado y que la clave esté AUTORIZADA, antes de tocar la
    // VM (autenticación + control de acceso del cable).
    let inner = match wire::unwrap_verified(&body, Some(&authorized)) {
        Ok(i) => i,
        Err(e) => {
            if verbose {
                println!("[VM] SERVE :: RECHAZADO (no autenticado): {e}");
            }
            let _ = stream.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nAccess-Control-Allow-Origin: *\r\n\
                  Content-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return;
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&inner) {
        Ok(v) => v,
        Err(_) => return,
    };
    let to = parsed
        .get("to")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let cap = parsed.get("cap").and_then(|v| v.as_str()).map(String::from);
    let payload = parsed
        .get("payload")
        .map(value::from_tagged)
        .unwrap_or(Value::Null);

    // Procesamiento serializado por el `Mutex` (un mensaje a la vez). Se recupera
    // de un posible envenenamiento del lock (un pánico en otro hilo no debe colgar
    // el servidor; los pánicos de actor ya se capturan dentro del intérprete).
    let reply = {
        let mut guard = vm.lock().unwrap_or_else(|e| e.into_inner());
        guard.process_incoming(to, cap, payload)
    };

    // Respuesta firmada con la identidad del nodo, por la misma conexión.
    let reply_wire = wire::wrap_signed(&identity, &wire::inner("@remote", "@caller", &None, &reply));
    let response = format!(
        "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        reply_wire.len(),
        reply_wire
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Abre una conexión TCP a `hostport`, escribe `body`, cierra la mitad de
/// escritura (EOF para el otro extremo) y lee la respuesta hasta EOF (con timeout
/// y tope de tamaño). Lo usa `SOCK_SEND` (stream TCP crudo request/response).
fn tcp_exchange(
    hostport: &str,
    body: &[u8],
    timeout: Duration,
    max_body: usize,
) -> Result<Option<Vec<u8>>, String> {
    use std::io::Write;
    use std::net::TcpStream;

    let addr = hostport
        .to_socket_addrs()
        .map_err(|e| format!("dirección inválida '{hostport}': {e}"))?
        .next()
        .ok_or_else(|| format!("no se resolvió '{hostport}'"))?;
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("no se conectó: {e}"))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    stream
        .write_all(body)
        .map_err(|e| format!("fallo al escribir: {e}"))?;
    stream.flush().ok();
    stream.shutdown(std::net::Shutdown::Write).ok();
    let mut buf = Vec::new();
    Read::take(stream, max_body as u64)
        .read_to_end(&mut buf)
        .map_err(|e| format!("fallo al leer: {e}"))?;
    if buf.is_empty() {
        Ok(None)
    } else {
        Ok(Some(buf))
    }
}

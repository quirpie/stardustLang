//! Tests de integración de la capacidad `NET` en el host nativo.
//!
//! Inyectan un transporte **mock** (respuestas deterministas, sin salir a la
//! red) para validar de punta a punta el patrón "encolar → resolver entre ciclos
//! → entregar como `on_message`", además del mínimo privilegio (allowlist y
//! capacidad) y el aislamiento de fallos de transporte.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use stardust_vm::interpreter::{NetRequest, RemoteSend, SockRequest};
use stardust_vm::program::Program;
use stardust_vm::value::Value;
use stardust_vm::vm::{NetResponse, NetTransport, RemoteReply, Vm};

/// Transporte determinista: registra cada petición y devuelve una respuesta fija.
struct MockTransport {
    log: Arc<Mutex<Vec<NetRequest>>>,
    response: NetResponse,
}

impl NetTransport for MockTransport {
    fn fetch(&self, req: &NetRequest) -> NetResponse {
        self.log.lock().unwrap().push(req.clone());
        self.response.clone()
    }
}

/// Crea un directorio sandbox único para aislar los ficheros de cada test.
fn temp_sandbox() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut p = std::env::temp_dir();
    p.push(format!("stardust-net-test-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Carga y ejecuta un programa (mono-app, silencioso) con el transporte dado.
fn run(program: &str, sandbox: &Path, transport: Box<dyn NetTransport + Send>) -> u32 {
    let prog: Program = serde_json::from_str(program).expect("programa StardustLang válido");
    let mut vm = Vm::load_apps(vec![prog], false)
        .with_sandbox(Some(sandbox.to_path_buf()))
        .with_net_transport(transport);
    vm.run()
}

fn read(sandbox: &Path, name: &str) -> String {
    std::fs::read_to_string(sandbox.join(name)).unwrap_or_default()
}

const PROG_FETCH: &str = r#"{
  "program": "t",
  "entry": "A",
  "net_allow": ["https://ok.test/"],
  "actors": [{
    "name": "A",
    "capabilities": ["NET", "FILE"],
    "on_start": [
      { "op": "NET_FETCH", "method": "GET", "url": "https://ok.test/data", "tag": "t1" }
    ],
    "on_message": {
      "bind": "resp",
      "body": [
        { "op": "FILE_WRITE", "path": "body.txt", "value": { "from_bytes": { "field": "body", "from": { "var": "resp" } } } },
        { "op": "FILE_WRITE", "path": "status.txt", "value": { "field": "status", "from": { "var": "resp" } } },
        { "op": "FILE_WRITE", "path": "ok.txt", "value": { "field": "ok", "from": { "var": "resp" } } },
        { "op": "FILE_WRITE", "path": "tag.txt", "value": { "field": "tag", "from": { "var": "resp" } } },
        { "op": "FILE_WRITE", "path": "err.txt", "value": { "field": "error", "from": { "var": "resp" } } }
      ]
    }
  }]
}"#;

#[test]
fn fetch_ok_entrega_respuesta_como_mensaje() {
    let sandbox = temp_sandbox();
    let log = Arc::new(Mutex::new(Vec::new()));
    let transport = Box::new(MockTransport {
        log: log.clone(),
        response: NetResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: b"hola mundo".to_vec(),
            error: None,
        },
    });

    let faults = run(PROG_FETCH, &sandbox, transport);

    // La petición llegó al transporte con el método/URL correctos.
    let reqs = log.lock().unwrap();
    assert_eq!(reqs.len(), 1, "debe emitirse exactamente una petición");
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(reqs[0].url, "https://ok.test/data");

    // La respuesta se entregó y el actor la procesó (sin fallos).
    assert_eq!(faults, 0, "el camino feliz no debe producir fallos");
    assert_eq!(read(&sandbox, "body.txt"), "hola mundo");
    assert_eq!(read(&sandbox, "status.txt"), "200");
    assert_eq!(read(&sandbox, "ok.txt"), "true");
    assert_eq!(read(&sandbox, "tag.txt"), "t1", "el tag se devuelve tal cual");
    assert_eq!(read(&sandbox, "err.txt"), "null");
}

#[test]
fn allowlist_bloquea_url_no_permitida() {
    let sandbox = temp_sandbox();
    let log = Arc::new(Mutex::new(Vec::new()));
    // Si el transporte se invocara, sería un bug: la allowlist debe cortar antes.
    let transport = Box::new(MockTransport {
        log: log.clone(),
        response: NetResponse::transport_error("no debería llamarse"),
    });

    let program = r#"{
      "program": "t",
      "entry": "A",
      "net_allow": ["https://ok.test/"],
      "actors": [{
        "name": "A",
        "capabilities": ["NET"],
        "on_start": [ { "op": "NET_FETCH", "url": "https://evil.test/x" } ],
        "on_message": { "bind": "m", "body": [] }
      }]
    }"#;

    let faults = run(program, &sandbox, transport);

    assert_eq!(faults, 1, "una URL fuera de la allowlist es un fallo aislado");
    assert!(
        log.lock().unwrap().is_empty(),
        "el transporte no debe invocarse para una URL bloqueada"
    );
}

#[test]
fn sin_capacidad_net_falla() {
    let sandbox = temp_sandbox();
    let log = Arc::new(Mutex::new(Vec::new()));
    let transport = Box::new(MockTransport {
        log: log.clone(),
        response: NetResponse::transport_error("no debería llamarse"),
    });

    // Actor sin la capacidad NET, aunque la URL esté en la allowlist.
    let program = r#"{
      "program": "t",
      "entry": "A",
      "net_allow": ["https://ok.test/"],
      "actors": [{
        "name": "A",
        "capabilities": [],
        "on_start": [ { "op": "NET_FETCH", "url": "https://ok.test/data" } ],
        "on_message": { "bind": "m", "body": [] }
      }]
    }"#;

    let faults = run(program, &sandbox, transport);

    assert_eq!(faults, 1, "sin capacidad NET, NET_FETCH falla");
    assert!(log.lock().unwrap().is_empty());
}

#[test]
fn allowlist_es_por_app_en_multi_app() {
    // Dos apps hospedadas juntas: `permitida` lista ok.test; `intrusa` no lista
    // nada. La misma URL debe funcionar desde una y bloquearse desde la otra.
    let sandbox = temp_sandbox();
    let log = Arc::new(Mutex::new(Vec::new()));
    let transport = Box::new(MockTransport {
        log: log.clone(),
        response: NetResponse {
            status: 200,
            headers: vec![],
            body: b"ok".to_vec(),
            error: None,
        },
    });

    let permitida: Program = serde_json::from_str(
        r#"{
          "program": "permitida",
          "entry": "A",
          "net_allow": ["https://ok.test/"],
          "actors": [{
            "name": "A",
            "capabilities": ["NET"],
            "on_start": [ { "op": "NET_FETCH", "url": "https://ok.test/data" } ],
            "on_message": { "bind": "m", "body": [] }
          }]
        }"#,
    )
    .unwrap();
    let intrusa: Program = serde_json::from_str(
        r#"{
          "program": "intrusa",
          "entry": "A",
          "actors": [{
            "name": "A",
            "capabilities": ["NET"],
            "on_start": [ { "op": "NET_FETCH", "url": "https://ok.test/data" } ],
            "on_message": { "bind": "m", "body": [] }
          }]
        }"#,
    )
    .unwrap();

    let mut vm = Vm::load_apps(vec![permitida, intrusa], false)
        .with_sandbox(Some(sandbox.clone()))
        .with_net_transport(transport);
    let faults = vm.run();

    // Solo `intrusa/A` falla (URL fuera de SU allowlist); `permitida/A` pasa.
    assert_eq!(faults, 1, "solo la app sin la URL en su allowlist debe fallar");
    let reqs = log.lock().unwrap();
    assert_eq!(reqs.len(), 1, "solo la app permitida alcanza el transporte");
    assert_eq!(reqs[0].from, "permitida/A");
}

// --- Actores remotos (actor://) y sockets (sock://) --------------------------

/// Transporte determinista para las rutas distribuidas: registra las direcciones
/// invocadas y devuelve respuestas fijas.
struct MockDist {
    calls: Arc<Mutex<Vec<String>>>,
    remote_reply: RemoteReply,
    sock: NetResponse,
}

impl NetTransport for MockDist {
    fn fetch(&self, _req: &NetRequest) -> NetResponse {
        NetResponse::transport_error("sin HTTP en este mock")
    }
    fn remote(&self, req: &RemoteSend) -> RemoteReply {
        self.calls.lock().unwrap().push(req.addr.clone());
        self.remote_reply.clone()
    }
    fn sock(&self, req: &SockRequest) -> NetResponse {
        self.calls.lock().unwrap().push(req.addr.clone());
        self.sock.clone()
    }
}

#[test]
fn send_a_actor_remoto_entrega_la_respuesta() {
    let sandbox = temp_sandbox();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let transport = Box::new(MockDist {
        calls: calls.clone(),
        remote_reply: RemoteReply {
            payload: Some(Value::Str("Hola, Stardust".to_string())),
            error: None,
        },
        sock: NetResponse::transport_error("n/a"),
    });

    let program = r#"{
      "program": "t",
      "entry": "Cliente",
      "net_allow": ["actor://nodo:9/"],
      "actors": [{
        "name": "Cliente",
        "capabilities": ["NET", "FILE"],
        "on_start": [
          { "op": "SEND", "to": "actor://nodo:9/Saludador", "value": { "record": { "nombre": "Stardust" } } }
        ],
        "on_message": {
          "bind": "resp",
          "body": [ { "op": "FILE_WRITE", "path": "reply.txt", "value": { "var": "resp" } } ]
        }
      }]
    }"#;

    let faults = run(program, &sandbox, transport);

    assert_eq!(faults, 0, "el camino feliz remoto no debe fallar");
    assert_eq!(calls.lock().unwrap().as_slice(), &["actor://nodo:9/Saludador"]);
    assert_eq!(read(&sandbox, "reply.txt"), "Hola, Stardust");
}

#[test]
fn send_a_actor_remoto_fuera_de_allowlist_falla() {
    let sandbox = temp_sandbox();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let transport = Box::new(MockDist {
        calls: calls.clone(),
        remote_reply: RemoteReply::default(),
        sock: NetResponse::transport_error("n/a"),
    });

    let program = r#"{
      "program": "t",
      "entry": "Cliente",
      "net_allow": ["actor://permitido:9/"],
      "actors": [{
        "name": "Cliente",
        "capabilities": ["NET"],
        "on_start": [ { "op": "SEND", "to": "actor://otro:9/X", "value": 1 } ],
        "on_message": { "bind": "m", "body": [] }
      }]
    }"#;

    let faults = run(program, &sandbox, transport);
    assert_eq!(faults, 1, "un actor:// fuera de la allowlist es fallo aislado");
    assert!(calls.lock().unwrap().is_empty(), "el transporte no debe invocarse");
}

#[test]
fn sock_send_entrega_el_cuerpo() {
    let sandbox = temp_sandbox();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let transport = Box::new(MockDist {
        calls: calls.clone(),
        remote_reply: RemoteReply::default(),
        sock: NetResponse {
            status: 200,
            headers: vec![],
            body: b"PONG".to_vec(),
            error: None,
        },
    });

    let program = r#"{
      "program": "t",
      "entry": "C",
      "net_allow": ["sock://127.0.0.1:9/"],
      "actors": [{
        "name": "C",
        "capabilities": ["NET", "FILE"],
        "on_start": [ { "op": "SOCK_SEND", "addr": "sock://127.0.0.1:9/", "body": "ping" } ],
        "on_message": {
          "bind": "r",
          "body": [
            { "op": "FILE_WRITE", "path": "eco.txt", "value": { "from_bytes": { "field": "body", "from": { "var": "r" } } } },
            { "op": "FILE_WRITE", "path": "ok.txt", "value": { "field": "ok", "from": { "var": "r" } } }
          ]
        }
      }]
    }"#;

    let faults = run(program, &sandbox, transport);
    assert_eq!(faults, 0);
    assert_eq!(calls.lock().unwrap().as_slice(), &["sock://127.0.0.1:9/"]);
    assert_eq!(read(&sandbox, "eco.txt"), "PONG");
    assert_eq!(read(&sandbox, "ok.txt"), "true");
}

#[test]
fn actor_remoto_sin_respuesta_o_error_llega_manejable() {
    let sandbox = temp_sandbox();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let transport = Box::new(MockDist {
        calls: calls.clone(),
        remote_reply: RemoteReply {
            payload: None,
            error: Some("conexión rechazada".into()),
        },
        sock: NetResponse::transport_error("n/a"),
    });

    // El actor lee el record `remote_error` y guarda su campo `error`.
    let program = r#"{
      "program": "t",
      "entry": "Cliente",
      "net_allow": ["actor://nodo:9/"],
      "actors": [{
        "name": "Cliente",
        "capabilities": ["NET", "FILE"],
        "on_start": [ { "op": "SEND", "to": "actor://nodo:9/X", "value": 1 } ],
        "on_message": {
          "bind": "resp",
          "body": [ { "op": "FILE_WRITE", "path": "err.txt", "value": { "field": "error", "from": { "var": "resp" } } } ]
        }
      }]
    }"#;

    let faults = run(program, &sandbox, transport);
    assert_eq!(faults, 0, "un fallo remoto se entrega, no colapsa la VM");
    assert_eq!(read(&sandbox, "err.txt"), "conexión rechazada");
}

#[test]
fn error_de_transporte_llega_como_respuesta_manejable() {
    let sandbox = temp_sandbox();
    let log = Arc::new(Mutex::new(Vec::new()));
    let transport = Box::new(MockTransport {
        log: log.clone(),
        response: NetResponse::transport_error("boom"),
    });

    let faults = run(PROG_FETCH, &sandbox, transport);

    // Un fallo de red NO es un fallo aislado: es un mensaje que el actor maneja.
    assert_eq!(faults, 0, "el error de transporte se entrega, no colapsa la VM");
    assert_eq!(read(&sandbox, "ok.txt"), "false");
    assert_eq!(read(&sandbox, "status.txt"), "0");
    assert_eq!(read(&sandbox, "err.txt"), "boom");
}

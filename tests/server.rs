//! Tests de extremo a extremo de `stardust-server`: levantan el servidor real en un
//! puerto libre, con una base en un directorio temporal, y lo usan por HTTP como lo
//! haría una IA o el CLI.
#![cfg(feature = "server")]

use std::path::PathBuf;

use serde_json::{json, Value as J};
use stardust_vm::server::{self, Config};

const ADMIN: &str = "admin-de-prueba";

struct Srv {
    base: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

fn temp_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let p = std::env::temp_dir().join(format!("stardust-server-test-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn start() -> Srv {
    start_with(PathBuf::from("web"))
}

fn start_with(web: PathBuf) -> Srv {
    let config = Config {
        port: 0,
        data: temp_dir(),
        web,
        public_origin: "http://stardust.test".into(),
        runner_origin: "http://run.test".into(),
        admin_token: ADMIN.into(),
    };
    let state = server::open_state(config).unwrap();
    let (tx_addr, rx_addr) = std::sync::mpsc::channel();
    let (tx_stop, rx_stop) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx_addr.send(listener.local_addr().unwrap()).unwrap();
            server::serve(listener, state, async {
                let _ = rx_stop.await;
            })
            .await
            .unwrap();
        });
    });
    let addr = rx_addr.recv().unwrap();
    Srv { base: format!("http://{addr}"), _shutdown: tx_stop }
}

impl Srv {
    /// Petición con token opcional y cuerpo JSON opcional → (estado, cuerpo).
    fn call(&self, method: &str, path: &str, token: Option<&str>, body: Option<J>) -> (u16, J) {
        let mut req = ureq::request(method, &format!("{}{path}", self.base));
        if let Some(t) = token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let res = match body {
            Some(b) => req.set("Content-Type", "application/json").send_string(&b.to_string()),
            None => req.call(),
        };
        let resp = match res {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{method} {path}: {e}"),
        };
        let status = resp.status();
        let text = resp.into_string().unwrap();
        (status, serde_json::from_str(&text).unwrap_or(J::String(text)))
    }

    /// Petición con cabeceras propias → (estado, Set-Cookie, CSP, cuerpo en texto).
    fn raw(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Option<J>) -> (u16, String, String, String) {
        let mut req = ureq::request(method, &format!("{}{path}", self.base));
        for (k, v) in headers {
            req = req.set(k, v);
        }
        let res = match body {
            Some(b) => req.set("Content-Type", "application/json").send_string(&b.to_string()),
            None => req.call(),
        };
        let resp = match res {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{method} {path}: {e}"),
        };
        let cookie = resp.header("set-cookie").unwrap_or("").to_string();
        let csp = resp.header("content-security-policy").unwrap_or("").to_string();
        (resp.status(), cookie, csp, resp.into_string().unwrap())
    }

    fn new_user(&self, handle: &str) -> String {
        let (st, b) = self.call("POST", "/api/v1/admin/users", Some(ADMIN), Some(json!({ "handle": handle })));
        assert_eq!(st, 201, "{b}");
        b["token"].as_str().unwrap().to_string()
    }
}

fn doble() -> String {
    std::fs::read_to_string("programs/texto/doble.stardust").unwrap()
}

#[test]
fn salud_y_guia() {
    let s = start();
    let (st, b) = s.call("GET", "/healthz", None, None);
    assert_eq!((st, b), (200, J::from("ok")));
    let (st, b) = s.call("GET", "/api/v1", None, None);
    assert_eq!(st, 200);
    assert!(b.as_str().unwrap().contains("http://stardust.test/api/v1/check"));
    let (st, b) = s.call("GET", "/api/v1/nada", None, None);
    assert_eq!((st, b["error"]["code"].clone()), (404, J::from("not_found")));
}

#[test]
fn autenticacion() {
    let s = start();
    let (st, b) = s.call("GET", "/api/v1/me", None, None);
    assert_eq!((st, b["error"]["code"].clone()), (401, J::from("unauthorized")));
    let (st, _) = s.call("GET", "/api/v1/me", Some("sd_falso"), None);
    assert_eq!(st, 401);

    let ana = s.new_user("ana");
    let (st, b) = s.call("GET", "/api/v1/me", Some(&ana), None);
    assert_eq!(st, 200);
    assert_eq!(b["user"]["handle"], "ana");
    assert_eq!(b["token"]["name"], "inicial");

    // Un usuario normal no administra; el handle se valida y no se repite.
    let (st, _) = s.call("GET", "/api/v1/admin/users", Some(&ana), None);
    assert_eq!(st, 403);
    let (st, _) = s.call("POST", "/api/v1/admin/users", Some(ADMIN), Some(json!({ "handle": "Ana!" })));
    assert_eq!(st, 400);
    let (st, b) = s.call("POST", "/api/v1/admin/users", Some(ADMIN), Some(json!({ "handle": "ana" })));
    assert_eq!((st, b["error"]["code"].clone()), (409, J::from("user_exists")));

    // Tokens propios: crear, usar, revocar.
    let (st, b) = s.call("POST", "/api/v1/tokens", Some(&ana), Some(json!({ "name": "claude" })));
    assert_eq!(st, 201);
    let claude = b["token"].as_str().unwrap().to_string();
    let id = b["id"].as_i64().unwrap();
    let (_, b) = s.call("GET", "/api/v1/me", Some(&claude), None);
    assert_eq!(b["token"]["name"], "claude");
    let (_, b) = s.call("GET", "/api/v1/tokens", Some(&ana), None);
    assert_eq!(b["tokens"].as_array().unwrap().len(), 2);
    assert!(!b.to_string().contains(&claude), "nunca se devuelve un token entero");
    let (st, _) = s.call("DELETE", &format!("/api/v1/tokens/{id}"), Some(&ana), None);
    assert_eq!(st, 200);
    let (st, _) = s.call("GET", "/api/v1/me", Some(&claude), None);
    assert_eq!(st, 401);

    // Deshabilitar revoca sus tokens.
    let (st, _) = s.call("POST", "/api/v1/admin/users/ana/disable", Some(ADMIN), None);
    assert_eq!(st, 200);
    let (st, _) = s.call("GET", "/api/v1/me", Some(&ana), None);
    assert_eq!(st, 401);
}

#[test]
fn check_da_errores_por_linea() {
    let s = start();
    let ana = s.new_user("ana");
    let (st, b) = s.call("POST", "/api/v1/check", Some(&ana), Some(json!({ "source": doble() })));
    assert_eq!(st, 200);
    assert_eq!(b["ok"], true);
    assert_eq!(b["ir"]["program"], "doble");
    assert_eq!(b["capabilities"]["actors"]["UI"], json!(["RENDER"]));

    let roto = "app roto\n\nactor A:\n  start:\n    print(x)\n";
    let (st, b) = s.call("POST", "/api/v1/check", Some(&ana), Some(json!({ "source": roto })));
    assert_eq!(st, 200);
    assert_eq!(b["ok"], false);
    assert_eq!(b["errors"][0]["line"], 5, "{b}");

    // JSON: errores con ruta.
    let (_, b) = s.call("POST", "/api/v1/check", Some(&ana), Some(json!({ "source": "{\"program\": 1}" })));
    assert_eq!(b["ok"], false);
    assert!(b["errors"][0]["path"].is_string(), "{b}");

    let (st, b) = s.call("POST", "/api/v1/check", Some(&ana), Some(json!({ "source": "x".repeat(300 * 1024) })));
    assert_eq!((st, b["error"]["code"].clone()), (413, J::from("too_large")));
    let (st, _) = s.call("POST", "/api/v1/check", Some(&ana), Some(json!({ "fuente": "x" })));
    assert_eq!(st, 400, "cuerpo con campos erróneos");
}

#[test]
fn instalar_actualizar_y_volver_atras() {
    let s = start();
    let ana = s.new_user("ana");
    let src = doble();

    // install: 201, versión 1, nombre derivado de `app doble`.
    let (st, b) = s.call("POST", "/api/v1/programs", Some(&ana), Some(json!({ "source": src, "note": "primera" })));
    assert_eq!(st, 201, "{b}");
    assert_eq!(b["program"]["name"], "doble");
    assert_eq!(b["program"]["current_version"], 1);
    assert_eq!(b["program"]["url"], "http://stardust.test/p/ana/doble");
    assert_eq!(b["program"]["version"]["created_by_token"], "inicial");
    assert!(b["program"]["version"].get("source").is_none(), "la respuesta no repite la fuente");

    // Reintento idéntico: idempotente. Con otra fuente: 409.
    let (st, b) = s.call("POST", "/api/v1/programs", Some(&ana), Some(json!({ "source": src })));
    assert_eq!((st, b["unchanged"].clone()), (200, J::Bool(true)));
    let v2 = format!("{src}\n# cambio\n");
    let (st, b) = s.call("POST", "/api/v1/programs", Some(&ana), Some(json!({ "source": v2 })));
    assert_eq!((st, b["error"]["code"].clone(), b["error"]["current_version"].clone()), (409, J::from("program_exists"), J::from(1)));

    // update sin base: 400; con base vieja: 409; con la buena: 201.
    let path = "/api/v1/programs/ana/doble/versions";
    let (st, _) = s.call("POST", path, Some(&ana), Some(json!({ "source": v2 })));
    assert_eq!(st, 400);
    let (st, b) = s.call("POST", path, Some(&ana), Some(json!({ "source": v2, "base_version": 0 })));
    assert_eq!((st, b["error"]["code"].clone()), (409, J::from("version_conflict")));
    let (st, b) = s.call("POST", path, Some(&ana), Some(json!({ "source": v2, "base_version": 1, "note": "comentario" })));
    assert_eq!(st, 201, "{b}");
    assert_eq!(b["program"]["current_version"], 2);
    assert_eq!(b["capabilities_changed"], false);

    // Misma fuente que la actual: unchanged, aunque la base esté vieja.
    let (st, b) = s.call("POST", path, Some(&ana), Some(json!({ "source": v2, "base_version": 1 })));
    assert_eq!((st, b["unchanged"].clone()), (200, J::Bool(true)));

    // Un programa con red: capabilities_changed y aviso de nombre.
    let red = std::fs::read_to_string("programs/texto/red.stardust").unwrap();
    let (st, b) = s.call("POST", path, Some(&ana), Some(json!({ "source": red, "base_version": 2 })));
    assert_eq!(st, 201, "{b}");
    assert_eq!(b["capabilities_changed"], true);
    assert!(b["report"]["warnings"].to_string().contains("demo-red"), "{b}");

    // Programa inválido: 422 con informe, y no crea versión.
    let (st, b) = s.call("POST", path, Some(&ana), Some(json!({ "source": "app x\nactor A:\n  start:\n    print(y)\n", "base_version": 3 })));
    assert_eq!((st, b["error"]["code"].clone()), (422, J::from("invalid_program")));
    assert_eq!(b["report"]["ok"], false);

    // Historial y versiones concretas.
    let (_, b) = s.call("GET", path, Some(&ana), None);
    assert_eq!(b["current_version"], 3);
    let ns: Vec<i64> = b["versions"].as_array().unwrap().iter().map(|v| v["n"].as_i64().unwrap()).collect();
    assert_eq!(ns, vec![3, 2, 1]);
    let (_, b) = s.call("GET", "/api/v1/programs/ana/doble/versions/1", Some(&ana), None);
    assert_eq!(b["source"], src);
    assert_eq!(b["note"], "primera");
    assert_eq!(b["ir"]["program"], "doble");

    // rollback a la 1: crea la 4 con el contenido de la 1.
    let rb = "/api/v1/programs/ana/doble/rollback";
    let (st, _) = s.call("POST", rb, Some(&ana), Some(json!({ "to_version": 1, "base_version": 2 })));
    assert_eq!(st, 409);
    let (st, b) = s.call("POST", rb, Some(&ana), Some(json!({ "to_version": 1, "base_version": 3 })));
    assert_eq!(st, 201, "{b}");
    assert_eq!(b["program"]["current_version"], 4);
    assert_eq!(b["program"]["version"]["note"], "vuelta a la versión 1");
    let (_, b) = s.call("GET", "/api/v1/programs/ana/doble", Some(&ana), None);
    assert_eq!(b["version"]["source"], src);
    let (st, b) = s.call("POST", rb, Some(&ana), Some(json!({ "to_version": 1, "base_version": 4 })));
    assert_eq!((st, b["unchanged"].clone()), (200, J::Bool(true)));

    // force: sin base.
    let (st, b) = s.call("POST", path, Some(&ana), Some(json!({ "source": v2, "force": true })));
    assert_eq!((st, b["program"]["current_version"].clone()), (201, J::from(5)));
}

#[test]
fn json_y_nombres() {
    let s = start();
    let ana = s.new_user("ana");
    let fib = std::fs::read_to_string("programs/fibonacci.json").unwrap();
    let (st, b) = s.call("POST", "/api/v1/programs", Some(&ana), Some(json!({ "source": fib, "name": "fib" })));
    assert_eq!(st, 201, "{b}");
    assert_eq!(b["program"]["version"]["format"], "json");
    let (st, _) = s.call("POST", "/api/v1/programs", Some(&ana), Some(json!({ "source": fib, "name": "Fib/x" })));
    assert_eq!(st, 400);
    let (st, _) = s.call("POST", "/api/v1/programs", Some(&ana), Some(json!({ "source": fib, "visibility": "public" })));
    assert_eq!(st, 400);
}

#[test]
fn visibilidad_y_permisos() {
    let s = start();
    let ana = s.new_user("ana");
    let bea = s.new_user("bea");
    s.call("POST", "/api/v1/programs", Some(&ana), Some(json!({ "source": doble() })));

    // private: para los demás no existe.
    for t in [None, Some(bea.as_str())] {
        let (st, _) = s.call("GET", "/api/v1/programs/ana/doble", t, None);
        assert_eq!(st, 404);
    }
    let (st, _) = s.call("POST", "/api/v1/programs/ana/doble/versions", Some(&bea), Some(json!({ "source": doble(), "force": true })));
    assert_eq!(st, 404);

    // link: cualquiera lee (incluso sin token), solo ana escribe.
    let (st, b) = s.call("PATCH", "/api/v1/programs/ana/doble", Some(&ana), Some(json!({ "visibility": "link" })));
    assert_eq!((st, b["visibility"].clone()), (200, J::from("link")));
    let (st, b) = s.call("GET", "/api/v1/programs/ana/doble?fields=meta", None, None);
    assert_eq!(st, 200);
    assert!(b["version"].get("source").is_none());
    let (st, _) = s.call("GET", "/api/v1/programs/ana/doble/versions/1", Some(&bea), None);
    assert_eq!(st, 200);
    let (st, _) = s.call("POST", "/api/v1/programs/ana/doble/versions", Some(&bea), Some(json!({ "source": doble(), "force": true })));
    assert_eq!(st, 403);
    let (st, _) = s.call("GET", "/api/v1/programs/ana/doble/versions", Some(&bea), None);
    assert_eq!(st, 403, "el historial es del propietario");
    let (st, _) = s.call("DELETE", "/api/v1/programs/ana/doble", Some(&bea), None);
    assert_eq!(st, 403);

    // Listados y borrado.
    let (_, b) = s.call("GET", "/api/v1/programs", Some(&bea), None);
    assert_eq!(b["programs"], json!([]));
    let (_, b) = s.call("GET", "/api/v1/programs", Some(&ana), None);
    assert_eq!(b["programs"][0]["name"], "doble");
    let (st, _) = s.call("DELETE", "/api/v1/programs/ana/doble", Some(&ana), None);
    assert_eq!(st, 204);
    let (st, _) = s.call("GET", "/api/v1/programs/ana/doble", Some(&ana), None);
    assert_eq!(st, 404);
}

#[test]
fn los_datos_sobreviven_a_un_reinicio() {
    let dir = temp_dir();
    let config = Config {
        port: 0,
        data: dir.clone(),
        web: PathBuf::from("web"),
        public_origin: "http://x".into(),
        runner_origin: "http://y".into(),
        admin_token: String::new(),
    };
    {
        let st = server::open_state(config.clone()).unwrap();
        st.db.with_conn(|c| server::auth::create_user(c, "ana", false)).unwrap();
        st.db.checkpoint();
    }
    let st = server::open_state(config).unwrap();
    let users = st.db.with_conn(|c| server::auth::list_users(c)).unwrap();
    assert_eq!(users[0]["handle"], "ana");
}

#[test]
fn sesion_por_cookie_y_csrf() {
    let s = start();
    let ana = s.new_user("ana");
    const APP: &str = "http://stardust.test";

    let (st, _, _, _) = s.raw("POST", "/api/v1/session", &[], Some(json!({ "token": "sd_falso" })));
    assert_eq!(st, 401);
    let (st, set_cookie, _, body) = s.raw("POST", "/api/v1/session", &[], Some(json!({ "token": ana })));
    assert_eq!(st, 200, "{body}");
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"), "{set_cookie}");
    assert!(!set_cookie.contains("Secure"), "sin Secure en un origen http");
    assert!(!body.contains(&ana), "la respuesta no repite el token");
    let session = set_cookie.split(';').next().unwrap().to_string();
    let cookie = [("Cookie", session.as_str())];

    let (st, _, _, body) = s.raw("GET", "/api/v1/me", &cookie, None);
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("\"ana\""));

    // CSRF: con la cookie, escribir exige Origin = la app.
    let src = json!({ "source": std::fs::read_to_string("programs/texto/doble.stardust").unwrap() });
    let (st, _, _, _) = s.raw("POST", "/api/v1/programs", &cookie, Some(src.clone()));
    assert_eq!(st, 403, "sin Origin");
    let evil = [cookie[0], ("Origin", "https://evil.example")];
    let (st, _, _, _) = s.raw("POST", "/api/v1/programs", &evil, Some(src.clone()));
    assert_eq!(st, 403, "Origin ajeno");
    let ours = [cookie[0], ("Origin", APP)];
    let (st, _, _, body) = s.raw("POST", "/api/v1/programs", &ours, Some(src));
    assert_eq!(st, 201, "{body}");
    let (_, b) = s.call("GET", "/api/v1/programs/ana/doble/versions", Some(&ana), None);
    assert_eq!(b["versions"][0]["created_by_token"], "inicial", "la versión dice con qué token se abrió la sesión");

    // Cerrar sesión invalida la cookie.
    let (st, set_cookie, _, _) = s.raw("DELETE", "/api/v1/session", &ours, None);
    assert_eq!(st, 200);
    assert!(set_cookie.contains("Max-Age=0"), "{set_cookie}");
    let (st, _, _, _) = s.raw("GET", "/api/v1/me", &cookie, None);
    assert_eq!(st, 401);

    // Revocar el token con el que se abrió una sesión la cierra.
    let (_, set_cookie, _, _) = s.raw("POST", "/api/v1/session", &[], Some(json!({ "token": ana })));
    let session = set_cookie.split(';').next().unwrap().to_string();
    let cookie = [("Cookie", session.as_str())];
    assert_eq!(s.raw("GET", "/api/v1/me", &cookie, None).0, 200);
    let (_, me) = s.call("GET", "/api/v1/me", Some(&ana), None);
    let id = me["token"]["id"].as_i64().unwrap();
    assert_eq!(s.call("DELETE", &format!("/api/v1/tokens/{id}"), Some(&ana), None).0, 200);
    assert_eq!(s.raw("GET", "/api/v1/me", &cookie, None).0, 401);
}

#[test]
fn paginas_y_enrutado_por_host() {
    let web = temp_dir();
    std::fs::write(web.join("playground.html"), "<script>const CONFIG = /*__STARDUST_CONFIG__*/null;</script>").unwrap();
    std::fs::write(web.join("runner.html"), "<script>const CONFIG = /*__STARDUST_CONFIG__*/null;</script>").unwrap();
    let s = start_with(web);
    let run_host = [("Host", "run.test")];

    // App: configuración inyectada y no se deja enmarcar.
    for path in ["/", "/p/ana/doble", "/p/ana/doble/v/2"] {
        let (st, _, csp, body) = s.raw("GET", path, &[], None);
        assert_eq!(st, 200, "{path}");
        assert_eq!(csp, "frame-ancestors 'none'");
        assert!(body.contains(r#""server":true"#) && body.contains(r#""runnerOrigin":"http://run.test""#), "{body}");
        assert!(body.contains(r#""devMode":false"#));
    }
    // Runner: solo en su host, solo enmarcable por la app.
    let (st, _, csp, body) = s.raw("GET", "/run/", &run_host, None);
    assert_eq!(st, 200);
    assert_eq!(csp, "frame-ancestors http://stardust.test");
    assert!(body.contains(r#"{"publicOrigin":"http://stardust.test"}"#), "{body}");
    assert_eq!(s.raw("GET", "/run/", &[], None).0, 404, "el runner no existe en el host de la app");
    // En el host del runner no hay app ni API (sí /healthz).
    for path in ["/", "/p/ana/doble", "/api/v1/me", "/api/v1"] {
        assert_eq!(s.raw("GET", path, &run_host, None).0, 404, "{path}");
    }
    assert_eq!(s.raw("GET", "/healthz", &run_host, None).0, 200);
}

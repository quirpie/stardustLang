//! Rutas no-API: salud, páginas (playground, `/p/…`, runner) y la guía para agentes.
//!
//! El runner se sirve en otro origen (`STARDUST_RUNNER_ORIGIN`): ahí corre el
//! código de los programas, sin acceso a la sesión ni a los datos de la app. Ver
//! `docs/servidor.md`, «Playground y runner».

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use serde_json::json;

use super::{ApiError, Shared};

/// Marcador que el servidor sustituye por la configuración de la página. En el
/// HTML autónomo (abierto sin servidor) queda `null`: modo local.
const CONFIG_MARKER: &str = "/*__STARDUST_CONFIG__*/null";

/// Lee una página de `$STARDUST_WEB` e inyecta su configuración.
async fn page(s: &Shared, file: &str, config: serde_json::Value) -> Result<String, ApiError> {
    match tokio::fs::read_to_string(s.config.web.join(file)).await {
        Ok(html) => Ok(html.replacen(CONFIG_MARKER, &config.to_string(), 1)),
        Err(_) => Err(ApiError::not_found(format!("{file} no construido: ejecuta web/build-playground.sh"))),
    }
}

fn with_headers(mut res: Response, headers: &[(header::HeaderName, String)]) -> Response {
    for (k, v) in headers {
        if let Ok(v) = HeaderValue::from_str(v) {
            res.headers_mut().insert(k.clone(), v);
        }
    }
    res
}

/// `GET /`, `/p/{owner}/{name}` y `/p/{owner}/{name}/v/{n}`: el playground. La
/// página decide por la ruta si muestra el escritorio o un solo programa.
pub async fn playground(State(s): State<Shared>) -> Response {
    let c = &s.config;
    let config = json!({
        "server": true,
        "publicOrigin": c.public_origin,
        "runnerOrigin": c.runner_origin,
        "devMode": c.runner_origin == c.public_origin,
    });
    match page(&s, "playground.html", config).await {
        Ok(html) => with_headers(
            Html(html).into_response(),
            &[
                (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'".into()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
                (header::REFERRER_POLICY, "same-origin".into()),
                (header::CACHE_CONTROL, "no-cache".into()),
            ],
        ),
        Err(e) => e.into_response(),
    }
}

/// `GET /run/`: el runner. Solo se deja enmarcar por la app y solo acepta sus
/// mensajes (`publicOrigin`).
pub async fn runner(State(s): State<Shared>) -> Response {
    let config = json!({ "publicOrigin": s.config.public_origin });
    match page(&s, "runner.html", config).await {
        Ok(html) => with_headers(
            Html(html).into_response(),
            &[
                (header::CONTENT_SECURITY_POLICY, format!("frame-ancestors {}", s.config.public_origin)),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
                (header::REFERRER_POLICY, "no-referrer".into()),
                (header::CACHE_CONTROL, "no-cache".into()),
            ],
        ),
        Err(e) => e.into_response(),
    }
}

/// Host de un origen (`https://run.example.com` → `run.example.com`).
fn host_of(origin: &str) -> &str {
    origin.split_once("://").map_or(origin, |(_, h)| h).trim_end_matches('/')
}

/// Separa los dos orígenes cuando son distintos: en el host del runner solo
/// existen `/run/` y `/healthz`, y `/run/` solo existe en el host del runner.
pub async fn route_by_host(State(s): State<Shared>, req: Request, next: Next) -> Response {
    let c = &s.config;
    if c.runner_origin != c.public_origin {
        let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
        let on_runner = host.eq_ignore_ascii_case(host_of(&c.runner_origin));
        let path = req.uri().path();
        let runner_path = path == "/run/" || path == "/run";
        if (on_runner && !runner_path && path != "/healthz") || (!on_runner && runner_path) {
            return ApiError::not_found("no existe en este origen").into_response();
        }
    }
    next.run(req).await
}

/// `GET /healthz`: 200 solo si la base responde.
pub async fn healthz(State(s): State<Shared>) -> Response {
    let ok = s
        .db
        .run(|c| Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?))
        .await
        .is_ok();
    if ok { (StatusCode::OK, "ok").into_response() } else { (StatusCode::SERVICE_UNAVAILABLE, "db").into_response() }
}

/// `GET /api/v1` y `/llms.txt`: guía breve para una IA que llega sin contexto.
pub async fn guide(State(s): State<Shared>) -> Response {
    let base = format!("{}/api/v1", s.config.public_origin);
    let text = GUIDE.replace("{BASE}", &base);
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
}

const GUIDE: &str = r#"# StardustLang — servidor de programas

StardustLang es un lenguaje de actores aislados con capacidades declaradas. Los
programas se escriben en texto (.stardust) o en su IR JSON, y se ejecutan en el
navegador (VM en WASM). Este servidor los valida, guarda y versiona.

Especificación del lenguaje: docs/sintaxis-texto.md del repositorio.

## Autenticación
Authorization: Bearer <token>   (tokens "sd_…", los crea un administrador)

## Flujo recomendado
1. Validar sin guardar (errores por línea; repite hasta "ok": true):
   POST {BASE}/check            {"source": "<texto>", "format": "stardust"}
2. Instalar (crea la versión 1; repetirlo con la misma fuente es inocuo):
   POST {BASE}/programs         {"name": "mi-app", "source": "<texto>"}
3. Actualizar sobre la versión que leíste (si otro cambió el programa: 409):
   GET  {BASE}/programs/<owner>/<name>?fields=meta    → current_version
   POST {BASE}/programs/<owner>/<name>/versions       {"base_version": N, "source": "<texto>"}

## Otras operaciones
GET    {BASE}/programs                                 mis programas
GET    {BASE}/programs/<owner>/<name>                  versión actual con source e ir
GET    {BASE}/programs/<owner>/<name>/versions         historial
GET    {BASE}/programs/<owner>/<name>/versions/<n>     una versión
POST   {BASE}/programs/<owner>/<name>/rollback         {"to_version": n, "base_version": N}
PATCH  {BASE}/programs/<owner>/<name>                  {"visibility": "private" | "link"}
DELETE {BASE}/programs/<owner>/<name>
GET    {BASE}/me

## Errores
{"error": {"code": "...", "message": "..."}}. Un programa inválido es 422 con
"report": {"ok": false, "errors": [{"line", "col", "message"}], ...}.

## Ejemplo
curl -s {BASE}/check -H "Authorization: Bearer $TOKEN" \
  --json "$(jq -n --rawfile s app.stardust '{source:$s}')"
"#;

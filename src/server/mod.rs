//! Servidor de programas StardustLang (`stardust-server`): instala, versiona y valida
//! programas, y gestiona usuarios con tokens de API. Los programas no se ejecutan
//! aquí: corren en el navegador (WASM). Diseño en `docs/servidor.md`; despliegue y
//! contrato con el entorno en `docs/deploy-fly.md`.
//!
//! ```text
//! HTTP ──axum──▶ handlers (programs, auth, web) ──spawn_blocking──▶ SQLite (db)
//!                    └─▶ validate (lang::check_source / check::check: el código de la VM)
//! ```

pub mod auth;
pub mod db;
pub mod programs;
pub mod validate;
pub mod web;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{DefaultBodyLimit, Request};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value as J};

use crate::date::Date;

/// Configuración del servidor, leída de variables de entorno (ver el contrato en
/// `docs/deploy-fly.md`).
#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    /// Directorio de todo el estado; la base es `<data>/stardust.db`.
    pub data: PathBuf,
    /// Estáticos (`playground.html`).
    pub web: PathBuf,
    /// Origen público de la app y la API (`https://stardust.example.com`).
    pub public_origin: String,
    /// Origen del runner (etapa 2). Si coincide con el público: modo desarrollo.
    pub runner_origin: String,
    /// Token de administración (`STARDUST_ADMIN_TOKEN`); vacío = solo usuarios admin.
    pub admin_token: String,
}

impl Config {
    pub fn from_env() -> Config {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let port = var("PORT").and_then(|p| p.parse().ok()).unwrap_or(8080);
        let public_origin = var("STARDUST_PUBLIC_ORIGIN").unwrap_or_else(|| format!("http://localhost:{port}"));
        Config {
            port,
            data: var("STARDUST_DATA").unwrap_or_else(|| "data".into()).into(),
            web: var("STARDUST_WEB").unwrap_or_else(|| "web".into()).into(),
            runner_origin: var("STARDUST_RUNNER_ORIGIN").unwrap_or_else(|| public_origin.clone()),
            public_origin: public_origin.trim_end_matches('/').to_string(),
            admin_token: var("STARDUST_ADMIN_TOKEN").unwrap_or_default(),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data.join("stardust.db")
    }
}

/// Estado compartido por todos los handlers.
pub struct AppState {
    pub config: Config,
    pub db: db::Db,
    pub limiter: auth::RateLimiter,
}

pub type Shared = Arc<AppState>;

/// Límites de la API (ver `docs/servidor.md`, «Límites»).
pub mod limits {
    /// Tamaño máximo de la fuente de un programa.
    pub const SOURCE_BYTES: usize = 256 * 1024;
    /// Tope del cuerpo HTTP: la fuente va escapada dentro de un JSON.
    pub const BODY_BYTES: usize = 1024 * 1024;
    pub const PROGRAMS_PER_USER: i64 = 100;
    pub const REQUESTS_PER_MINUTE: u32 = 120;
    pub const CHECKS_PER_MINUTE: u32 = 300;
}

// --- Errores de la API -------------------------------------------------------

/// Error de la API: `{"error": {"code", "message", ...extra}}` con su estado HTTP.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    /// Campos extra (en caja: casi ningún error los lleva y así `Result` es pequeño).
    pub more: Option<Box<Extra>>,
}

#[derive(Debug, Default)]
pub struct Extra {
    /// Dentro de `error` (p. ej. `current_version`).
    pub error: Map<String, J>,
    /// Al nivel superior (p. ej. `report` en un 422).
    pub top: Map<String, J>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError { status, code, message: message.into(), more: None }
    }
    pub fn bad_request(m: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, "bad_request", m)
    }
    pub fn unauthorized(m: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", m)
    }
    pub fn forbidden(m: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::FORBIDDEN, "forbidden", m)
    }
    pub fn not_found(m: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", m)
    }
    pub fn internal(m: impl std::fmt::Display) -> ApiError {
        log("error", &format!("interno: {m}"));
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "error interno del servidor")
    }
    pub fn with(mut self, k: &str, v: J) -> ApiError {
        self.more.get_or_insert_default().error.insert(k.into(), v);
        self
    }
    pub fn with_top(mut self, k: &str, v: J) -> ApiError {
        self.more.get_or_insert_default().top.insert(k.into(), v);
        self
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> ApiError {
        ApiError::internal(format!("sqlite: {e}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut err = Map::new();
        err.insert("code".into(), J::from(self.code));
        err.insert("message".into(), J::from(self.message));
        let more = self.more.map(|m| *m).unwrap_or_default();
        err.extend(more.error);
        let mut body = Map::new();
        body.insert("error".into(), J::Object(err));
        body.extend(more.top);
        (self.status, Json(J::Object(body))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// Como `Json<T>`, pero los cuerpos inválidos responden con el formato de error de
/// la API (no con el texto plano de axum).
pub struct JsonBody<T>(pub T);

impl<T: serde::de::DeserializeOwned, S: Send + Sync> axum::extract::FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(JsonBody(v)),
            Err(rej) => {
                // axum usa 422 para un JSON con campos erróneos; aquí 422 queda
                // reservado a «programa inválido», así que se responde 400.
                let (status, code) = match rej.status() {
                    StatusCode::PAYLOAD_TOO_LARGE => (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
                    StatusCode::UNSUPPORTED_MEDIA_TYPE => (StatusCode::UNSUPPORTED_MEDIA_TYPE, "bad_request"),
                    _ => (StatusCode::BAD_REQUEST, "bad_request"),
                };
                Err(ApiError::new(status, code, rej.body_text()))
            }
        }
    }
}

// --- Utilidades -------------------------------------------------------------

/// Fecha y hora UTC actual en ISO 8601 (`2026-10-03T12:30:00Z`).
pub fn now_iso() -> String {
    iso_at(now_secs())
}

/// Segundos desde 1970-01-01 (UTC).
pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Un instante (segundos desde 1970-01-01) en ISO 8601 UTC.
pub fn iso_at(secs: i64) -> String {
    let d = Date::from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", d.y, d.m, d.d, s / 3600, s / 60 % 60, s % 60)
}

/// Una línea de log a stdout. Nunca se registran tokens ni fuentes de programas.
pub fn log(level: &str, msg: &str) {
    println!("{} {level} {msg}", now_iso());
}

// --- Router y arranque ------------------------------------------------------

pub fn router(state: Shared) -> Router {
    let api = Router::new()
        .route("/", get(web::guide))
        .route("/check", post(programs::check))
        .route("/programs", get(programs::list_mine).post(programs::install))
        .route(
            "/programs/{owner}/{name}",
            get(programs::get_program).patch(programs::patch_program).delete(programs::delete_program),
        )
        .route("/programs/{owner}/{name}/versions", get(programs::list_versions).post(programs::update))
        .route("/programs/{owner}/{name}/versions/{n}", get(programs::get_version))
        .route("/programs/{owner}/{name}/rollback", post(programs::rollback))
        .route("/me", get(auth::me))
        .route("/session", post(auth::create_session).delete(auth::delete_session))
        .route("/tokens", get(auth::list_tokens).post(auth::create_token))
        .route("/tokens/{id}", delete(auth::revoke_token))
        .route("/admin/users", get(auth::admin_list_users).post(auth::admin_create_user))
        .route("/admin/users/{handle}/disable", post(auth::admin_disable_user))
        .fallback(|| async { ApiError::not_found("ruta de la API desconocida") });

    Router::new()
        .nest("/api/v1", api)
        .route("/", get(web::playground))
        .route("/p/{owner}/{name}", get(web::playground))
        .route("/p/{owner}/{name}/v/{n}", get(web::playground))
        .route("/run/", get(web::runner))
        .route("/healthz", get(web::healthz))
        .route("/llms.txt", get(web::guide))
        .layer(DefaultBodyLimit::max(limits::BODY_BYTES))
        .layer(middleware::from_fn_with_state(state.clone(), web::route_by_host))
        .layer(middleware::from_fn(log_requests))
        .with_state(state)
}

/// Registra método, ruta (sin query), estado y duración de cada petición.
async fn log_requests(req: Request, next: Next) -> Response {
    let (method, path) = (req.method().clone(), req.uri().path().to_string());
    let t = Instant::now();
    let res = next.run(req).await;
    if path != "/healthz" {
        log("info", &format!("{method} {path} {} {}ms", res.status().as_u16(), t.elapsed().as_millis()));
    }
    res
}

/// Abre la base (aplicando migraciones) y construye el estado.
pub fn open_state(config: Config) -> Result<Shared, String> {
    std::fs::create_dir_all(&config.data)
        .map_err(|e| format!("no se pudo crear {}: {e}", config.data.display()))?;
    let db = db::Db::open(&config.db_path())?;
    Ok(Arc::new(AppState { config, db, limiter: auth::RateLimiter::default() }))
}

/// Sirve hasta que `shutdown` se resuelva; después hace checkpoint del WAL.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: Shared,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let c = &state.config;
    if c.runner_origin == c.public_origin {
        log("warn", "modo desarrollo: el runner comparte origen con la app (no abras programas de terceros)");
    }
    if c.admin_token.is_empty() {
        log("warn", "STARDUST_ADMIN_TOKEN vacío: la API de admin solo acepta usuarios admin");
    }
    log("info", &format!("escuchando en {} (datos: {})", listener.local_addr()?, c.data.display()));
    let db = state.db.clone();
    axum::serve(listener, router(state)).with_graceful_shutdown(shutdown).await?;
    db.checkpoint();
    log("info", "parada limpia");
    Ok(())
}

/// Espera SIGINT (Ctrl-C) o, en Unix, SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    log("info", "señal de parada recibida");
}

/// JSON de respuesta con estado.
pub fn reply(status: StatusCode, body: J) -> Response {
    (status, Json(body)).into_response()
}

/// `{"ok": true}`.
pub fn ok() -> Response {
    reply(StatusCode::OK, json!({ "ok": true }))
}

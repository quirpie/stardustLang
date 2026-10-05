//! Usuarios y tokens de API (`docs/servidor.md`, «Usuarios y autenticación»).
//!
//! - Token: `sd_` + 32 bytes aleatorios en base32. En la base solo se guarda su
//!   SHA-256 (tiene alta entropía: no hace falta un KDF de contraseñas) y un
//!   prefijo visible.
//! - `Caller`: quien llama con `Authorization: Bearer <token>`.
//! - `Admin`: el `STARDUST_ADMIN_TOKEN` o un usuario con `is_admin`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value as J};

use super::validate::valid_name;
use super::{iso_at, limits, now_iso, now_secs, reply, ApiError, ApiResult, JsonBody, Shared};
use crate::crypto;

// --- Tokens ------------------------------------------------------------------

const B32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

fn base32(bytes: &[u8]) -> String {
    let (mut out, mut buf, mut bits) = (String::new(), 0u32, 0u32);
    for &b in bytes {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Token nuevo: (token completo, prefijo visible, hash para la base).
pub fn new_token() -> (String, String, [u8; 32]) {
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw).expect("sin fuente de aleatoriedad del sistema");
    let token = format!("sd_{}", base32(&raw));
    let prefix = token[..9].to_string();
    let hash = crypto::sha256(token.as_bytes());
    (token, prefix, hash)
}

/// Emite un token para un usuario. Devuelve (id, token).
pub fn issue_token(conn: &Connection, user_id: i64, name: &str) -> rusqlite::Result<(i64, String, String)> {
    let (token, prefix, hash) = new_token();
    conn.execute(
        "INSERT INTO tokens (user_id, name, prefix, sha256, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![user_id, name, prefix, hash.to_vec(), now_iso()],
    )?;
    Ok((conn.last_insert_rowid(), token, prefix))
}

// --- Usuarios (compartido por la API de admin y el CLI) ----------------------

/// Crea un usuario con un primer token. Devuelve `{user, token}`; el token solo se
/// muestra esta vez.
pub fn create_user(conn: &mut Connection, handle: &str, admin: bool) -> ApiResult<J> {
    if !valid_name(handle) {
        return Err(ApiError::bad_request(format!(
            "handle '{handle}' no válido: minúsculas, cifras y guiones; empieza por letra o cifra; máximo 40"
        )));
    }
    let tx = conn.transaction()?;
    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM users WHERE handle = ?1)", [handle], |r| r.get(0))?;
    if exists {
        return Err(ApiError::new(StatusCode::CONFLICT, "user_exists", format!("ya existe el usuario '{handle}'")));
    }
    tx.execute(
        "INSERT INTO users (handle, is_admin, created_at) VALUES (?1, ?2, ?3)",
        params![handle, admin, now_iso()],
    )?;
    let user_id = tx.last_insert_rowid();
    let (_, token, _) = issue_token(&tx, user_id, "inicial")?;
    tx.commit()?;
    Ok(json!({ "user": user_json(conn, user_id)?, "token": token }))
}

fn user_json(conn: &Connection, id: i64) -> rusqlite::Result<J> {
    conn.query_row(
        "SELECT handle, is_admin, disabled_at, created_at,
                (SELECT COUNT(*) FROM programs WHERE owner_id = users.id),
                (SELECT COUNT(*) FROM tokens WHERE user_id = users.id AND revoked_at IS NULL)
         FROM users WHERE id = ?1",
        [id],
        |r| {
            Ok(json!({
                "handle": r.get::<_, String>(0)?,
                "is_admin": r.get::<_, bool>(1)?,
                "disabled_at": r.get::<_, Option<String>>(2)?,
                "created_at": r.get::<_, String>(3)?,
                "programs": r.get::<_, i64>(4)?,
                "active_tokens": r.get::<_, i64>(5)?,
            }))
        },
    )
}

pub fn list_users(conn: &Connection) -> rusqlite::Result<Vec<J>> {
    let ids: Vec<i64> = conn
        .prepare("SELECT id FROM users ORDER BY handle")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    ids.into_iter().map(|id| user_json(conn, id)).collect()
}

fn user_id(conn: &Connection, handle: &str) -> ApiResult<i64> {
    conn.query_row("SELECT id FROM users WHERE handle = ?1", [handle], |r| r.get(0))
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no existe el usuario '{handle}'")))
}

/// Deshabilita un usuario y revoca todos sus tokens. Sus programas se conservan.
pub fn disable_user(conn: &mut Connection, handle: &str) -> ApiResult<J> {
    let id = user_id(conn, handle)?;
    let now = now_iso();
    let tx = conn.transaction()?;
    tx.execute("UPDATE users SET disabled_at = COALESCE(disabled_at, ?2) WHERE id = ?1", params![id, now])?;
    tx.execute("UPDATE tokens SET revoked_at = ?2 WHERE user_id = ?1 AND revoked_at IS NULL", params![id, now])?;
    tx.execute("DELETE FROM sessions WHERE user_id = ?1", [id])?;
    tx.commit()?;
    Ok(user_json(conn, id)?)
}

/// Emite un token nuevo para un usuario existente (p. ej. si perdió el suyo).
pub fn token_for(conn: &mut Connection, handle: &str, name: &str) -> ApiResult<String> {
    let id = user_id(conn, handle)?;
    Ok(issue_token(conn, id, name)?.1)
}

// --- Quién llama --------------------------------------------------------------

/// Usuario autenticado por su token.
#[derive(Clone, Debug)]
pub struct Caller {
    pub user_id: i64,
    pub handle: String,
    pub is_admin: bool,
    pub token_id: i64,
    pub token_name: String,
}

fn bearer(parts: &Parts) -> Option<String> {
    let v = parts.headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim().to_string())
}

/// Nombre de la cookie de sesión web.
pub const SESSION_COOKIE: &str = "sd_session";
/// Duración de una sesión web.
const SESSION_DAYS: i64 = 30;

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == name && !v.is_empty()).then(|| v.to_string())
    })
}

/// ¿Viene de la propia app? (`Origin` = `STARDUST_PUBLIC_ORIGIN`).
fn same_origin(headers: &HeaderMap, state: &Shared) -> bool {
    headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) == Some(state.config.public_origin.as_str())
}

/// Fila de quien llama + si su usuario está deshabilitado.
fn caller_row(r: &rusqlite::Row) -> rusqlite::Result<(Caller, Option<String>)> {
    Ok((
        Caller { token_id: r.get(0)?, token_name: r.get(1)?, user_id: r.get(2)?, handle: r.get(3)?, is_admin: r.get(4)? },
        r.get(5)?,
    ))
}

/// Usuario de un token (sin límite de peticiones).
async fn by_token(state: &Shared, token: String) -> ApiResult<Caller> {
    let hash = crypto::sha256(token.as_bytes()).to_vec();
    state
        .db
        .run(move |conn| {
            let row = conn
                .query_row(
                    "SELECT t.id, t.name, u.id, u.handle, u.is_admin, u.disabled_at
                     FROM tokens t JOIN users u ON u.id = t.user_id
                     WHERE t.sha256 = ?1 AND t.revoked_at IS NULL",
                    [hash],
                    caller_row,
                )
                .optional()?;
            let (caller, disabled) = row.ok_or_else(|| ApiError::unauthorized("token inválido o revocado"))?;
            if disabled.is_some() {
                return Err(ApiError::forbidden("usuario deshabilitado"));
            }
            conn.execute("UPDATE tokens SET last_used_at = ?2 WHERE id = ?1", params![caller.token_id, now_iso()])?;
            Ok(caller)
        })
        .await
}

/// Usuario de una cookie de sesión vigente (cuyo token no esté revocado).
async fn by_session(state: &Shared, session: String) -> ApiResult<Caller> {
    let hash = crypto::sha256(session.as_bytes()).to_vec();
    state
        .db
        .run(move |conn| {
            let row = conn
                .query_row(
                    "SELECT t.id, t.name, u.id, u.handle, u.is_admin, u.disabled_at
                     FROM sessions s JOIN tokens t ON t.id = s.token_id JOIN users u ON u.id = s.user_id
                     WHERE s.sha256 = ?1 AND s.expires_at > ?2 AND t.revoked_at IS NULL",
                    params![hash, now_iso()],
                    caller_row,
                )
                .optional()?;
            let (caller, disabled) =
                row.ok_or_else(|| ApiError::unauthorized("sesión caducada o cerrada: vuelve a iniciar sesión"))?;
            if disabled.is_some() {
                return Err(ApiError::forbidden("usuario deshabilitado"));
            }
            Ok(caller)
        })
        .await
}

/// Quién llama: `Authorization: Bearer` o, si no hay, la cookie de sesión. Sin
/// ninguna de las dos, `None`; con una inválida, error (no se ignora en silencio).
///
/// CSRF: una petición que cambia algo y se autentica con la cookie debe venir de
/// la propia app. Con `SameSite=Strict` la cookie ya no viajaría desde otro sitio;
/// comprobar `Origin` es la segunda barrera.
async fn authenticate(parts: &Parts, state: &Shared) -> ApiResult<Option<Caller>> {
    let caller = if let Some(token) = bearer(parts) {
        by_token(state, token).await?
    } else if let Some(session) = cookie(&parts.headers, SESSION_COOKIE) {
        let safe = matches!(parts.method.as_str(), "GET" | "HEAD" | "OPTIONS");
        if !safe && !same_origin(&parts.headers, state) {
            return Err(ApiError::forbidden("origen no permitido para una petición con sesión"));
        }
        by_session(state, session).await?
    } else {
        return Ok(None);
    };
    state.limiter.check(caller.token_id, parts.uri.path().ends_with("/check"))?;
    Ok(Some(caller))
}

impl FromRequestParts<Shared> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Shared) -> Result<Self, Self::Rejection> {
        authenticate(parts, state)
            .await?
            .ok_or_else(|| ApiError::unauthorized("falta la cabecera Authorization: Bearer <token> (o inicia sesión)"))
    }
}

/// Autenticación opcional: sin credenciales, `None`; con unas inválidas, error.
pub struct MaybeCaller(pub Option<Caller>);

impl FromRequestParts<Shared> for MaybeCaller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Shared) -> Result<Self, Self::Rejection> {
        Ok(MaybeCaller(authenticate(parts, state).await?))
    }
}

/// Administración: el `STARDUST_ADMIN_TOKEN` o un usuario admin.
pub struct Admin;

impl FromRequestParts<Shared> for Admin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Shared) -> Result<Self, Self::Rejection> {
        let admin = &state.config.admin_token;
        if let Some(token) = bearer(parts) {
            if !admin.is_empty() && crypto::ct_eq(admin.as_bytes(), token.as_bytes()) {
                return Ok(Admin);
            }
        }
        match authenticate(parts, state).await? {
            Some(c) if c.is_admin => Ok(Admin),
            Some(_) => Err(ApiError::forbidden("solo para administradores")),
            None => Err(ApiError::unauthorized("falta la cabecera Authorization: Bearer <token>")),
        }
    }
}

// --- Límite de peticiones ----------------------------------------------------

/// Ventana fija de un minuto por token (y aparte para `/check`).
#[derive(Default)]
pub struct RateLimiter(Mutex<HashMap<(i64, bool), (u64, u32)>>);

impl RateLimiter {
    pub fn check(&self, token_id: i64, is_check: bool) -> ApiResult<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let minute = now / 60;
        let limit = if is_check { limits::CHECKS_PER_MINUTE } else { limits::REQUESTS_PER_MINUTE };
        let mut m = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() > 10_000 {
            m.retain(|_, (w, _)| *w == minute);
        }
        let e = m.entry((token_id, is_check)).or_insert((minute, 0));
        if e.0 != minute {
            *e = (minute, 0);
        }
        e.1 += 1;
        if e.1 > limit {
            let retry = 60 - now % 60;
            return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", format!("más de {limit} peticiones por minuto"))
                .with("retry_after", json!(retry)));
        }
        Ok(())
    }
}

// --- Handlers ----------------------------------------------------------------

pub async fn me(State(s): State<Shared>, caller: Caller) -> ApiResult<Response> {
    let id = caller.user_id;
    let user = s.db.run(move |c| Ok(user_json(c, id)?)).await?;
    Ok(reply(StatusCode::OK, json!({ "user": user, "token": { "id": caller.token_id, "name": caller.token_name } })))
}

pub async fn list_tokens(State(s): State<Shared>, caller: Caller) -> ApiResult<Response> {
    let uid = caller.user_id;
    let tokens = s
        .db
        .run(move |c| {
            let mut st = c.prepare(
                "SELECT id, name, prefix, created_at, last_used_at, revoked_at FROM tokens WHERE user_id = ?1 ORDER BY id",
            )?;
            let rows = st.query_map([uid], |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "prefix": r.get::<_, String>(2)?,
                    "created_at": r.get::<_, String>(3)?, "last_used_at": r.get::<_, Option<String>>(4)?,
                    "revoked_at": r.get::<_, Option<String>>(5)?,
                }))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    Ok(reply(StatusCode::OK, json!({ "tokens": tokens })))
}

#[derive(Deserialize)]
pub struct NewToken {
    name: String,
}

pub async fn create_token(State(s): State<Shared>, caller: Caller, JsonBody(b): JsonBody<NewToken>) -> ApiResult<Response> {
    let name = b.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 60 {
        return Err(ApiError::bad_request("name debe tener entre 1 y 60 caracteres"));
    }
    let uid = caller.user_id;
    let (id, token, prefix) = s.db.run(move |c| Ok(issue_token(c, uid, &name)?)).await?;
    Ok(reply(StatusCode::CREATED, json!({ "id": id, "token": token, "prefix": prefix })))
}

pub async fn revoke_token(State(s): State<Shared>, caller: Caller, Path(id): Path<i64>) -> ApiResult<Response> {
    let uid = caller.user_id;
    let n = s
        .db
        .run(move |c| {
            Ok(c.execute(
                "UPDATE tokens SET revoked_at = COALESCE(revoked_at, ?3) WHERE id = ?1 AND user_id = ?2",
                params![id, uid, now_iso()],
            )?)
        })
        .await?;
    if n == 0 {
        return Err(ApiError::not_found("no existe ese token"));
    }
    Ok(super::ok())
}

pub async fn admin_list_users(State(s): State<Shared>, _: Admin) -> ApiResult<Response> {
    let users = s.db.run(|c| Ok(list_users(c)?)).await?;
    Ok(reply(StatusCode::OK, json!({ "users": users })))
}

#[derive(Deserialize)]
pub struct NewUser {
    handle: String,
    #[serde(default)]
    admin: bool,
}

pub async fn admin_create_user(State(s): State<Shared>, _: Admin, JsonBody(b): JsonBody<NewUser>) -> ApiResult<Response> {
    let out = s.db.run(move |c| create_user(c, &b.handle, b.admin)).await?;
    Ok(reply(StatusCode::CREATED, out))
}

pub async fn admin_disable_user(State(s): State<Shared>, _: Admin, Path(handle): Path<String>) -> ApiResult<Response> {
    let user = s.db.run(move |c| disable_user(c, &handle)).await?;
    Ok(reply(StatusCode::OK, json!({ "user": user })))
}

// --- Sesión web ---------------------------------------------------------------

#[derive(Deserialize)]
pub struct Login {
    token: String,
}

fn session_cookie(state: &Shared, value: &str, max_age: i64) -> String {
    let secure = if state.config.public_origin.starts_with("https://") { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict{secure}")
}

fn with_cookie(mut res: Response, cookie: String) -> Response {
    if let Ok(v) = cookie.parse() {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    res
}

/// `POST /session`: cambia un token por una cookie de sesión `HttpOnly` (el
/// navegador no guarda el token).
pub async fn create_session(State(s): State<Shared>, JsonBody(b): JsonBody<Login>) -> ApiResult<Response> {
    let caller = by_token(&s, b.token.trim().to_string()).await?;
    s.limiter.check(caller.token_id, false)?;
    let (session, _, hash) = new_token();
    let (uid, tid) = (caller.user_id, caller.token_id);
    let user = s
        .db
        .run(move |c| {
            c.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now_iso()])?;
            c.execute(
                "INSERT INTO sessions (sha256, user_id, token_id, expires_at) VALUES (?1, ?2, ?3, ?4)",
                params![hash.to_vec(), uid, tid, iso_at(now_secs() + SESSION_DAYS * 86_400)],
            )?;
            Ok(user_json(c, uid)?)
        })
        .await?;
    let body = json!({ "user": user, "token": { "id": caller.token_id, "name": caller.token_name } });
    Ok(with_cookie(reply(StatusCode::OK, body), session_cookie(&s, &session, SESSION_DAYS * 86_400)))
}

/// `DELETE /session`: cierra la sesión del navegador.
pub async fn delete_session(State(s): State<Shared>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(session) = cookie(&headers, SESSION_COOKIE) {
        if !same_origin(&headers, &s) {
            return Err(ApiError::forbidden("origen no permitido para una petición con sesión"));
        }
        let hash = crypto::sha256(session.as_bytes()).to_vec();
        s.db.run(move |c| Ok(c.execute("DELETE FROM sessions WHERE sha256 = ?1", [hash])?)).await?;
    }
    Ok(with_cookie(super::ok(), session_cookie(&s, "", 0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_rfc4648() {
        assert_eq!(base32(b""), "");
        assert_eq!(base32(b"f"), "my");
        assert_eq!(base32(b"foobar"), "mzxw6ytboi");
    }

    #[test]
    fn tokens_distintos_y_con_prefijo() {
        let (a, pa, ha) = new_token();
        let (b, _, _) = new_token();
        assert_ne!(a, b);
        assert!(a.starts_with("sd_") && a.len() == 3 + 52);
        assert!(a.starts_with(&pa));
        assert_eq!(ha, crypto::sha256(a.as_bytes()));
    }
}

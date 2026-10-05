//! Programas y versiones: `check`, installProgram, updateProgram, rollback y
//! lecturas (`docs/servidor.md`, «API HTTP»).
//!
//! El historial solo crece: cada cambio inserta una versión nueva dentro de una
//! transacción `IMMEDIATE`, que comprueba `current_version` (conflictos → 409).

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Deserialize;
use serde_json::{json, Value as J};

use super::auth::{Caller, MaybeCaller};
use super::validate::{self, add_warning, capabilities_grew, slugify, valid_name, Validated};
use super::{limits, now_iso, reply, ApiError, ApiResult, JsonBody, Shared};
use crate::crypto::to_hex;

// --- Filas y permisos ---------------------------------------------------------

struct Prog {
    id: i64,
    owner_id: i64,
    owner: String,
    name: String,
    visibility: String,
    current: i64,
    created_at: String,
    updated_at: String,
}

fn find(conn: &Connection, owner: &str, name: &str) -> rusqlite::Result<Option<Prog>> {
    conn.query_row(
        "SELECT p.id, p.owner_id, u.handle, p.name, p.visibility, p.current_version, p.created_at, p.updated_at
         FROM programs p JOIN users u ON u.id = p.owner_id WHERE u.handle = ?1 AND p.name = ?2",
        [owner, name],
        |r| {
            Ok(Prog {
                id: r.get(0)?,
                owner_id: r.get(1)?,
                owner: r.get(2)?,
                name: r.get(3)?,
                visibility: r.get(4)?,
                current: r.get(5)?,
                created_at: r.get(6)?,
                updated_at: r.get(7)?,
            })
        },
    )
    .optional()
}

fn not_found(owner: &str, name: &str) -> ApiError {
    ApiError::not_found(format!("no existe el programa '{owner}/{name}'"))
}

/// Lectura: el propietario, o cualquiera si es `link`. Un `private` ajeno es 404
/// (no se revela que existe).
fn readable(p: Option<Prog>, caller: Option<&Caller>, owner: &str, name: &str) -> ApiResult<Prog> {
    match p {
        Some(p) if p.visibility == "link" || caller.is_some_and(|c| c.user_id == p.owner_id) => Ok(p),
        _ => Err(not_found(owner, name)),
    }
}

/// Escritura: solo el propietario. Un `link` ajeno es 403; un `private` ajeno, 404.
fn owned(p: Option<Prog>, caller: &Caller, owner: &str, name: &str) -> ApiResult<Prog> {
    match p {
        Some(p) if p.owner_id == caller.user_id => Ok(p),
        Some(p) if p.visibility == "link" => Err(ApiError::forbidden("solo el propietario puede cambiar este programa")),
        _ => Err(not_found(owner, name)),
    }
}

/// Datos de la versión actual que deciden «unchanged» y `capabilities_changed`.
struct Head {
    sha256: Vec<u8>,
    format: String,
    capabilities: J,
}

fn head(conn: &Connection, p: &Prog) -> rusqlite::Result<Head> {
    conn.query_row(
        "SELECT sha256, format, capabilities FROM versions WHERE program_id = ?1 AND n = ?2",
        params![p.id, p.current],
        |r| {
            Ok(Head {
                sha256: r.get(0)?,
                format: r.get(1)?,
                capabilities: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or(J::Null),
            })
        },
    )
}

// --- Serialización ------------------------------------------------------------

fn version_json(conn: &Connection, program_id: i64, n: i64, full: bool) -> rusqlite::Result<Option<J>> {
    conn.query_row(
        "SELECT v.n, v.format, v.source, v.ir, v.sha256, v.capabilities, v.note, v.created_at, t.name
         FROM versions v LEFT JOIN tokens t ON t.id = v.token_id
         WHERE v.program_id = ?1 AND v.n = ?2",
        params![program_id, n],
        |r| {
            let mut v = json!({
                "n": r.get::<_, i64>(0)?,
                "format": r.get::<_, String>(1)?,
                "sha256": to_hex(&r.get::<_, Vec<u8>>(4)?),
                "capabilities": serde_json::from_str::<J>(&r.get::<_, String>(5)?).unwrap_or(J::Null),
                "note": r.get::<_, Option<String>>(6)?,
                "created_at": r.get::<_, String>(7)?,
                "created_by_token": r.get::<_, Option<String>>(8)?,
            });
            if full {
                v["source"] = J::from(r.get::<_, String>(2)?);
                v["ir"] = serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(J::Null);
            }
            Ok(v)
        },
    )
    .optional()
}

fn program_json(conn: &Connection, origin: &str, p: &Prog, full: bool) -> rusqlite::Result<J> {
    Ok(json!({
        "owner": p.owner,
        "name": p.name,
        "visibility": p.visibility,
        "url": format!("{origin}/p/{}/{}", p.owner, p.name),
        "current_version": p.current,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
        "version": version_json(conn, p.id, p.current, full)?,
    }))
}

/// Respuesta de install/update/rollback: `{program, report?, unchanged, capabilities_changed}`.
/// El programa va sin `source` ni `ir` (quien llama acaba de enviarlos).
fn change(status: StatusCode, program: J, report: Option<J>, unchanged: bool, caps_changed: bool) -> Response {
    let mut body = json!({ "program": program, "unchanged": unchanged, "capabilities_changed": caps_changed });
    if let Some(r) = report {
        body["report"] = r;
    }
    let location = format!("/api/v1/programs/{}/{}", body["program"]["owner"].as_str().unwrap_or(""), body["program"]["name"].as_str().unwrap_or(""));
    let mut res = reply(status, body);
    if status == StatusCode::CREATED {
        if let Ok(v) = location.parse() {
            res.headers_mut().insert(header::LOCATION, v);
        }
    }
    res
}

fn insert_version(
    conn: &Connection,
    p_id: i64,
    n: i64,
    v: &Validated,
    source: &str,
    note: Option<&str>,
    token_id: i64,
) -> ApiResult<()> {
    let ir = v.ir.as_ref().ok_or_else(|| ApiError::internal("versión válida sin IR"))?;
    conn.execute(
        "INSERT INTO versions (program_id, n, format, source, ir, sha256, capabilities, note, token_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            p_id,
            n,
            v.format.as_str(),
            source,
            ir.to_string(),
            v.sha256.to_vec(),
            v.capabilities.as_ref().unwrap_or(&J::Null).to_string(),
            note,
            token_id,
            now_iso()
        ],
    )?;
    Ok(())
}

// --- Validación previa ------------------------------------------------------

/// Valida en un hilo bloqueante; devuelve también la fuente para guardarla.
async fn validate_source(source: String, format: Option<String>) -> ApiResult<(Validated, String)> {
    tokio::task::spawn_blocking(move || validate::validate(&source, format.as_deref()).map(|v| (v, source)))
        .await
        .map_err(ApiError::internal)?
}

/// 422 con el informe si el programa no valida.
fn require_valid(v: &Validated) -> ApiResult<()> {
    if v.ok {
        return Ok(());
    }
    let n = v.report["errors"].as_array().map_or(0, |e| e.len());
    Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_program", format!("el programa no valida ({n} errores)"))
        .with_top("report", v.report.clone()))
}

fn check_note(note: &Option<String>) -> ApiResult<()> {
    match note {
        Some(n) if n.chars().count() > 500 => Err(ApiError::bad_request("note admite como máximo 500 caracteres")),
        _ => Ok(()),
    }
}

/// Avisa si el campo `program` de la IR no coincide con el nombre del programa.
fn warn_name_mismatch(v: &mut Validated, name: &str) {
    let declared = v.ir.as_ref().and_then(|ir| ir["program"].as_str()).unwrap_or("").to_string();
    if slugify(&declared) != name {
        add_warning(
            &mut v.report,
            format!("el programa se declara '{declared}' pero se guarda como '{name}'; su identidad es la URL"),
        );
    }
}

// --- Handlers ------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SourceBody {
    source: String,
    format: Option<String>,
}

/// `POST /check`: valida sin guardar.
pub async fn check(_: Caller, JsonBody(b): JsonBody<SourceBody>) -> ApiResult<Response> {
    let (v, _) = validate_source(b.source, b.format).await?;
    let mut report = v.report;
    if v.format == validate::Format::Stardust {
        if let Some(ir) = v.ir {
            report["ir"] = ir;
        }
    }
    Ok(reply(StatusCode::OK, report))
}

#[derive(Deserialize)]
pub struct InstallBody {
    name: Option<String>,
    source: String,
    format: Option<String>,
    visibility: Option<String>,
    note: Option<String>,
}

fn parse_visibility(v: Option<&str>) -> ApiResult<&'static str> {
    match v {
        None | Some("private") => Ok("private"),
        Some("link") => Ok("link"),
        Some(o) => Err(ApiError::bad_request(format!("visibility '{o}' desconocida: usa \"private\" o \"link\""))),
    }
}

/// `POST /programs`: installProgram.
pub async fn install(State(s): State<Shared>, caller: Caller, JsonBody(b): JsonBody<InstallBody>) -> ApiResult<Response> {
    let visibility = parse_visibility(b.visibility.as_deref())?;
    check_note(&b.note)?;
    if let Some(n) = &b.name {
        if !valid_name(n) {
            return Err(ApiError::bad_request(format!(
                "name '{n}' no válido: minúsculas, cifras y guiones; empieza por letra o cifra; máximo 40"
            )));
        }
    }
    let (mut v, source) = validate_source(b.source, b.format).await?;
    require_valid(&v)?;
    let name = match b.name {
        Some(n) => n,
        None => {
            let derived = slugify(v.ir.as_ref().and_then(|ir| ir["program"].as_str()).unwrap_or(""));
            if !valid_name(&derived) {
                return Err(ApiError::bad_request("no se pudo derivar un nombre del campo 'program': indica name"));
            }
            derived
        }
    };
    warn_name_mismatch(&mut v, &name);

    let origin = s.config.public_origin.clone();
    s.db
        .run(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(p) = find(&tx, &caller.handle, &name)? {
                let h = head(&tx, &p)?;
                if h.sha256 == v.sha256 && h.format == v.format.as_str() {
                    let prog = program_json(&tx, &origin, &p, false)?;
                    return Ok(change(StatusCode::OK, prog, Some(v.report), true, false));
                }
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "program_exists",
                    format!("ya existe '{}/{name}' (versión {}): para cambiarlo usa updateProgram", caller.handle, p.current),
                )
                .with("current_version", json!(p.current)));
            }
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM programs WHERE owner_id = ?1", [caller.user_id], |r| r.get(0))?;
            if count >= limits::PROGRAMS_PER_USER {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "quota_exceeded",
                    format!("máximo {} programas por usuario", limits::PROGRAMS_PER_USER),
                ));
            }
            let now = now_iso();
            tx.execute(
                "INSERT INTO programs (owner_id, name, visibility, current_version, created_at, updated_at) VALUES (?1, ?2, ?3, 1, ?4, ?4)",
                params![caller.user_id, name, visibility, now],
            )?;
            let pid = tx.last_insert_rowid();
            insert_version(&tx, pid, 1, &v, &source, b.note.as_deref(), caller.token_id)?;
            let p = find(&tx, &caller.handle, &name)?.ok_or_else(|| ApiError::internal("programa recién creado no encontrado"))?;
            let prog = program_json(&tx, &origin, &p, false)?;
            tx.commit()?;
            Ok(change(StatusCode::CREATED, prog, Some(v.report), false, false))
        })
        .await
}

#[derive(Deserialize)]
pub struct UpdateBody {
    base_version: Option<i64>,
    #[serde(default)]
    force: bool,
    source: String,
    format: Option<String>,
    note: Option<String>,
}

/// Comprueba la versión base: obligatoria salvo `force`.
fn check_base(base: Option<i64>, force: bool, current: i64) -> ApiResult<()> {
    if force {
        return Ok(());
    }
    match base {
        Some(b) if b == current => Ok(()),
        Some(b) => Err(ApiError::new(StatusCode::CONFLICT, "version_conflict", format!("la versión actual es {current}, no {b}"))
            .with("current_version", json!(current))),
        None => Err(ApiError::bad_request("base_version es obligatorio (o \"force\": true)")),
    }
}

/// `POST /programs/{owner}/{name}/versions`: updateProgram.
pub async fn update(
    State(s): State<Shared>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    JsonBody(b): JsonBody<UpdateBody>,
) -> ApiResult<Response> {
    if b.base_version.is_none() && !b.force {
        return Err(ApiError::bad_request("base_version es obligatorio (o \"force\": true)"));
    }
    check_note(&b.note)?;
    let (mut v, source) = validate_source(b.source, b.format).await?;
    require_valid(&v)?;
    warn_name_mismatch(&mut v, &name);

    let origin = s.config.public_origin.clone();
    s.db
        .run(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let p = owned(find(&tx, &owner, &name)?, &caller, &owner, &name)?;
            let h = head(&tx, &p)?;
            if h.sha256 == v.sha256 && h.format == v.format.as_str() {
                let prog = program_json(&tx, &origin, &p, false)?;
                return Ok(change(StatusCode::OK, prog, Some(v.report), true, false));
            }
            check_base(b.base_version, b.force, p.current)?;
            let n = p.current + 1;
            insert_version(&tx, p.id, n, &v, &source, b.note.as_deref(), caller.token_id)?;
            tx.execute(
                "UPDATE programs SET current_version = ?2, updated_at = ?3 WHERE id = ?1",
                params![p.id, n, now_iso()],
            )?;
            let caps_changed = v.capabilities.as_ref().is_some_and(|c| capabilities_grew(&h.capabilities, c));
            let p = find(&tx, &owner, &name)?.ok_or_else(|| ApiError::internal("programa desaparecido"))?;
            let prog = program_json(&tx, &origin, &p, false)?;
            tx.commit()?;
            Ok(change(StatusCode::CREATED, prog, Some(v.report), false, caps_changed))
        })
        .await
}

#[derive(Deserialize)]
pub struct RollbackBody {
    to_version: i64,
    base_version: Option<i64>,
    #[serde(default)]
    force: bool,
    note: Option<String>,
}

/// `POST /programs/{owner}/{name}/rollback`: copia una versión antigua como nueva.
pub async fn rollback(
    State(s): State<Shared>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    JsonBody(b): JsonBody<RollbackBody>,
) -> ApiResult<Response> {
    check_note(&b.note)?;
    let origin = s.config.public_origin.clone();
    s.db
        .run(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let p = owned(find(&tx, &owner, &name)?, &caller, &owner, &name)?;
            let target: Option<(Vec<u8>, String, String)> = tx
                .query_row(
                    "SELECT sha256, format, capabilities FROM versions WHERE program_id = ?1 AND n = ?2",
                    params![p.id, b.to_version],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let (sha, format, caps) =
                target.ok_or_else(|| ApiError::not_found(format!("no existe la versión {} de '{owner}/{name}'", b.to_version)))?;
            let h = head(&tx, &p)?;
            if h.sha256 == sha && h.format == format {
                let prog = program_json(&tx, &origin, &p, false)?;
                return Ok(change(StatusCode::OK, prog, None, true, false));
            }
            check_base(b.base_version, b.force, p.current)?;
            let n = p.current + 1;
            let note = b.note.unwrap_or_else(|| format!("vuelta a la versión {}", b.to_version));
            tx.execute(
                "INSERT INTO versions (program_id, n, format, source, ir, sha256, capabilities, note, token_id, created_at)
                 SELECT program_id, ?3, format, source, ir, sha256, capabilities, ?4, ?5, ?6
                 FROM versions WHERE program_id = ?1 AND n = ?2",
                params![p.id, b.to_version, n, note, caller.token_id, now_iso()],
            )?;
            tx.execute(
                "UPDATE programs SET current_version = ?2, updated_at = ?3 WHERE id = ?1",
                params![p.id, n, now_iso()],
            )?;
            let caps: J = serde_json::from_str(&caps).unwrap_or(J::Null);
            let caps_changed = capabilities_grew(&h.capabilities, &caps);
            let p = find(&tx, &owner, &name)?.ok_or_else(|| ApiError::internal("programa desaparecido"))?;
            let prog = program_json(&tx, &origin, &p, false)?;
            tx.commit()?;
            Ok(change(StatusCode::CREATED, prog, None, false, caps_changed))
        })
        .await
}

#[derive(Deserialize)]
pub struct Fields {
    fields: Option<String>,
}

/// `GET /programs/{owner}/{name}`: versión actual (con `?fields=meta`, sin fuente ni IR).
pub async fn get_program(
    State(s): State<Shared>,
    MaybeCaller(caller): MaybeCaller,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<Fields>,
) -> ApiResult<Response> {
    let full = q.fields.as_deref() != Some("meta");
    let origin = s.config.public_origin.clone();
    let body = s
        .db
        .run(move |conn| {
            let p = readable(find(conn, &owner, &name)?, caller.as_ref(), &owner, &name)?;
            Ok(program_json(conn, &origin, &p, full)?)
        })
        .await?;
    Ok(reply(StatusCode::OK, body))
}

/// `GET /programs/{owner}/{name}/versions/{n}`.
pub async fn get_version(
    State(s): State<Shared>,
    MaybeCaller(caller): MaybeCaller,
    Path((owner, name, n)): Path<(String, String, i64)>,
) -> ApiResult<Response> {
    let body = s
        .db
        .run(move |conn| {
            let p = readable(find(conn, &owner, &name)?, caller.as_ref(), &owner, &name)?;
            version_json(conn, p.id, n, true)?
                .ok_or_else(|| ApiError::not_found(format!("no existe la versión {n} de '{owner}/{name}'")))
        })
        .await?;
    Ok(reply(StatusCode::OK, body))
}

/// `GET /programs/{owner}/{name}/versions`: historial (sin fuente ni IR), la más reciente primero.
pub async fn list_versions(
    State(s): State<Shared>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    let body = s
        .db
        .run(move |conn| {
            let p = owned(find(conn, &owner, &name)?, &caller, &owner, &name)?;
            let ns: Vec<i64> = conn
                .prepare("SELECT n FROM versions WHERE program_id = ?1 ORDER BY n DESC")?
                .query_map([p.id], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            let versions = ns.into_iter().filter_map(|n| version_json(conn, p.id, n, false).transpose()).collect::<Result<Vec<_>, _>>()?;
            Ok(json!({ "current_version": p.current, "versions": versions }))
        })
        .await?;
    Ok(reply(StatusCode::OK, body))
}

#[derive(Deserialize)]
pub struct PatchBody {
    visibility: String,
}

/// `PATCH /programs/{owner}/{name}`: cambia la visibilidad.
pub async fn patch_program(
    State(s): State<Shared>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    JsonBody(b): JsonBody<PatchBody>,
) -> ApiResult<Response> {
    let visibility = parse_visibility(Some(&b.visibility))?;
    let origin = s.config.public_origin.clone();
    let body = s
        .db
        .run(move |conn| {
            let p = owned(find(conn, &owner, &name)?, &caller, &owner, &name)?;
            conn.execute(
                "UPDATE programs SET visibility = ?2, updated_at = ?3 WHERE id = ?1",
                params![p.id, visibility, now_iso()],
            )?;
            let p = find(conn, &owner, &name)?.ok_or_else(|| not_found(&owner, &name))?;
            Ok(program_json(conn, &origin, &p, false)?)
        })
        .await?;
    Ok(reply(StatusCode::OK, body))
}

/// `DELETE /programs/{owner}/{name}`: borra el programa con todas sus versiones.
pub async fn delete_program(
    State(s): State<Shared>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    s.db
        .run(move |conn| {
            let p = owned(find(conn, &owner, &name)?, &caller, &owner, &name)?;
            conn.execute("DELETE FROM programs WHERE id = ?1", [p.id])?;
            Ok(())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /programs`: mis programas (sin fuente ni IR), los modificados más recientemente primero.
pub async fn list_mine(State(s): State<Shared>, caller: Caller) -> ApiResult<Response> {
    let origin = s.config.public_origin.clone();
    let body = s
        .db
        .run(move |conn| {
            let names: Vec<String> = conn
                .prepare("SELECT name FROM programs WHERE owner_id = ?1 ORDER BY updated_at DESC, name")?
                .query_map([caller.user_id], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            let mut out = Vec::new();
            for n in names {
                if let Some(p) = find(conn, &caller.handle, &n)? {
                    out.push(program_json(conn, &origin, &p, false)?);
                }
            }
            Ok(json!({ "programs": out }))
        })
        .await?;
    Ok(reply(StatusCode::OK, body))
}

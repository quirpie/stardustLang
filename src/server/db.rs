//! SQLite: conexión única (un solo escritor), migraciones aditivas y acceso desde
//! handlers async vía `spawn_blocking`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use super::{ApiError, ApiResult};

/// Migraciones en orden. **Solo aditivas** (columnas o tablas nuevas): un rollback
/// de imagen debe poder seguir usando la base (ver `docs/deploy-fly.md`).
const MIGRATIONS: &[&str] = &[
    // 1: esquema inicial (docs/servidor.md, «Esquema de la base»).
    r#"
    CREATE TABLE users (
      id          INTEGER PRIMARY KEY,
      handle      TEXT NOT NULL UNIQUE,
      is_admin    INTEGER NOT NULL DEFAULT 0,
      disabled_at TEXT,
      created_at  TEXT NOT NULL
    );
    CREATE TABLE tokens (
      id           INTEGER PRIMARY KEY,
      user_id      INTEGER NOT NULL REFERENCES users(id),
      name         TEXT NOT NULL,
      prefix       TEXT NOT NULL,
      sha256       BLOB NOT NULL UNIQUE,
      created_at   TEXT NOT NULL,
      last_used_at TEXT,
      revoked_at   TEXT
    );
    CREATE TABLE sessions (
      sha256     BLOB PRIMARY KEY,
      user_id    INTEGER NOT NULL REFERENCES users(id),
      expires_at TEXT NOT NULL
    );
    CREATE TABLE programs (
      id              INTEGER PRIMARY KEY,
      owner_id        INTEGER NOT NULL REFERENCES users(id),
      name            TEXT NOT NULL,
      visibility      TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private','link')),
      current_version INTEGER NOT NULL,
      created_at      TEXT NOT NULL,
      updated_at      TEXT NOT NULL,
      UNIQUE (owner_id, name)
    );
    CREATE TABLE versions (
      program_id   INTEGER NOT NULL REFERENCES programs(id) ON DELETE CASCADE,
      n            INTEGER NOT NULL,
      format       TEXT NOT NULL CHECK (format IN ('stardust','json')),
      source       TEXT NOT NULL,
      ir           TEXT NOT NULL,
      sha256       BLOB NOT NULL,
      capabilities TEXT NOT NULL,
      note         TEXT,
      token_id     INTEGER REFERENCES tokens(id),
      created_at   TEXT NOT NULL,
      PRIMARY KEY (program_id, n)
    );
    "#,
    // 2: cada sesión web queda atada al token con el que se abrió (revocar el
    // token cierra la sesión, y las versiones guardadas desde el navegador dicen
    // con qué token se hicieron).
    "ALTER TABLE sessions ADD COLUMN token_id INTEGER REFERENCES tokens(id);",
];

/// Conexión compartida. SQLite admite un solo escritor: un `Mutex` basta y evita
/// `SQLITE_BUSY` dentro del proceso.
#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(path: &Path) -> Result<Db, String> {
        let conn = Connection::open(path).map_err(|e| format!("no se pudo abrir {}: {e}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000; PRAGMA synchronous = NORMAL;",
        )
        .map_err(|e| format!("pragmas: {e}"))?;
        migrate(&conn).map_err(|e| format!("migraciones: {e}"))?;
        Ok(Db(Arc::new(Mutex::new(conn))))
    }

    /// Ejecuta `f` con la conexión en un hilo bloqueante.
    pub async fn run<T, F>(&self, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> ApiResult<T> + Send + 'static,
    {
        let db = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = db.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut conn)
        })
        .await
        .map_err(ApiError::internal)?
    }

    /// Vuelca el WAL a la base principal (al parar).
    pub fn checkpoint(&self) {
        let conn = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
            super::log("warn", &format!("checkpoint del WAL: {e}"));
        }
    }

    /// Acceso síncrono, para los subcomandos de administración del binario.
    pub fn with_conn<T>(&self, f: impl FnOnce(&mut Connection) -> T) -> T {
        let mut conn = self.0.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut conn)
    }
}

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (v INTEGER NOT NULL);")?;
    let current: i64 = conn.query_row("SELECT COALESCE(MAX(v), 0) FROM schema_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let v = i as i64 + 1;
        if v > current {
            conn.execute_batch(&format!("BEGIN; {sql} INSERT INTO schema_version (v) VALUES ({v}); COMMIT;"))?;
            super::log("info", &format!("migración {v} aplicada"));
        }
    }
    Ok(())
}

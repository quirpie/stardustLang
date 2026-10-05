//! `stardust-server` — servidor de programas StardustLang (ver docs/servidor.md).
//!
//! Uso:
//!   stardust-server                         sirve la API (configuración por entorno)
//!   stardust-server user add <handle> [--admin]
//!   stardust-server user list
//!   stardust-server user token <handle> [--name <nombre>]
//!   stardust-server user disable <handle>
//!
//! Los subcomandos `user` actúan directamente sobre la base (`$STARDUST_DATA`); en
//! Fly se usan con `fly ssh console -C "stardust-server user …"`.

use std::process::ExitCode;

use stardust_vm::server::{self, auth, db::Db, Config};

const USAGE: &str = "uso: stardust-server [user add <handle> [--admin] | user list | user token <handle> [--name <n>] | user disable <handle>]

Variables de entorno: PORT (8080), STARDUST_DATA (./data), STARDUST_WEB (./web),
STARDUST_PUBLIC_ORIGIN, STARDUST_RUNNER_ORIGIN, STARDUST_ADMIN_TOKEN.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = Config::from_env();
    match args.first().map(String::as_str) {
        None | Some("serve") => serve(config),
        Some("user") => user(config, &args[1..]),
        Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("error: subcomando desconocido '{other}'\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn serve(config: Config) -> ExitCode {
    let state = match server::open_state(config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("runtime de tokio");
    let res = rt.block_on(async {
        let addr = format!("0.0.0.0:{}", state.config.port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        server::serve(listener, state, server::shutdown_signal()).await
    });
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn user(config: Config, args: &[String]) -> ExitCode {
    if let Err(e) = std::fs::create_dir_all(&config.data) {
        eprintln!("error: {e}");
        return ExitCode::from(1);
    }
    let db = match Db::open(&config.db_path()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let flag = |f: &str| args.iter().any(|a| a == f);
    let opt = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let handle = args.get(1).filter(|h| !h.starts_with("--")).cloned();

    let out = db.with_conn(|c| match (args.first().map(String::as_str), handle) {
        (Some("add"), Some(h)) => auth::create_user(c, &h, flag("--admin")).map(|v| {
            format!(
                "usuario '{h}' creado{}.\ntoken (se muestra una sola vez): {}",
                if flag("--admin") { " (admin)" } else { "" },
                v["token"].as_str().unwrap_or("")
            )
        }),
        (Some("list"), _) => auth::list_users(c).map_err(Into::into).map(|users| {
            let mut s = format!("{:<20} {:<6} {:>9} {:>7}  {}\n", "handle", "admin", "programas", "tokens", "deshabilitado");
            for u in users {
                s += &format!(
                    "{:<20} {:<6} {:>9} {:>7}  {}\n",
                    u["handle"].as_str().unwrap_or(""),
                    if u["is_admin"] == true { "sí" } else { "" },
                    u["programs"].as_i64().unwrap_or(0),
                    u["active_tokens"].as_i64().unwrap_or(0),
                    u["disabled_at"].as_str().unwrap_or("")
                );
            }
            s.trim_end().to_string()
        }),
        (Some("token"), Some(h)) => {
            let name = opt("--name").unwrap_or_else(|| "cli".into());
            auth::token_for(c, &h, &name).map(|t| format!("token '{name}' para '{h}' (se muestra una sola vez): {t}"))
        }
        (Some("disable"), Some(h)) => {
            auth::disable_user(c, &h).map(|_| format!("usuario '{h}' deshabilitado y sus tokens revocados"))
        }
        _ => Err(server::ApiError::bad_request(USAGE)),
    });
    db.checkpoint();
    match out {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {}", e.message);
            ExitCode::from(1)
        }
    }
}

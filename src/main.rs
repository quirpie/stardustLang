//! `stardust` — CLI del Nano POC: carga un programa StardustLang (JSON) y lo ejecuta
//! en la StardustVM.
//!
//! Uso:
//!   stardust <programa.stardust|programa.json> [--quiet]
//!
//!   --quiet   Silencia la traza de observabilidad de la VM (solo E/S del programa).
//!   --check   Solo valida (no ejecuta): imprime un informe JSON con errores
//!             localizados y pistas, pensado para generadores (LLMs).
//!   --emit-json  Compila un `.stardust` e imprime su IR (el JSON que ejecuta la VM).
//!
//! `.stardust` es la sintaxis de texto (formato principal, ver docs/sintaxis-texto.md);
//! `.json` es la IR directamente.
//!
//! Programas con UI (vista + capacidad RENDER) entran en un bucle interactivo:
//! se teclea la etiqueta del botón a pulsar; 'q' para salir.

use std::process::ExitCode;

use stardust_vm::program::Program;
use stardust_vm::vm::Vm;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut paths: Vec<String> = Vec::new();
    let mut verbose = true;
    let mut data_dir = String::from("stardust-data");
    let mut key: Option<String> = None;
    let mut net_timeout: Option<u64> = None;
    let mut net_max_body: Option<usize> = None;
    let mut serve_addr: Option<String> = None;
    let mut identity: Option<[u8; 32]> = None;
    let mut authorized: Vec<[u8; 32]> = Vec::new();
    let mut print_identity = false;
    let mut check_only = false;
    let mut emit_json = false;

    let mut it = args[1..].iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--quiet" | "-q" => verbose = false,
            "--data" => {
                data_dir = it.next().cloned().unwrap_or_default();
            }
            "--key" => {
                key = it.next().cloned();
            }
            "--net-timeout" => {
                net_timeout = it.next().and_then(|s| s.parse().ok());
            }
            "--net-max-body" => {
                net_max_body = it.next().and_then(|s| s.parse().ok());
            }
            "--serve" => {
                serve_addr = it.next().cloned();
            }
            "--identity" => {
                identity = it.next().and_then(|s| stardust_vm::wire::key32_from_hex(s));
            }
            "--authorize" => {
                if let Some(k) = it.next().and_then(|s| stardust_vm::wire::key32_from_hex(s)) {
                    authorized.push(k);
                }
            }
            "--print-identity" => print_identity = true,
            "--check" => check_only = true,
            "--emit-json" => emit_json = true,
            "--help" | "-h" => {
                eprintln!("uso: stardust <app.stardust|app.json> [<app2> ...] [--quiet] [--data <dir>] [--key <secreto>]");
                eprintln!("            [--net-timeout <segs>] [--net-max-body <bytes>] [--serve <host:puerto>]");
                eprintln!("  varias apps se hospedan juntas y se llaman vía 'app/actor'.");
                eprintln!("  --data <dir>   raíz del sandbox de ficheros (por defecto: stardust-data).");
                eprintln!("  --key <secreto> clave del host para SIGN/VERIFY (CRYPTO) y para firmar");
                eprintln!("                 los mensajes entre nodos (actor://); todos deben compartirla.");
                eprintln!("  --net-timeout <segs>   timeout por petición de red (capacidad NET; def. 30).");
                eprintln!("  --net-max-body <bytes> tope del cuerpo de respuesta de red (def. 16 MiB).");
                eprintln!("  --serve <host:puerto>  modo nodo: escucha mensajes de actores remotos (actor://).");
                eprintln!("  --identity <hex64>     semilla privada Ed25519 del nodo (firma el cable).");
                eprintln!("  --authorize <hex64>    clave pública autorizada a enviarnos (repetible; --serve).");
                eprintln!("  --print-identity       imprime la clave pública del nodo y sale.");
                eprintln!("  --check                valida sin ejecutar; informe JSON (salida 0 si es válido).");
                eprintln!("  --emit-json            compila un .stardust e imprime su IR (JSON).");
                return ExitCode::SUCCESS;
            }
            other => paths.push(other.to_string()),
        }
    }

    // Imprime la clave pública del nodo (para autorizarla en otro `--serve`) y sale.
    if print_identity {
        let seed = identity.unwrap_or(stardust_vm::wire::DEMO_SEED);
        println!("{}", stardust_vm::wire::public_key_hex(&seed));
        return ExitCode::SUCCESS;
    }

    if paths.is_empty() {
        eprintln!("error: falta la ruta del programa StardustLang (.json)");
        eprintln!("uso: stardust <app.json> [<app2.json> ...] [--quiet]");
        return ExitCode::from(2);
    }

    // Solo validar: un informe JSON por programa, sin ejecutar nada.
    if check_only {
        let mut all_ok = true;
        for path in &paths {
            let src = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: no se pudo leer '{path}': {e}");
                    return ExitCode::from(2);
                }
            };
            if is_text(path) {
                let mut report = stardust_vm::lang::check_source(&src);
                report.ir = None;
                all_ok &= report.ok;
                println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
            } else {
                let report = stardust_vm::check::check(&src);
                all_ok &= report.ok;
                println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
            }
        }
        return if all_ok { ExitCode::SUCCESS } else { ExitCode::from(1) };
    }

    let mut programs: Vec<Program> = Vec::new();
    for path in &paths {
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: no se pudo leer '{path}': {e}");
                return ExitCode::from(2);
            }
        };
        if is_text(path) {
            let compiled = match stardust_vm::lang::compile(&source) {
                Ok(c) => c,
                Err(errors) => {
                    for e in errors {
                        eprintln!("{path}:{e}");
                    }
                    return ExitCode::from(2);
                }
            };
            if emit_json {
                println!("{}", serde_json::to_string_pretty(&compiled.ir).unwrap_or_default());
                continue;
            }
            match compiled.program() {
                Ok(p) => programs.push(p),
                Err(e) => {
                    eprintln!("error interno: la IR generada para '{path}' no carga: {e}");
                    return ExitCode::from(2);
                }
            }
            continue;
        }
        if emit_json {
            eprintln!("error: --emit-json es para programas .stardust");
            return ExitCode::from(2);
        }
        match serde_json::from_str(&source) {
            Ok(p) => programs.push(p),
            Err(e) => {
                eprintln!("error: StardustLang inválido en '{path}': {e}");
                return ExitCode::from(2);
            }
        }
    }

    if emit_json {
        return ExitCode::SUCCESS;
    }

    // Los pánicos de los actores se capturan en la VM; silenciamos el hook por
    // defecto para que el aislamiento de fallos se vea limpio en consola.
    panic::silence_default_hook();

    let sandbox = if data_dir.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(&data_dir))
    };
    let mut machine = Vm::load_apps(programs, verbose)
        .with_sandbox(sandbox)
        .with_key(key.map(|k| k.into_bytes()).unwrap_or_default());
    if let Some(seed) = identity {
        machine = machine.with_identity(seed);
    }
    if !authorized.is_empty() {
        machine = machine.with_authorized(authorized);
    }
    // Solo se ajustan los límites de red si el usuario pasó alguna flag; si no,
    // se conserva el transporte HTTP por defecto (30 s / 16 MiB).
    if net_timeout.is_some() || net_max_body.is_some() {
        machine = machine.with_net_limits(
            std::time::Duration::from_secs(net_timeout.unwrap_or(30)),
            net_max_body.unwrap_or(16 * 1024 * 1024),
        );
    }

    // Modo nodo: escucha mensajes de actores remotos hasta Ctrl-C.
    if let Some(addr) = serve_addr {
        return match machine.serve(&addr) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: no se pudo escuchar en '{addr}': {e}");
                ExitCode::from(1)
            }
        };
    }

    let faults = machine.run();

    if faults == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// `.stardust` (o cualquier cosa que no sea `.json`) es la sintaxis de texto.
fn is_text(path: &str) -> bool {
    !path.to_ascii_lowercase().ends_with(".json")
}

mod panic {
    pub fn silence_default_hook() {
        std::panic::set_hook(Box::new(|_info| {}));
    }
}

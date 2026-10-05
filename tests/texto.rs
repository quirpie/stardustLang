//! La sintaxis de texto (`.stardust`) es equivalente a la IR: cada programa de
//! `programs/texto/` produce exactamente la misma salida que su `.json` original
//! al ejecutarlo con el binario `stardust` y la misma entrada.

use std::io::Write;
use std::process::{Command, Stdio};

/// Ejecuta `stardust <args> --quiet` con `stdin` y devuelve su salida estándar.
fn run(args: &[&str], stdin: &str, data: &std::path::Path) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_stardust"))
        .args(args)
        .arg("--quiet")
        .arg("--data")
        .arg(data)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("se ejecuta stardust");
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.stderr.is_empty() || out.status.success(),
        "{args:?} falló: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn sandbox(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let p = std::env::temp_dir().join(format!("stardust-texto-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Misma salida para el `.json` original y su versión `.stardust`.
fn same(json: &[&str], stardust: &[&str], stdin: &str) {
    let a = run(json, stdin, &sandbox("json"));
    let b = run(stardust, stdin, &sandbox("stardust"));
    assert!(!a.trim().is_empty(), "sin salida para {json:?}");
    assert_eq!(a, b, "\n--- {json:?}\n{a}\n--- {stardust:?}\n{b}");
}

#[test]
fn fibonacci() {
    same(&["programs/fibonacci.json"], &["programs/texto/fibonacci.stardust"], "10\n");
    same(&["programs/fibonacci.json"], &["programs/texto/fibonacci.stardust"], "1\n");
}

#[test]
fn calculadora() {
    let teclas = "7\n*\n8\n=\n+\n2\n=\nC\n9\n/\n0\n=\nq\n";
    same(&["programs/calculadora.json"], &["programs/texto/calculadora.stardust"], teclas);
}

#[test]
fn procedimientos_recursivos() {
    same(&["programs/procedures.json"], &["programs/texto/procedures.stardust"], "10\n");
}

#[test]
fn llamada_entre_apps() {
    same(
        &["programs/cliente.json", "programs/geometria.json"],
        &["programs/texto/cliente.stardust", "programs/texto/geometria.stardust"],
        "10\n8\n",
    );
}

#[test]
fn contador_persistente() {
    // Dos ejecuciones sobre el mismo sandbox: 1 y luego 2, igual en ambos formatos.
    for (prog, tag) in [("programs/contador.json", "cj"), ("programs/texto/contador.stardust", "ca")] {
        let dir = sandbox(tag);
        let first = run(&[prog], "", &dir);
        let second = run(&[prog], "", &dir);
        assert!(first.contains("Ejecuciones registradas: 1"), "{prog}: {first}");
        assert!(second.contains("Ejecuciones registradas: 2"), "{prog}: {second}");
    }
}

#[test]
fn errores_de_compilacion_con_linea() {
    let dir = sandbox("err");
    let bad = dir.join("malo.stardust");
    std::fs::write(&bad, "app malo\nactor A:\n  start:\n    x = parse(1)\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_stardust")).arg(&bad).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("línea 4") && err.contains("number(x)"), "{err}");
}

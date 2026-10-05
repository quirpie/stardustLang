//! Sintaxis de texto de StardustLang (`.stardust`): el formato principal para escribir
//! programas, a mano o con un LLM. Se compila a la IR (el JSON que ejecuta la
//! VM); ver `docs/sintaxis-texto.md`.
//!
//! ```text
//! texto ──lexer──▶ tokens ──parser──▶ AST ──lower──▶ IR (JSON) ──serde──▶ Program
//! ```

mod ast;
mod lexer;
mod lower;
mod parser;

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value as J;

/// Un error (o aviso) localizado en el texto fuente.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Diagnostic {
    pub line: usize,
    pub col: usize,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "línea {}, col {}: {}", self.line, self.col, self.message)?;
        if let Some(h) = &self.hint {
            write!(f, "\n    ej.: {}", h.replace('\n', "\n         "))?;
        }
        Ok(())
    }
}

/// Programa compilado: la IR y, para cada ruta de la IR (`actors[0].on_message.body[2]`),
/// la línea del texto de la que salió.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub ir: J,
    pub lines: BTreeMap<String, usize>,
}

impl Compiled {
    /// Línea de origen de una ruta de la IR (la del nodo más cercano que la tenga).
    pub fn line_of(&self, path: &str) -> Option<usize> {
        let mut p = path.to_string();
        loop {
            if let Some(l) = self.lines.get(&p) {
                return Some(*l);
            }
            let cut = p.rfind(['.', '['])?;
            p.truncate(cut);
        }
    }

    pub fn program(&self) -> Result<crate::program::Program, serde_json::Error> {
        serde_json::from_value(self.ir.clone())
    }
}

/// Compila texto `.stardust` a la IR.
pub fn compile(src: &str) -> Result<Compiled, Vec<Diagnostic>> {
    let toks = lexer::lex(src).map_err(|e| vec![e])?;
    let app = parser::Parser::new(toks).app().map_err(|e| vec![e])?;
    let mut ir = lower::lower(&app)?;
    let mut lines = BTreeMap::new();
    strip_lines(&mut ir, "", &mut lines);
    Ok(Compiled { ir, lines })
}

/// Quita las claves internas `__line` y apunta su ruta en el mapa de origen.
fn strip_lines(v: &mut J, path: &str, lines: &mut BTreeMap<String, usize>) {
    match v {
        J::Object(m) => {
            if let Some(l) = m.remove("__line").and_then(|l| l.as_u64()) {
                lines.insert(path.to_string(), l as usize);
            }
            for (k, child) in m.iter_mut() {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                strip_lines(child, &p, lines);
            }
        }
        J::Array(a) => {
            for (i, child) in a.iter_mut().enumerate() {
                strip_lines(child, &format!("{path}[{i}]"), lines);
            }
        }
        _ => {}
    }
}

/// Resultado de validar un `.stardust`: compilación + validación semántica de la IR
/// (`crate::check`), todo expresado en líneas del texto.
#[derive(Debug, Serialize)]
pub struct SourceReport {
    pub ok: bool,
    pub errors: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
    /// La IR, si compiló (aunque tenga errores semánticos).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ir: Option<J>,
}

pub fn check_source(src: &str) -> SourceReport {
    let compiled = match compile(src) {
        Ok(c) => c,
        Err(errors) => return SourceReport { ok: false, errors, warnings: vec![], ir: None },
    };
    let report = crate::check::check(&compiled.ir.to_string());
    let to_diag = |i: crate::check::Issue| Diagnostic {
        line: compiled.line_of(&i.path).unwrap_or(1),
        col: 1,
        message: source_message(&i.message),
        // Las pistas de check.rs son fragmentos de JSON: no sirven para el texto.
        hint: None,
    };
    let errors: Vec<Diagnostic> = report.errors.into_iter().map(to_diag).collect();
    let warnings: Vec<Diagnostic> = report.warnings.into_iter().map(to_diag).collect();
    SourceReport { ok: errors.is_empty(), errors, warnings, ir: Some(compiled.ir) }
}

/// Adapta los mensajes de check.rs (pensados para el JSON) a la sintaxis de texto.
fn source_message(m: &str) -> String {
    m.replace(
        "créala con DEF_VAR (p. ej. en on_start) o recíbela con on_message.bind",
        "asígnale un valor antes de usarla (p. ej. en start:) o recíbela en 'on m:'",
    )
    .replace("; recalcúlala con COMPARE al final del cuerpo", "; cámbiala dentro del bucle")
    .replace("añádela a sus capabilities", "añádela a 'uses'")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ir(src: &str) -> J {
        match compile(src) {
            Ok(c) => c.ir,
            Err(e) => panic!("{}", e.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n")),
        }
    }

    fn errs(src: &str) -> String {
        let r = check_source(src);
        r.errors.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn todos_los_ejemplos_compilan_y_validan() {
        for entry in std::fs::read_dir("programs/texto").unwrap() {
            let p = entry.unwrap().path();
            if p.extension().is_some_and(|e| e == "stardust") {
                let r = check_source(&std::fs::read_to_string(&p).unwrap());
                assert!(r.ok, "{}:\n{}", p.display(), r.errors.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n"));
                serde_json::from_value::<crate::program::Program>(r.ir.unwrap()).unwrap();
            }
        }
    }

    #[test]
    fn operadores_bajan_a_math_y_compare_sin_temporal_si_hay_destino() {
        let v = ir("app t\nactor A:\n  start:\n    r = 2 * 3 + 1\n    ok = r > 5\n");
        let start = &v["actors"][0]["on_start"];
        assert_eq!(start[0]["op"], "MATH"); // _1 = 2 * 3
        assert_eq!(start[1]["target"], "r"); // r = _1 + 1, directo
        assert_eq!(start[2]["op"], "COMPARE");
        assert_eq!(start[2]["target"], "ok");
    }

    #[test]
    fn capacidades_deducidas_y_uses_comprobado() {
        let v = ir("app t\nactor A:\n  start:\n    print('hola')\n    write('a.txt', 1)\n");
        assert_eq!(v["actors"][0]["capabilities"], serde_json::json!(["FILE", "IO_STREAM"]));
        let e = compile("app t\nactor A:\n  uses FILE\n  start:\n    print('hola')\n").unwrap_err();
        assert!(e[0].message.contains("IO_STREAM"), "{e:?}");
    }

    #[test]
    fn errores_con_linea() {
        let e = errs("app t\nactor A:\n  start:\n    x = 1\n  on m:\n    y = z + 1\n");
        assert!(e.contains("línea 6") && e.contains("'z' nunca se define"), "{e}");
        let e = compile("app t\nactor A:\n  def f(x):\n    return 1\n    y = 2\n").unwrap_err();
        assert!(e[0].message.contains("return") && e[0].line == 4, "{e:?}");
        let e = compile("app t\nactor A:\n  start:\n    x = parse(m)\n").unwrap_err();
        assert_eq!(e[0].hint.as_deref(), Some("number(x)"));
        let e = compile("app t\nactor A:\n  start:\n    x = 'hola\n").unwrap_err();
        assert!(e[0].message.contains("sin cerrar"), "{e:?}");
    }

    #[test]
    fn vista_derivada_y_for_desenrollado() {
        let v = ir("app t\nactor UI:\n  start:\n    xs = []\n  on m:\n    xs.append(m)\n  view:\n    column:\n      label(f'n: {len(xs)}')\n      for k in [1, 2]:\n        button(k, send=k)\n");
        let a = &v["actors"][0];
        assert_eq!(a["view"]["children"][0]["text"], serde_json::json!({"var":"_v1"}));
        assert_eq!(a["view"]["children"].as_array().unwrap().len(), 3);
        // _v1 se recalcula al final de start y de on.
        let last = |k: &str| a[k].as_array().map(|x| x.last().unwrap()["name"].clone()).unwrap_or_default();
        assert_eq!(last("on_start"), "_v1");
        assert_eq!(a["on_message"]["body"].as_array().unwrap().last().unwrap()["name"], "_v1");
    }
}

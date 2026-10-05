//! Única puerta de entrada de los programas: fuente → (IR, informe, capacidades).
//! Usa el mismo código que la VM (`lang::check_source` y `check::check`), así que un
//! programa que el servidor acepta, la VM lo carga.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{json, Value as J};

use super::{limits, ApiError, ApiResult};
use crate::{check, crypto, lang};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Stardust,
    Json,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Stardust => "stardust",
            Format::Json => "json",
        }
    }

    /// `format` explícito, o detección: JSON si empieza por `{` (como el CLI, que
    /// decide por la extensión).
    pub fn resolve(explicit: Option<&str>, source: &str) -> ApiResult<Format> {
        match explicit {
            Some("stardust") => Ok(Format::Stardust),
            Some("json") => Ok(Format::Json),
            Some(other) => Err(ApiError::bad_request(format!("format '{other}' desconocido: usa \"stardust\" o \"json\""))),
            None if source.trim_start().starts_with('{') => Ok(Format::Json),
            None => Ok(Format::Stardust),
        }
    }

    pub fn parse(s: &str) -> Format {
        if s == "json" { Format::Json } else { Format::Stardust }
    }
}

/// Resultado de validar una fuente.
pub struct Validated {
    pub format: Format,
    pub ok: bool,
    /// Informe con la forma de `docs/servidor.md` («Informe de validación»).
    pub report: J,
    /// La IR, si compiló (aunque tenga errores semánticos).
    pub ir: Option<J>,
    pub capabilities: Option<J>,
    pub sha256: [u8; 32],
}

/// Valida una fuente. Solo falla (`Err`) por límites o formato desconocido; un
/// programa inválido devuelve `Ok` con `ok: false`.
pub fn validate(source: &str, format: Option<&str>) -> ApiResult<Validated> {
    if source.len() > limits::SOURCE_BYTES {
        return Err(ApiError::new(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!("la fuente ocupa {} bytes; el máximo es {}", source.len(), limits::SOURCE_BYTES),
        ));
    }
    if source.trim().is_empty() {
        return Err(ApiError::bad_request("source está vacío"));
    }
    let format = Format::resolve(format, source)?;
    let (ok, errors, warnings, ir) = match format {
        Format::Stardust => {
            let r = lang::check_source(source);
            (r.ok, to_json(&r.errors), to_json(&r.warnings), r.ir)
        }
        Format::Json => {
            let r = check::check(source);
            // Si el informe no da errores, `check` ya comprobó que la VM lo carga.
            let ir = if r.ok { serde_json::from_str(source).ok() } else { None };
            (r.ok, to_json(&r.errors), to_json(&r.warnings), ir)
        }
    };
    let capabilities = ir.as_ref().map(capabilities_of);
    let mut report = json!({ "ok": ok, "errors": errors, "warnings": warnings });
    if let Some(c) = &capabilities {
        report["capabilities"] = c.clone();
    }
    Ok(Validated { format, ok, report, ir, capabilities, sha256: crypto::sha256(source.as_bytes()) })
}

fn to_json<T: Serialize>(v: &T) -> J {
    serde_json::to_value(v).unwrap_or(J::Array(vec![]))
}

/// Resumen de lo que pide el programa: capacidades por actor y allowlist de red.
pub fn capabilities_of(ir: &J) -> J {
    let mut actors = serde_json::Map::new();
    for a in ir["actors"].as_array().into_iter().flatten() {
        if let Some(name) = a["name"].as_str() {
            actors.insert(name.into(), a.get("capabilities").cloned().unwrap_or(json!([])));
        }
    }
    json!({ "actors": actors, "net_allow": ir.get("net_allow").cloned().unwrap_or(json!([])) })
}

/// ¿Pide `new` algo que `old` no pedía? (una capacidad o un prefijo de red nuevos).
pub fn capabilities_grew(old: &J, new: &J) -> bool {
    let caps = |c: &J| -> BTreeSet<String> {
        c["actors"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.values())
            .flat_map(|v| v.as_array().into_iter().flatten())
            .filter_map(|s| s.as_str().map(String::from))
            .collect()
    };
    let net = |c: &J| -> BTreeSet<String> {
        c["net_allow"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect()
    };
    !caps(new).is_subset(&caps(old)) || !net(new).is_subset(&net(old))
}

/// Añade un aviso sin posición al informe (p. ej. «program no coincide con name»).
pub fn add_warning(report: &mut J, message: String) {
    if let Some(w) = report["warnings"].as_array_mut() {
        w.push(json!({ "message": message }));
    }
}

// --- Nombres ----------------------------------------------------------------

/// `^[a-z0-9][a-z0-9-]{0,39}$`: sirve tal cual en rutas, URLs y nombres de bases
/// del navegador.
pub fn valid_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Nombre derivado del campo `program`: minúsculas, lo demás pasa a `-`.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').chars().take(40).collect::<String>().trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nombres() {
        assert!(valid_name("calculadora"));
        assert!(valid_name("app-2"));
        assert!(!valid_name("-app"));
        assert!(!valid_name("App"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(&"a".repeat(41)));
        assert_eq!(slugify("Mi Calculadora_2!"), "mi-calculadora-2");
        assert_eq!(slugify("  ¡hola!  "), "hola");
    }

    #[test]
    fn detecta_formato() {
        assert_eq!(Format::resolve(None, "  {\"program\":1}").unwrap(), Format::Json);
        assert_eq!(Format::resolve(None, "app x").unwrap(), Format::Stardust);
        assert!(Format::resolve(Some("xml"), "").is_err());
    }

    #[test]
    fn todos_los_ejemplos_validan() {
        for dir in ["programs", "programs/texto"] {
            for entry in std::fs::read_dir(dir).unwrap() {
                let p = entry.unwrap().path();
                let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
                if ext != "json" && ext != "stardust" {
                    continue;
                }
                let v = validate(&std::fs::read_to_string(&p).unwrap(), Some(ext)).unwrap();
                assert!(v.ok, "{}: {}", p.display(), v.report);
                assert!(v.ir.is_some() && v.capabilities.is_some(), "{}", p.display());
            }
        }
    }

    #[test]
    fn capacidades_que_crecen() {
        let a = json!({ "actors": { "A": ["RENDER"] }, "net_allow": [] });
        let b = json!({ "actors": { "A": ["RENDER", "NET"] }, "net_allow": [] });
        let c = json!({ "actors": { "A": ["RENDER"] }, "net_allow": ["https://x/"] });
        assert!(!capabilities_grew(&a, &a));
        assert!(capabilities_grew(&a, &b));
        assert!(capabilities_grew(&a, &c));
        assert!(!capabilities_grew(&b, &a));
    }
}

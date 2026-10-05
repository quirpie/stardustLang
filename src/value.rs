//! Sistema de tipos de StardustLang.
//!
//! Este módulo es el **único** lugar donde viven las reglas de tipo: promoción
//! numérica, aritmética, comparación, verdad/falsedad, parseo de entrada y
//! construcción de literales tipados. Añadir un tipo nuevo (p. ej. `Duration`,
//! `Money`) es un cambio localizado:
//!
//!   1. Añadir una variante a [`Value`].
//!   2. Cubrirla en `Display`, [`Value::is_truthy`] y [`parse_input`].
//!   3. Añadir sus casos en [`arith`] / [`order`] / [`values_equal`].
//!   4. (Opcional) Registrar su nombre en [`construct`] para literales tipados.
//!
//! Ver `date.rs` como ejemplo completo de un tipo no numérico.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use crate::crypto;
use crate::date::Date;
use crate::error::RuntimeError;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Date(Date),
    /// Estructura con campos nombrados: el payload de una "API" entre módulos.
    /// `BTreeMap` para orden determinista (observabilidad reproducible).
    Record(BTreeMap<String, Value>),
    /// Colección ordenada de longitud variable.
    List(Vec<Value>),
    /// Datos binarios (imágenes, blobs). Se transportan en base64.
    Bytes(Vec<u8>),
    Null,
}

impl Value {
    /// Bytes crudos de un valor: `Bytes` tal cual; el resto, su texto en UTF-8.
    /// Base de `HASH`/`SIGN` para que operen sobre el contenido real.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Value::Bytes(b) => b.clone(),
            other => other.to_string().into_bytes(),
        }
    }
}

impl Value {
    /// Interpretación de veracidad usada por `IF_COND` y `LOOP`.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Int(n) => *n != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Date(_) => true,
            Value::Record(m) => !m.is_empty(),
            Value::List(l) => !l.is_empty(),
            Value::Bytes(b) => !b.is_empty(),
            Value::Null => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Str(s) => write!(f, "{s}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Date(d) => write!(f, "{d}"),
            Value::Record(m) => {
                let fields: Vec<String> = m.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                write!(f, "{{{}}}", fields.join(", "))
            }
            Value::List(items) => {
                let parts: Vec<String> = items.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Value::Bytes(b) => write!(f, "<bytes:{}>", b.len()),
            Value::Null => write!(f, "null"),
        }
    }
}

/// Clasifica el texto de entrada del usuario (`IO_STREAM mode:"in"`) al tipo más
/// específico posible: entero > flotante > fecha > cadena.
pub fn parse_input(s: &str) -> Value {
    let t = s.trim();
    if let Ok(n) = t.parse::<i64>() {
        return Value::Int(n);
    }
    if let Ok(f) = t.parse::<f64>() {
        return Value::Float(f);
    }
    if let Some(d) = Date::parse(t) {
        return Value::Date(d);
    }
    Value::Str(t.to_string())
}

/// Construye un valor a partir de un literal tipado `{"lit": <json>, "as": "<tipo>"}`.
pub fn construct(type_name: &str, lit: &serde_json::Value) -> Result<Value, RuntimeError> {
    let bad = || RuntimeError::BadLiteral(format!("no es un '{type_name}' válido: {lit}"));
    match type_name {
        "int" => lit.as_i64().map(Value::Int).ok_or_else(bad),
        "float" => lit.as_f64().map(Value::Float).ok_or_else(bad),
        "string" => lit.as_str().map(|s| Value::Str(s.to_string())).ok_or_else(bad),
        "bool" => lit.as_bool().map(Value::Bool).ok_or_else(bad),
        "date" => lit
            .as_str()
            .and_then(Date::parse)
            .map(Value::Date)
            .ok_or_else(bad),
        other => Err(RuntimeError::UnknownType(other.to_string())),
    }
}

// --- Motor de operaciones (centralizado) -----------------------------------

/// Vista numérica unificada para promoción `Int`/`Float`.
enum Num {
    I(i64),
    F(f64),
}

fn to_num(v: &Value) -> Option<Num> {
    match v {
        Value::Int(n) => Some(Num::I(*n)),
        Value::Float(f) => Some(Num::F(*f)),
        _ => None,
    }
}

fn as_f64(n: &Num) -> f64 {
    match n {
        Num::I(i) => *i as f64,
        Num::F(f) => *f,
    }
}

/// Aritmética (`MATH`). Reglas, en orden de prioridad:
///   * `+` con alguna cadena  -> concatenación.
///   * fechas: `Date + Int`/`Int + Date` -> `Date`; `Date - Int` -> `Date`;
///     `Date - Date` -> `Int` (días).
///   * numérico: si algún operando es `Float`, el resultado es `Float`; si no, `Int`.
pub fn arith(op: &str, l: &Value, r: &Value) -> Result<Value, RuntimeError> {
    // 1. Concatenación de cadenas.
    if op == "+" && (matches!(l, Value::Str(_)) || matches!(r, Value::Str(_))) {
        return Ok(Value::Str(format!("{l}{r}")));
    }

    // 1b. Concatenación de listas: [1,2] + [3] -> [1,2,3].
    if op == "+" {
        if let (Value::List(a), Value::List(b)) = (l, r) {
            let mut items = a.clone();
            items.extend(b.iter().cloned());
            return Ok(Value::List(items));
        }
    }

    // 2. Aritmética temporal.
    match (l, r, op) {
        (Value::Date(a), Value::Int(n), "+") | (Value::Int(n), Value::Date(a), "+") => {
            return Ok(Value::Date(a.add_days(*n)))
        }
        (Value::Date(a), Value::Int(n), "-") => return Ok(Value::Date(a.add_days(-*n))),
        (Value::Date(a), Value::Date(b), "-") => return Ok(Value::Int(a.days_between(b))),
        (Value::Date(_), _, _) | (_, Value::Date(_), _) => {
            return Err(RuntimeError::TypeError(format!(
                "operación de fecha no soportada: {l} {op} {r}"
            )))
        }
        _ => {}
    }

    // 3. Numérico con promoción.
    match (to_num(l), to_num(r)) {
        (Some(a), Some(b)) => {
            let float_result = matches!(a, Num::F(_)) || matches!(b, Num::F(_));
            if float_result {
                let (x, y) = (as_f64(&a), as_f64(&b));
                let res = match op {
                    "+" => x + y,
                    "-" => x - y,
                    "*" => x * y,
                    "/" | "%" if y == 0.0 => return Err(RuntimeError::DivByZero),
                    "/" => x / y,
                    "%" => x % y,
                    _ => return Err(RuntimeError::BadOperator(op.to_string())),
                };
                Ok(Value::Float(res))
            } else {
                let (x, y) = (as_i64(&a), as_i64(&b));
                let res = match op {
                    "+" => x + y,
                    "-" => x - y,
                    "*" => x * y,
                    "/" | "%" if y == 0 => return Err(RuntimeError::DivByZero),
                    "/" => x / y,
                    "%" => x % y,
                    _ => return Err(RuntimeError::BadOperator(op.to_string())),
                };
                Ok(Value::Int(res))
            }
        }
        _ => Err(RuntimeError::TypeError(format!(
            "MATH '{op}' no soporta {l} y {r}"
        ))),
    }
}

fn as_i64(n: &Num) -> i64 {
    match n {
        Num::I(i) => *i,
        Num::F(f) => *f as i64,
    }
}

/// Comparación (`COMPARE`). `==`/`!=` admiten cualquier par (con promoción
/// numérica); los operadores de orden requieren tipos ordenables entre sí.
pub fn compare(op: &str, l: &Value, r: &Value) -> Result<bool, RuntimeError> {
    match op {
        "==" => Ok(values_equal(l, r)),
        "!=" => Ok(!values_equal(l, r)),
        "<" | ">" | "<=" | ">=" => {
            let ord = order(l, r).ok_or_else(|| {
                RuntimeError::TypeError(format!("COMPARE '{op}' no puede ordenar {l} y {r}"))
            })?;
            Ok(match op {
                "<" => ord == Ordering::Less,
                ">" => ord == Ordering::Greater,
                "<=" => ord != Ordering::Greater,
                ">=" => ord != Ordering::Less,
                _ => unreachable!(),
            })
        }
        _ => Err(RuntimeError::BadOperator(op.to_string())),
    }
}

pub fn values_equal(l: &Value, r: &Value) -> bool {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => (*a as f64) == *b,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Date(a), Value::Date(b)) => a == b,
        (Value::Record(a), Value::Record(b)) => a == b,
        (Value::List(a), Value::List(b)) => a == b,
        (Value::Bytes(a), Value::Bytes(b)) => a == b,
        (Value::Null, Value::Null) => true,
        _ => false,
    }
}

/// Valida un payload contra un esquema de contrato (campo -> nombre de tipo).
/// El payload debe ser un `Record` con todos los campos del tipo esperado.
/// Tipos: `int`, `float`, `number`, `string`, `bool`, `date`, `record`, `any`.
pub fn check_schema(
    payload: &Value,
    expects: &BTreeMap<String, String>,
) -> Result<(), RuntimeError> {
    let rec = match payload {
        Value::Record(m) => m,
        other => {
            return Err(RuntimeError::TypeError(format!(
                "el contrato espera un record, llegó {other}"
            )))
        }
    };
    for (field, ty) in expects {
        match rec.get(field) {
            None => {
                return Err(RuntimeError::TypeError(format!(
                    "el contrato requiere el campo '{field}' ({ty}), ausente"
                )))
            }
            Some(v) if !type_matches(v, ty) => {
                return Err(RuntimeError::TypeError(format!(
                    "campo '{field}': se esperaba {ty}, llegó {v}"
                )))
            }
            _ => {}
        }
    }
    Ok(())
}

fn type_matches(v: &Value, ty: &str) -> bool {
    match ty {
        "any" => true,
        "int" => matches!(v, Value::Int(_)),
        "float" => matches!(v, Value::Float(_) | Value::Int(_)),
        "number" => matches!(v, Value::Float(_) | Value::Int(_)),
        "string" => matches!(v, Value::Str(_)),
        "bool" => matches!(v, Value::Bool(_)),
        "date" => matches!(v, Value::Date(_)),
        "record" => matches!(v, Value::Record(_)),
        "list" => matches!(v, Value::List(_)),
        "bytes" => matches!(v, Value::Bytes(_)),
        _ => false,
    }
}

/// Serialización **etiquetada** y sin pérdidas: los tipos sin representación JSON
/// nativa (fechas, binarios) se codifican como `{"$type":"…","v":…}`, de modo que
/// round-trippean conservando su tipo (a diferencia de JSON plano, que perdería
/// una `Date` como texto). El resto usa JSON nativo.
pub fn to_tagged(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Int(n) => J::from(*n),
        Value::Float(f) => J::from(*f),
        Value::Str(s) => J::from(s.clone()),
        Value::Bool(b) => J::from(*b),
        Value::Null => J::Null,
        Value::Date(d) => serde_json::json!({ "$type": "date", "v": d.to_string() }),
        Value::Bytes(b) => serde_json::json!({ "$type": "bytes", "v": crypto::base64_encode(b) }),
        Value::List(items) => J::Array(items.iter().map(to_tagged).collect()),
        Value::Record(m) => J::Object(m.iter().map(|(k, v)| (k.clone(), to_tagged(v))).collect()),
    }
}

/// Inversa de [`to_tagged`]. Un objeto con `$type` reconocido reconstruye ese tipo;
/// el resto de objetos son records.
pub fn from_tagged(j: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match j {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0))),
        J::String(s) => Value::Str(s.clone()),
        J::Array(a) => Value::List(a.iter().map(from_tagged).collect()),
        J::Object(m) => {
            if let Some(J::String(t)) = m.get("$type") {
                let raw = m.get("v").and_then(|v| v.as_str());
                match t.as_str() {
                    "date" => {
                        return raw.and_then(Date::parse).map(Value::Date).unwrap_or(Value::Null)
                    }
                    "bytes" => {
                        return raw
                            .and_then(crypto::base64_decode)
                            .map(Value::Bytes)
                            .unwrap_or(Value::Null)
                    }
                    _ => {}
                }
            }
            Value::Record(m.iter().map(|(k, v)| (k.clone(), from_tagged(v))).collect())
        }
    }
}

/// Orden total parcial entre valores comparables. `None` = tipos no ordenables.
fn order(l: &Value, r: &Value) -> Option<Ordering> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
        (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
        (Value::Date(a), Value::Date(b)) => Some(a.to_days().cmp(&b.to_days())),
        (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

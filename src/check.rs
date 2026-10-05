//! Validador estático de StardustLang, pensado para generadores automáticos (LLMs).
//!
//! `serde` ya rechaza un programa mal formado, pero sus errores no sirven para que
//! un modelo pequeño se corrija: no dicen *dónde* (las expresiones son un enum
//! `untagged`: "data did not match any variant") y además ignora en silencio los
//! campos desconocidos (`"body"` en un `IF_COND` desaparece sin aviso). Este módulo
//! recorre el JSON crudo y devuelve errores con **ruta exacta**
//! (`actors[1].on_message.body[3].left`), un mensaje corto y una **pista** con la
//! forma correcta de esa instrucción/expresión/widget. Encima añade comprobaciones
//! semánticas baratas: actor `entry` inexistente, `SEND` a un actor que no existe,
//! capacidad no declarada, variable leída que nunca se define, `LOOP` cuya
//! condición no cambia, `CALL` a un procedimiento inexistente…
//!
//! Es común a los dos hosts: el CLI lo expone como `stardust --check` y el motor WASM
//! como `check_program()`. Si el recorrido no encuentra errores, el programa se
//! deserializa además con el esquema real ([`crate::program::Program`]), de modo que
//! "ok" significa "la StardustVM lo carga".

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Map, Value as J};

/// Un problema localizado en el programa.
#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    /// Ruta al nodo, p. ej. `actors[0].on_message.body[2].value`.
    pub path: String,
    pub message: String,
    /// La forma correcta (un fragmento JSON de ejemplo), si aplica.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Resultado de validar un programa.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub ok: bool,
    pub errors: Vec<Issue>,
    pub warnings: Vec<Issue>,
}

/// Capacidades que entiende la StardustVM.
pub const CAPABILITIES: &[&str] = &["IO_STREAM", "FILE", "CRYPTO", "NET", "RENDER"];

// --- Tablas del esquema -----------------------------------------------------

/// Tipo esperado de un campo.
#[derive(Clone, Copy, PartialEq)]
enum K {
    /// Nombre de variable que el nodo ESCRIBE (target, into, name…).
    Def,
    /// Texto libre (operator, mode, prompt, cap, to de widget…).
    Str,
    Expr,
    /// Lista de instrucciones.
    Block,
    /// Lista de expresiones.
    Exprs,
    Widget,
    Widgets,
    /// Entero positivo.
    Int,
    /// Cualquier JSON (el `lit` de un literal tipado).
    Any,
}

#[derive(Clone, Copy)]
struct F {
    key: &'static str,
    kind: K,
    required: bool,
}

const fn r(key: &'static str, kind: K) -> F {
    F { key, kind, required: true }
}
const fn o(key: &'static str, kind: K) -> F {
    F { key, kind, required: false }
}

/// Instrucciones: (op, campos, ejemplo). El ejemplo es la pista que ve el modelo.
const OPS: &[(&str, &[F], &str)] = &[
    ("DEF_VAR", &[r("name", K::Def), o("value", K::Expr)],
        r#"{"op":"DEF_VAR","name":"x","value":0}"#),
    ("ASSIGN", &[r("target", K::Def), r("value", K::Expr)],
        r#"{"op":"ASSIGN","target":"x","value":{"var":"y"}}"#),
    ("IF_COND", &[r("cond", K::Expr), o("then", K::Block), o("else", K::Block)],
        r#"{"op":"IF_COND","cond":{"var":"es_cero"},"then":[...],"else":[...]}"#),
    ("LOOP", &[r("cond", K::Expr), o("body", K::Block)],
        r#"{"op":"LOOP","cond":{"var":"seguir"},"body":[..., {"op":"COMPARE","target":"seguir",...}]}"#),
    ("COMPARE", &[r("target", K::Def), r("operator", K::Str), r("left", K::Expr), r("right", K::Expr)],
        r#"{"op":"COMPARE","target":"es_mayor","operator":">","left":{"var":"a"},"right":10}"#),
    ("MATH", &[r("target", K::Def), r("operator", K::Str), r("left", K::Expr), r("right", K::Expr)],
        r#"{"op":"MATH","target":"total","operator":"+","left":{"var":"total"},"right":1}"#),
    ("IO_STREAM", &[r("mode", K::Str), o("target", K::Def), o("value", K::Expr), o("prompt", K::Str)],
        r#"{"op":"IO_STREAM","mode":"out","value":{"var":"x"}}"#),
    ("FILE_WRITE", &[r("path", K::Expr), r("value", K::Expr)],
        r#"{"op":"FILE_WRITE","path":"notas.txt","value":{"var":"texto"}}"#),
    ("FILE_APPEND", &[r("path", K::Expr), r("value", K::Expr)],
        r#"{"op":"FILE_APPEND","path":"log.txt","value":{"var":"linea"}}"#),
    ("FILE_READ", &[r("path", K::Expr), r("into", K::Def), o("as", K::Str)],
        r#"{"op":"FILE_READ","path":"notas.txt","into":"texto"}"#),
    ("FILE_EXISTS", &[r("path", K::Expr), r("into", K::Def)],
        r#"{"op":"FILE_EXISTS","path":"notas.txt","into":"existe"}"#),
    ("FILE_LIST", &[o("path", K::Expr), r("into", K::Def)],
        r#"{"op":"FILE_LIST","into":"ficheros"}"#),
    ("FILE_DELETE", &[r("path", K::Expr)],
        r#"{"op":"FILE_DELETE","path":"notas.txt"}"#),
    ("HASH", &[r("value", K::Expr), r("into", K::Def)],
        r#"{"op":"HASH","value":{"var":"texto"},"into":"h"}"#),
    ("SIGN", &[r("value", K::Expr), r("into", K::Def)],
        r#"{"op":"SIGN","value":{"var":"texto"},"into":"firma"}"#),
    ("VERIFY", &[r("value", K::Expr), r("signature", K::Expr), r("into", K::Def)],
        r#"{"op":"VERIFY","value":{"var":"texto"},"signature":{"var":"firma"},"into":"valida"}"#),
    ("SERIALIZE", &[r("value", K::Expr), r("into", K::Def)],
        r#"{"op":"SERIALIZE","value":{"var":"datos"},"into":"json"}"#),
    ("DESERIALIZE", &[r("value", K::Expr), r("into", K::Def)],
        r#"{"op":"DESERIALIZE","value":{"var":"json"},"into":"datos"}"#),
    ("CALL", &[r("proc", K::Str), o("args", K::Exprs), o("into", K::Def)],
        r#"{"op":"CALL","proc":"doble","args":[{"var":"n"}],"into":"r"}"#),
    ("APPEND", &[r("target", K::Def), r("value", K::Expr)],
        r#"{"op":"APPEND","target":"items","value":{"var":"x"}}"#),
    ("FOREACH", &[r("in", K::Expr), r("var", K::Def), o("index", K::Def), o("body", K::Block)],
        r#"{"op":"FOREACH","in":{"var":"items"},"var":"x","body":[...]}"#),
    ("SEND", &[r("to", K::Expr), r("value", K::Expr), o("cap", K::Str)],
        r#"{"op":"SEND","to":"Actor_UI","value":{"var":"resultado"}}"#),
    ("NET_FETCH", &[r("url", K::Expr), o("method", K::Str), o("headers", K::Expr), o("body", K::Expr), o("tag", K::Expr)],
        r#"{"op":"NET_FETCH","method":"GET","url":"https://api.ejemplo.com/datos"}"#),
    ("SOCK_SEND", &[r("addr", K::Expr), r("body", K::Expr), o("tag", K::Expr)],
        r#"{"op":"SOCK_SEND","addr":"sock://127.0.0.1:7000","body":"ping"}"#),
];

/// Capacidad que exige cada instrucción.
fn op_cap(op: &str) -> Option<&'static str> {
    match op {
        "IO_STREAM" => Some("IO_STREAM"),
        "FILE_WRITE" | "FILE_APPEND" | "FILE_READ" | "FILE_EXISTS" | "FILE_LIST" | "FILE_DELETE" => Some("FILE"),
        "SIGN" | "VERIFY" => Some("CRYPTO"),
        "NET_FETCH" | "SOCK_SEND" => Some("NET"),
        _ => None,
    }
}

/// Expresiones con forma de objeto: (clave principal, campos, ejemplo). La clave
/// principal identifica la forma (`{"var":…}`, `{"field":…,"from":…}`…).
const EXPRS: &[(&str, &[F], &str)] = &[
    ("var", &[r("var", K::Str)], r#"{"var":"x"}"#),
    ("lit", &[r("lit", K::Any), r("as", K::Str)], r#"{"lit":"2026-12-31","as":"date"}"#),
    ("record", &[r("record", K::Any)], r#"{"record":{"nombre":{"var":"n"},"edad":30}}"#),
    ("field", &[r("field", K::Str), r("from", K::Expr)], r#"{"field":"nombre","from":{"var":"m"}}"#),
    ("list", &[r("list", K::Exprs)], r#"{"list":[1,2,3]}"#),
    ("at", &[r("at", K::Expr), r("of", K::Expr)], r#"{"at":0,"of":{"var":"items"}}"#),
    ("len", &[r("len", K::Expr)], r#"{"len":{"var":"items"}}"#),
    ("bytes", &[r("bytes", K::Str)], r#"{"bytes":"aG9sYQ=="}"#),
    ("to_bytes", &[r("to_bytes", K::Expr)], r#"{"to_bytes":{"var":"texto"}}"#),
    ("from_bytes", &[r("from_bytes", K::Expr)], r#"{"from_bytes":{"var":"datos"}}"#),
    ("base64", &[r("base64", K::Expr)], r#"{"base64":{"var":"datos"}}"#),
    ("split", &[r("split", K::Expr), r("on", K::Expr)], r#"{"split":{"var":"texto"},"on":","}"#),
    ("join", &[r("join", K::Expr), r("with", K::Expr)], r#"{"join":{"var":"items"},"with":", "}"#),
    ("slice", &[r("slice", K::Expr), r("from", K::Expr), o("to", K::Expr)], r#"{"slice":{"var":"t"},"from":0,"to":3}"#),
    ("replace", &[r("replace", K::Expr), r("find", K::Expr), r("with", K::Expr)], r#"{"replace":{"var":"t"},"find":"a","with":"b"}"#),
    ("contains", &[r("contains", K::Expr), r("sub", K::Expr)], r#"{"contains":{"var":"t"},"sub":"hola"}"#),
    ("starts_with", &[r("starts_with", K::Expr), r("prefix", K::Expr)], r##"{"starts_with":{"var":"t"},"prefix":"#"}"##),
    ("ends_with", &[r("ends_with", K::Expr), r("suffix", K::Expr)], r#"{"ends_with":{"var":"t"},"suffix":"."}"#),
    ("index_of", &[r("index_of", K::Expr), r("sub", K::Expr)], r#"{"index_of":{"var":"t"},"sub":"x"}"#),
    ("upper", &[r("upper", K::Expr)], r#"{"upper":{"var":"t"}}"#),
    ("lower", &[r("lower", K::Expr)], r#"{"lower":{"var":"t"}}"#),
    ("trim", &[r("trim", K::Expr)], r#"{"trim":{"var":"t"}}"#),
    ("repeat", &[r("repeat", K::Expr), r("times", K::Expr)], r#"{"repeat":"-","times":10}"#),
    ("to_str", &[r("to_str", K::Expr)], r#"{"to_str":{"var":"n"}}"#),
    ("parse", &[r("parse", K::Expr)], r#"{"parse":{"var":"m"}}"#),
];

/// Widgets de `view`: (type, campos, ejemplo).
const WIDGETS: &[(&str, &[F], &str)] = &[
    ("label", &[r("text", K::Expr)], r#"{"type":"label","text":{"var":"display"}}"#),
    ("button", &[r("label", K::Expr), r("send", K::Expr), o("to", K::Str)],
        r#"{"type":"button","label":"+1","send":"inc"}"#),
    ("row", &[o("children", K::Widgets)], r#"{"type":"row","children":[...]}"#),
    ("column", &[o("children", K::Widgets)], r#"{"type":"column","children":[...]}"#),
    ("grid", &[r("columns", K::Int), o("children", K::Widgets)], r#"{"type":"grid","columns":3,"children":[...]}"#),
    ("list", &[r("bind", K::Expr), r("as", K::Def), o("index", K::Def), r("item", K::Widget), o("empty", K::Widget)],
        r#"{"type":"list","bind":{"var":"items"},"as":"it","item":{"type":"label","text":{"var":"it"}}}"#),
    ("input", &[o("src", K::Expr), o("placeholder", K::Str), o("label", K::Str), r("submit", K::Expr), o("to", K::Str)],
        r#"{"type":"input","placeholder":"Escribe…","label":"Añadir","submit":{"var":"$input"}}"#),
    ("image", &[r("src", K::Expr), o("alt", K::Expr)], r#"{"type":"image","src":{"var":"foto"}}"#),
    ("filedrop", &[o("label", K::Str), r("drop", K::Expr), o("to", K::Str)],
        r#"{"type":"filedrop","drop":{"record":{"nombre":{"var":"$name"},"datos":{"var":"$bytes"}}}}"#),
    ("textarea", &[o("src", K::Expr), o("placeholder", K::Str), o("label", K::Str), r("submit", K::Expr), o("to", K::Str)],
        r#"{"type":"textarea","label":"Guardar","submit":{"var":"$input"}}"#),
    ("html", &[r("src", K::Expr)], r#"{"type":"html","src":{"var":"marcado"}}"#),
];

const PROGRAM_HINT: &str =
    r#"{"program":"nombre","entry":"Actor_UI","actors":[{"name":"Actor_UI","capabilities":["RENDER"],"on_start":[...],"on_message":{"bind":"m","body":[...]},"view":{...}}]}"#;
const ACTOR_KEYS: &[&str] = &["name", "capabilities", "on_start", "on_message", "procedures", "view"];
const ON_MESSAGE_HINT: &str = r#""on_message":{"bind":"m","body":[ ...instrucciones... ]}"#;

// --- Recorrido --------------------------------------------------------------

/// Lo que un actor (o un procedimiento) define y lee, para las comprobaciones
/// semánticas posteriores.
#[derive(Default)]
struct Scope {
    defs: BTreeSet<String>,
    reads: Vec<(String, String)>,
}

#[derive(Default)]
struct ActorFacts {
    name: String,
    path: String,
    caps: BTreeSet<String>,
    has_view: bool,
    /// (ruta, op) de instrucciones que requieren capacidad.
    cap_uses: Vec<(String, &'static str)>,
    /// (ruta, actor destino literal) de SEND y widgets con `to`.
    targets: Vec<(String, String)>,
    /// (ruta, url literal) de NET_FETCH / SOCK_SEND / SEND remoto.
    urls: Vec<(String, String)>,
    /// (ruta, proc, nº args).
    calls: Vec<(String, String, usize)>,
    procs: BTreeMap<String, usize>,
    scope: Scope,
    proc_scopes: Vec<(String, Scope)>,
}

struct Checker {
    errors: Vec<Issue>,
    warnings: Vec<Issue>,
}

impl Checker {
    fn err(&mut self, path: &str, message: impl Into<String>, hint: Option<&str>) {
        self.errors.push(Issue { path: path.to_string(), message: message.into(), hint: hint.map(str::to_string) });
    }
    fn warn(&mut self, path: &str, message: impl Into<String>, hint: Option<&str>) {
        self.warnings.push(Issue { path: path.to_string(), message: message.into(), hint: hint.map(str::to_string) });
    }

    /// Comprueba los campos de un objeto contra su tabla: requeridos presentes,
    /// tipos correctos y claves desconocidas (que serde ignoraría en silencio).
    fn fields(&mut self, obj: &Map<String, J>, path: &str, specs: &[F], skip: &str, hint: &str, scope: &mut Scope, facts: &mut ActorFacts) {
        for f in specs {
            let p = format!("{path}.{}", f.key);
            match obj.get(f.key) {
                None if f.required => self.err(path, format!("falta el campo obligatorio '{}'", f.key), Some(hint)),
                None => {}
                Some(v) => self.value(v, &p, f.kind, hint, scope, facts),
            }
        }
        for k in obj.keys() {
            if k == skip || specs.iter().any(|f| f.key == k) {
                continue;
            }
            let valid: Vec<&str> = specs.iter().map(|f| f.key).collect();
            let msg = match closest(k, &valid) {
                Some(s) => format!("campo desconocido '{k}' (¿quisiste decir '{s}'?)"),
                None => format!("campo desconocido '{k}'; los válidos son: {}", valid.join(", ")),
            };
            self.err(&format!("{path}.{k}"), msg, Some(hint));
        }
    }

    fn value(&mut self, v: &J, path: &str, kind: K, hint: &str, scope: &mut Scope, facts: &mut ActorFacts) {
        match kind {
            K::Def => match v.as_str() {
                Some(s) if !s.is_empty() => {
                    scope.defs.insert(s.to_string());
                }
                _ => self.err(path, "debe ser un nombre de variable (texto)", Some(hint)),
            },
            K::Str => {
                if !v.is_string() {
                    self.err(path, "debe ser texto", Some(hint));
                }
            }
            K::Int => {
                if !v.as_u64().is_some_and(|n| n > 0) {
                    self.err(path, "debe ser un entero positivo", Some(hint));
                }
            }
            K::Any => {}
            K::Expr => self.expr(v, path, scope, facts),
            K::Exprs => match v.as_array() {
                Some(a) => {
                    for (i, e) in a.iter().enumerate() {
                        self.expr(e, &format!("{path}[{i}]"), scope, facts);
                    }
                }
                None => self.err(path, "debe ser una lista de expresiones: [ ... ]", Some(hint)),
            },
            K::Block => self.block(v, path, scope, facts),
            K::Widget => self.widget(v, path, scope, facts),
            K::Widgets => match v.as_array() {
                Some(a) => {
                    for (i, w) in a.iter().enumerate() {
                        self.widget(w, &format!("{path}[{i}]"), scope, facts);
                    }
                }
                None => self.err(path, "debe ser una lista de widgets: [ ... ]", Some(hint)),
            },
        }
    }

    fn block(&mut self, v: &J, path: &str, scope: &mut Scope, facts: &mut ActorFacts) {
        match v {
            J::Array(items) => {
                for (i, ins) in items.iter().enumerate() {
                    self.instr(ins, &format!("{path}[{i}]"), scope, facts);
                }
            }
            J::Object(o) if o.contains_key("op") => {
                self.err(path, "debe ser una LISTA de instrucciones; envuelve la instrucción en [ ]", Some(r#"[ {"op":"..."} ]"#))
            }
            _ => self.err(path, "debe ser una lista de instrucciones: [ {\"op\":...}, ... ]", None),
        }
    }

    fn instr(&mut self, v: &J, path: &str, scope: &mut Scope, facts: &mut ActorFacts) {
        let Some(obj) = v.as_object() else {
            return self.err(path, "una instrucción debe ser un objeto {\"op\":...}", None);
        };
        let Some(op) = obj.get("op").and_then(J::as_str) else {
            return self.err(path, "falta \"op\" (el nombre de la instrucción, p. ej. \"DEF_VAR\")", Some(OPS[0].2));
        };
        let Some(&(_, specs, hint)) = OPS.iter().find(|(name, ..)| *name == op) else {
            let names: Vec<&str> = OPS.iter().map(|(n, ..)| *n).collect();
            let msg = match closest(op, &names) {
                Some(s) => format!("instrucción desconocida '{op}' (¿quisiste decir '{s}'?)"),
                None => format!("instrucción desconocida '{op}'; las válidas son: {}", names.join(", ")),
            };
            return self.err(&format!("{path}.op"), msg, None);
        };

        // Confusiones típicas, con su propio mensaje.
        if op == "IF_COND" && obj.contains_key("body") {
            self.err(&format!("{path}.body"), "IF_COND usa \"then\" y \"else\", no \"body\"", Some(hint));
        }
        if matches!(op, "LOOP" | "FOREACH") && (obj.contains_key("then") || obj.contains_key("do")) {
            self.err(path, format!("{op} usa \"body\" para sus instrucciones"), Some(hint));
        }
        let skip_body = op == "IF_COND" && obj.contains_key("body");
        let mut filtered;
        let obj = if skip_body {
            filtered = obj.clone();
            filtered.remove("body");
            &filtered
        } else {
            obj
        };

        self.fields(obj, path, specs, "op", hint, scope, facts);

        if let Some(cap) = op_cap(op) {
            facts.cap_uses.push((path.to_string(), cap));
        }
        match op {
            "COMPARE" => self.operator(obj, path, &["==", "!=", "<", ">", "<=", ">="], hint),
            "MATH" => self.operator(obj, path, &["+", "-", "*", "/", "%"], hint),
            "IO_STREAM" => {
                let mode = obj.get("mode").and_then(J::as_str).unwrap_or("");
                if !matches!(mode, "in" | "out") {
                    self.err(&format!("{path}.mode"), "mode debe ser \"in\" o \"out\"", Some(hint));
                }
                if mode == "in" && !obj.contains_key("target") {
                    self.err(path, "IO_STREAM \"in\" necesita \"target\" (dónde guardar lo leído)", Some(hint));
                }
                if mode == "out" && !obj.contains_key("value") && !obj.contains_key("prompt") {
                    self.err(path, "IO_STREAM \"out\" necesita \"value\" o \"prompt\" (qué mostrar)", Some(hint));
                }
            }
            "IF_COND" | "LOOP" => {
                if let Some(c) = obj.get("cond") {
                    if c.is_number() || c.is_string() {
                        self.warn(&format!("{path}.cond"), "cond debería ser un booleano, normalmente {\"var\":...} producido por COMPARE", Some(hint));
                    }
                }
            }
            "SEND" => match obj.get("to") {
                Some(J::String(to)) if to.contains("://") => facts.urls.push((format!("{path}.to"), to.clone())),
                Some(J::String(to)) if !to.contains('/') => facts.targets.push((format!("{path}.to"), to.clone())),
                _ => {}
            },
            "NET_FETCH" => {
                if let Some(J::String(u)) = obj.get("url") {
                    facts.urls.push((format!("{path}.url"), u.clone()));
                }
            }
            "SOCK_SEND" => {
                if let Some(J::String(a)) = obj.get("addr") {
                    facts.urls.push((format!("{path}.addr"), a.clone()));
                }
            }
            "CALL" => {
                if let Some(p) = obj.get("proc").and_then(J::as_str) {
                    let argc = obj.get("args").and_then(J::as_array).map_or(0, Vec::len);
                    facts.calls.push((path.to_string(), p.to_string(), argc));
                }
            }
            _ => {}
        }

        // Un LOOP cuya variable de condición nunca cambia dentro del cuerpo es un
        // bucle infinito (la VM lo cortaría, pero mejor avisar antes).
        if op == "LOOP" {
            match obj.get("cond") {
                Some(J::Bool(true)) => self.err(&format!("{path}.cond"), "cond: true es un bucle infinito", Some(hint)),
                Some(J::Object(c)) => {
                    if let Some(var) = c.get("var").and_then(J::as_str) {
                        let mut writes = BTreeSet::new();
                        if let Some(body) = obj.get("body") {
                            collect_writes(body, &mut writes);
                        }
                        if !writes.contains(var) {
                            self.err(
                                &format!("{path}.cond"),
                                format!("bucle infinito: '{var}' no se actualiza dentro de \"body\"; recalcúlala con COMPARE al final del cuerpo"),
                                Some(hint),
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn operator(&mut self, obj: &Map<String, J>, path: &str, valid: &[&str], hint: &str) {
        if let Some(op) = obj.get("operator").and_then(J::as_str) {
            if !valid.contains(&op) {
                self.err(&format!("{path}.operator"), format!("operador '{op}' inválido; usa uno de: {}", valid.join(" ")), Some(hint));
            }
        }
    }

    fn expr(&mut self, v: &J, path: &str, scope: &mut Scope, facts: &mut ActorFacts) {
        match v {
            J::Number(_) | J::Bool(_) | J::String(_) => {}
            J::Null => self.err(path, "null no es una expresión válida; usa \"\", 0 o false", None),
            J::Array(_) => self.err(path, "una lista se escribe {\"list\":[ ... ]}, no [ ... ]", Some(r#"{"list":[1,2,3]}"#)),
            J::Object(obj) => {
                if obj.contains_key("op") {
                    return self.err(path, "aquí va una expresión, no una instrucción; calcula antes en una variable y usa {\"var\":...}", None);
                }
                if obj.contains_key("type") {
                    return self.err(path, "aquí va una expresión, no un widget", None);
                }
                let Some(&(head, specs, hint)) = EXPRS.iter().find(|(h, ..)| obj.contains_key(*h)) else {
                    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
                    let heads: Vec<&str> = EXPRS.iter().map(|(h, ..)| *h).collect();
                    let msg = match keys.iter().find_map(|k| expr_alias(k).or_else(|| closest(k, &heads))) {
                        Some(s) => format!("expresión desconocida con claves {keys:?} (¿quisiste decir '{s}'?)"),
                        None => format!("expresión desconocida con claves {keys:?}; las más comunes son {{\"var\":...}}, {{\"field\":...,\"from\":...}}, {{\"list\":[...]}}, {{\"record\":{{...}}}}"),
                    };
                    return self.err(path, msg, Some(r#"{"var":"x"}"#));
                };
                self.fields(obj, path, specs, "", hint, scope, facts);
                match head {
                    "var" => {
                        if let Some(name) = obj.get("var").and_then(J::as_str) {
                            scope.reads.push((format!("{path}.var"), name.to_string()));
                        }
                    }
                    "lit" => {
                        let t = obj.get("as").and_then(J::as_str).unwrap_or("");
                        if !matches!(t, "int" | "float" | "string" | "bool" | "date") {
                            self.err(&format!("{path}.as"), "as debe ser int, float, string, bool o date", Some(hint));
                        }
                    }
                    "record" => match obj.get("record") {
                        Some(J::Object(fields)) => {
                            for (k, e) in fields {
                                self.expr(e, &format!("{path}.record.{k}"), scope, facts);
                            }
                        }
                        _ => self.err(&format!("{path}.record"), "record debe ser un objeto {\"campo\": <expr>}", Some(hint)),
                    },
                    _ => {}
                }
            }
        }
    }

    fn widget(&mut self, v: &J, path: &str, scope: &mut Scope, facts: &mut ActorFacts) {
        let Some(obj) = v.as_object() else {
            return self.err(path, "un widget debe ser un objeto {\"type\":...}", Some(WIDGETS[0].2));
        };
        let Some(ty) = obj.get("type").and_then(J::as_str) else {
            return self.err(path, "falta \"type\" del widget (label, button, row, column, grid, list, input…)", Some(WIDGETS[0].2));
        };
        let Some(&(_, specs, hint)) = WIDGETS.iter().find(|(name, ..)| *name == ty) else {
            let names: Vec<&str> = WIDGETS.iter().map(|(n, ..)| *n).collect();
            let msg = match closest(ty, &names) {
                Some(s) => format!("widget desconocido '{ty}' (¿quisiste decir '{s}'?)"),
                None => format!("widget desconocido '{ty}'; los válidos son: {}", names.join(", ")),
            };
            return self.err(&format!("{path}.type"), msg, None);
        };
        self.fields(obj, path, specs, "type", hint, scope, facts);
        // Los marcadores de host ($input, $name, $bytes) solo se rellenan si van
        // directos o dentro de record/list; cualquier otra expresión se evalúa
        // contra la memoria, donde no existen.
        for k in ["send", "submit", "drop"] {
            if let Some(e) = obj.get(k) {
                if let Some(bad) = host_var_misuse(e) {
                    self.err(
                        &format!("{path}.{k}"),
                        format!("'{bad}' solo puede ir tal cual ({{\"var\":\"{bad}\"}}) o dentro de record/list; conviértelo después, en on_message (p. ej. {{\"parse\":{{\"var\":\"m\"}}}})"),
                        Some(hint),
                    );
                }
            }
        }
        if let Some(J::String(to)) = obj.get("to") {
            facts.targets.push((format!("{path}.to"), to.clone()));
        }
    }

    fn actor(&mut self, v: &J, path: &str) -> Option<ActorFacts> {
        let Some(obj) = v.as_object() else {
            self.err(path, "cada actor debe ser un objeto {\"name\":...}", None);
            return None;
        };
        let mut facts = ActorFacts { path: path.to_string(), ..Default::default() };
        match obj.get("name").and_then(J::as_str) {
            Some(n) if !n.is_empty() => facts.name = n.to_string(),
            _ => self.err(path, "falta \"name\" del actor (texto)", None),
        }
        for k in obj.keys() {
            if !ACTOR_KEYS.contains(&k.as_str()) {
                let msg = match closest(k, ACTOR_KEYS) {
                    Some(s) => format!("campo desconocido '{k}' en el actor (¿quisiste decir '{s}'?)"),
                    None => format!("campo desconocido '{k}' en el actor; los válidos son: {}", ACTOR_KEYS.join(", ")),
                };
                self.err(&format!("{path}.{k}"), msg, None);
            }
        }
        match obj.get("capabilities") {
            None => {}
            Some(J::Array(caps)) => {
                for (i, c) in caps.iter().enumerate() {
                    match c.as_str() {
                        Some(c) if CAPABILITIES.contains(&c) => {
                            facts.caps.insert(c.to_string());
                        }
                        _ => self.err(
                            &format!("{path}.capabilities[{i}]"),
                            format!("capacidad desconocida {c}; las válidas son: {}", CAPABILITIES.join(", ")),
                            None,
                        ),
                    }
                }
            }
            Some(_) => self.err(&format!("{path}.capabilities"), "capabilities debe ser una lista, p. ej. [\"RENDER\"]", None),
        }

        // Procedimientos: scope aislado (parámetros + lo que definen).
        match obj.get("procedures") {
            None => {}
            Some(J::Object(procs)) => {
                for (pname, p) in procs {
                    let pp = format!("{path}.procedures.{pname}");
                    let mut ps = Scope::default();
                    let mut argc = 0;
                    match p.as_object() {
                        Some(po) => {
                            for k in po.keys() {
                                if !matches!(k.as_str(), "params" | "returns" | "body") {
                                    self.err(&format!("{pp}.{k}"), format!("campo desconocido '{k}'; un procedimiento tiene params, returns y body"), None);
                                }
                            }
                            if let Some(params) = po.get("params") {
                                match params.as_array() {
                                    Some(a) => {
                                        argc = a.len();
                                        for x in a {
                                            match x.as_str() {
                                                Some(s) => {
                                                    ps.defs.insert(s.to_string());
                                                }
                                                None => self.err(&format!("{pp}.params"), "params es una lista de nombres", None),
                                            }
                                        }
                                    }
                                    None => self.err(&format!("{pp}.params"), "params es una lista de nombres, p. ej. [\"n\"]", None),
                                }
                            }
                            match po.get("body") {
                                Some(b) => self.block(b, &format!("{pp}.body"), &mut ps, &mut facts),
                                None => self.err(&pp, "falta \"body\" del procedimiento", None),
                            }
                            if let Some(ret) = po.get("returns") {
                                match ret.as_str() {
                                    Some(rv) => ps.reads.push((format!("{pp}.returns"), rv.to_string())),
                                    None => self.err(&format!("{pp}.returns"), "returns es el nombre de la variable a devolver", None),
                                }
                            }
                        }
                        None => self.err(&pp, "un procedimiento es {\"params\":[...],\"returns\":\"r\",\"body\":[...]}", None),
                    }
                    facts.procs.insert(pname.clone(), argc);
                    facts.proc_scopes.push((pp, ps));
                }
            }
            Some(_) => self.err(&format!("{path}.procedures"), "procedures es un objeto {\"nombre\":{\"params\":[...],\"body\":[...]}}", None),
        }

        let mut scope = Scope::default();
        if let Some(b) = obj.get("on_start") {
            self.block(b, &format!("{path}.on_start"), &mut scope, &mut facts);
        }
        match obj.get("on_message") {
            None => {}
            Some(J::Object(om)) => {
                let mp = format!("{path}.on_message");
                for k in om.keys() {
                    if !matches!(k.as_str(), "bind" | "reply_to" | "expects" | "body") {
                        let msg = match closest(k, &["bind", "reply_to", "expects", "body"]) {
                            Some(s) => format!("campo desconocido '{k}' (¿quisiste decir '{s}'?)"),
                            None => format!("campo desconocido '{k}'; on_message tiene bind, reply_to, expects y body"),
                        };
                        self.err(&format!("{mp}.{k}"), msg, Some(ON_MESSAGE_HINT));
                    }
                }
                for k in ["bind", "reply_to"] {
                    match om.get(k) {
                        None => {}
                        Some(J::String(s)) => {
                            scope.defs.insert(s.clone());
                        }
                        Some(_) => self.err(&format!("{mp}.{k}"), format!("{k} es el nombre de una variable (texto)"), Some(ON_MESSAGE_HINT)),
                    }
                }
                if let Some(ex) = om.get("expects") {
                    if !ex.as_object().is_some_and(|m| m.values().all(J::is_string)) {
                        self.err(&format!("{mp}.expects"), "expects es un objeto {\"campo\":\"tipo\"}", None);
                    }
                }
                match om.get("body") {
                    Some(b) => self.block(b, &format!("{mp}.body"), &mut scope, &mut facts),
                    None => self.err(&mp, "falta \"body\" en on_message", Some(ON_MESSAGE_HINT)),
                }
            }
            Some(J::Array(_)) => self.err(
                &format!("{path}.on_message"),
                "on_message es un OBJETO con bind y body, no una lista",
                Some(ON_MESSAGE_HINT),
            ),
            Some(_) => self.err(&format!("{path}.on_message"), "on_message debe ser un objeto", Some(ON_MESSAGE_HINT)),
        }
        if let Some(view) = obj.get("view") {
            facts.has_view = true;
            self.widget(view, &format!("{path}.view"), &mut scope, &mut facts);
        }
        facts.scope = scope;
        Some(facts)
    }

    fn program(&mut self, root: &J) -> Vec<ActorFacts> {
        let Some(obj) = root.as_object() else {
            self.err("$", "el programa debe ser un objeto JSON {...}", Some(PROGRAM_HINT));
            return vec![];
        };
        for k in ["program", "entry"] {
            match obj.get(k) {
                Some(J::String(s)) if !s.is_empty() => {}
                Some(_) => self.err(k, format!("\"{k}\" debe ser texto"), Some(PROGRAM_HINT)),
                None => self.err("$", format!("falta el campo obligatorio \"{k}\""), Some(PROGRAM_HINT)),
            }
        }
        let top = ["program", "entry", "actors", "exports", "grants", "net_allow"];
        for k in obj.keys() {
            if !top.contains(&k.as_str()) {
                let msg = match closest(k, &top) {
                    Some(s) => format!("campo desconocido '{k}' (¿quisiste decir '{s}'?)"),
                    None => format!("campo desconocido '{k}' a nivel raíz; los válidos son: {}", top.join(", ")),
                };
                self.err(k, msg, Some(PROGRAM_HINT));
            }
        }
        for k in ["exports", "net_allow"] {
            if let Some(v) = obj.get(k) {
                if !v.as_array().is_some_and(|a| a.iter().all(J::is_string)) {
                    self.err(k, format!("\"{k}\" debe ser una lista de textos"), None);
                }
            }
        }
        let actors = match obj.get("actors") {
            Some(J::Array(a)) if !a.is_empty() => a,
            Some(J::Array(_)) => {
                self.err("actors", "el programa necesita al menos un actor", Some(PROGRAM_HINT));
                return vec![];
            }
            Some(_) => {
                self.err("actors", "\"actors\" debe ser una lista [ {actor}, ... ]", Some(PROGRAM_HINT));
                return vec![];
            }
            None => {
                self.err("$", "falta el campo obligatorio \"actors\"", Some(PROGRAM_HINT));
                return vec![];
            }
        };
        actors
            .iter()
            .enumerate()
            .filter_map(|(i, a)| self.actor(a, &format!("actors[{i}]")))
            .collect()
    }

    /// Comprobaciones entre actores, una vez recorrido todo.
    fn semantics(&mut self, root: &J, facts: &[ActorFacts]) {
        let names: Vec<&str> = facts.iter().map(|f| f.name.as_str()).collect();
        let mut seen = BTreeSet::new();
        for f in facts {
            if !f.name.is_empty() && !seen.insert(f.name.as_str()) {
                self.err(&f.path, format!("actor duplicado '{}'", f.name), None);
            }
        }
        if let Some(entry) = root.get("entry").and_then(J::as_str) {
            if !names.contains(&entry) {
                let msg = match closest(entry, &names) {
                    Some(s) => format!("entry '{entry}' no es ningún actor (¿quisiste decir '{s}'?)"),
                    None => format!("entry '{entry}' no es ningún actor; los actores son: {}", names.join(", ")),
                };
                self.err("entry", msg, None);
            }
        }
        let net_allow: Vec<String> = root
            .get("net_allow")
            .and_then(J::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();

        let mut ui_actors = 0;
        for f in facts {
            for (path, cap) in &f.cap_uses {
                if !f.caps.contains(*cap) {
                    self.err(path, format!("el actor '{}' usa esta instrucción sin la capacidad \"{cap}\"; añádela a sus capabilities", f.name), None);
                }
            }
            if f.has_view {
                if f.caps.contains("RENDER") {
                    ui_actors += 1;
                } else {
                    self.err(&format!("{}.view", f.path), format!("el actor '{}' tiene view pero no la capacidad \"RENDER\": la vista no se mostraría", f.name), None);
                }
            } else if f.caps.contains("RENDER") {
                self.warn(&f.path, format!("el actor '{}' tiene \"RENDER\" pero no declara view", f.name), None);
            }
            for (path, to) in &f.targets {
                if !names.contains(&to.as_str()) {
                    let msg = match closest(to, &names) {
                        Some(s) => format!("el actor destino '{to}' no existe (¿quisiste decir '{s}'?)"),
                        None => format!("el actor destino '{to}' no existe; los actores son: {}", names.join(", ")),
                    };
                    self.err(path, msg, None);
                }
            }
            for (path, url) in &f.urls {
                if !net_allow.iter().any(|p| url.starts_with(p.as_str())) {
                    self.err(path, format!("'{url}' no está en \"net_allow\"; añade su prefijo a la lista net_allow del programa"), None);
                }
                if !f.caps.contains("NET") {
                    self.err(path, format!("el actor '{}' necesita la capacidad \"NET\" para salir a la red", f.name), None);
                }
            }
            for (path, p, argc) in &f.calls {
                match f.procs.get(p) {
                    None => {
                        let procs: Vec<&str> = f.procs.keys().map(String::as_str).collect();
                        let msg = match closest(p, &procs) {
                            Some(s) => format!("procedimiento '{p}' no existe en el actor '{}' (¿quisiste decir '{s}'?)", f.name),
                            None => format!("procedimiento '{p}' no existe en el actor '{}'; decláralo en su campo \"procedures\"", f.name),
                        };
                        self.err(path, msg, Some(r#""procedures":{"doble":{"params":["n"],"returns":"r","body":[...]}}"#));
                    }
                    Some(n) if n != argc => self.err(path, format!("'{p}' espera {n} argumento(s) y recibe {argc}"), None),
                    _ => {}
                }
            }
            self.undefined(&f.scope, &f.name);
            for (_, ps) in &f.proc_scopes {
                self.undefined(ps, &f.name);
            }
        }
        if ui_actors > 1 {
            self.warn("actors", "hay más de un actor con view + RENDER; solo se muestra el primero", None);
        }
    }

    /// Variables leídas que nadie escribe en ese scope.
    fn undefined(&mut self, scope: &Scope, actor: &str) {
        let mut reported = BTreeSet::new();
        for (path, name) in &scope.reads {
            if name.starts_with('$') || scope.defs.contains(name) || !reported.insert(name.clone()) {
                continue;
            }
            let defs: Vec<&str> = scope.defs.iter().map(String::as_str).collect();
            let msg = match closest(name, &defs) {
                Some(s) => format!("la variable '{name}' nunca se define en '{actor}' (¿quisiste decir '{s}'?)"),
                None => format!("la variable '{name}' nunca se define en '{actor}'; créala con DEF_VAR (p. ej. en on_start) o recíbela con on_message.bind"),
            };
            self.err(path, msg, Some(r#"{"op":"DEF_VAR","name":"x","value":0}"#));
        }
    }
}

/// Primer marcador de host (`$…`) usado dentro de una expresión que no sea
/// `var`/`record`/`list` (ver `render::resolve_send`).
fn host_var_misuse(e: &J) -> Option<String> {
    fn any_host_var(e: &J) -> Option<String> {
        match e {
            J::Object(o) => match o.get("var").and_then(J::as_str) {
                Some(v) if v.starts_with('$') => Some(v.to_string()),
                _ => o.values().find_map(any_host_var),
            },
            J::Array(a) => a.iter().find_map(any_host_var),
            _ => None,
        }
    }
    let o = e.as_object()?;
    if o.contains_key("var") {
        return None;
    }
    if let Some(J::Object(fields)) = o.get("record") {
        return fields.values().find_map(host_var_misuse);
    }
    if let Some(J::Array(items)) = o.get("list") {
        return items.iter().find_map(host_var_misuse);
    }
    any_host_var(e)
}

/// Nombres que los modelos usan a menudo para una forma de expresión existente.
fn expr_alias(key: &str) -> Option<&'static str> {
    match key {
        "variable" | "ref" | "get" | "name" => Some("var"),
        "literal" | "value" | "const" => Some("lit"),
        "array" | "items" => Some("list"),
        "object" | "dict" | "map" => Some("record"),
        "length" | "size" | "count" => Some("len"),
        "index" | "get_at" => Some("at"),
        "concat" => Some("join"),
        "str" | "string" | "to_string" => Some("to_str"),
        "to_int" | "to_num" | "number" | "int" | "float" | "to_number" | "parse_int" => Some("parse"),
        _ => None,
    }
}

/// Nombres que un bloque de instrucciones escribe (para detectar bucles infinitos).
fn collect_writes(v: &J, out: &mut BTreeSet<String>) {
    match v {
        J::Array(a) => a.iter().for_each(|x| collect_writes(x, out)),
        J::Object(o) => {
            if o.contains_key("op") {
                for k in ["name", "target", "into", "var", "index"] {
                    if let Some(J::String(s)) = o.get(k) {
                        out.insert(s.clone());
                    }
                }
                for k in ["then", "else", "body"] {
                    if let Some(b) = o.get(k) {
                        collect_writes(b, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Sugerencia por distancia de edición (o mayúsculas/minúsculas) para typos.
fn closest<'a>(word: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let w = word.to_lowercase();
    let mut best: Option<(&str, usize)> = None;
    for &c in candidates {
        let d = levenshtein(&w, &c.to_lowercase());
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some((c, d));
        }
    }
    best.filter(|&(c, d)| d <= 2.max(c.len() / 4) && d < c.len()).map(|(c, _)| c)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + usize::from(ca != *cb)));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Valida el texto de un programa StardustLang.
pub fn check(src: &str) -> Report {
    let mut c = Checker { errors: vec![], warnings: vec![] };
    let root: J = match serde_json::from_str(src) {
        Ok(v) => v,
        Err(e) => {
            let line = src.lines().nth(e.line().saturating_sub(1)).unwrap_or("");
            let col = e.column().saturating_sub(1);
            // Recorte de la línea alrededor del error (las líneas pueden ser enormes).
            let chars: Vec<char> = line.chars().collect();
            let start = col.saturating_sub(40).min(chars.len());
            let end = (col + 40).min(chars.len());
            let snippet: String = chars[start..end].iter().collect();
            if e.is_eof() {
                c.err(
                    "$",
                    "el JSON está incompleto (se corta antes de terminar): faltan llaves } o corchetes ] de cierre",
                    Some("envía el programa completo; si es largo, simplifícalo (menos widgets o instrucciones)"),
                );
                return Report { ok: false, errors: c.errors, warnings: c.warnings };
            }
            c.err(
                "$",
                format!("JSON inválido en línea {}, columna {}: {e}. Cerca de: «{snippet}»", e.line(), e.column()),
                Some("revisa comas sobrantes o faltantes, llaves/corchetes sin cerrar y comillas dobles en todas las claves; no uses comentarios"),
            );
            return Report { ok: false, errors: c.errors, warnings: c.warnings };
        }
    };
    let facts = c.program(&root);
    c.semantics(&root, &facts);
    if c.errors.is_empty() {
        // Última red: el esquema real de la VM.
        if let Err(e) = serde_json::from_value::<crate::program::Program>(root) {
            c.err("$", format!("la StardustVM rechaza el programa: {e}"), None);
        }
    }
    Report { ok: c.errors.is_empty(), errors: c.errors, warnings: c.warnings }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errs(src: &str) -> Vec<String> {
        check(src).errors.into_iter().map(|i| format!("{} :: {}", i.path, i.message)).collect()
    }

    #[test]
    fn todos_los_programas_de_ejemplo_validan() {
        for entry in std::fs::read_dir("programs").unwrap() {
            let p = entry.unwrap().path();
            if p.extension().is_some_and(|e| e == "json") {
                let r = check(&std::fs::read_to_string(&p).unwrap());
                assert!(r.ok, "{}: {:?}", p.display(), r.errors);
            }
        }
    }

    #[test]
    fn json_invalido_dice_linea_y_columna() {
        let e = errs("{\"program\":\"x\",\n\"entry\":\"A\",}");
        assert!(e[0].contains("línea 2"), "{e:?}");
    }

    #[test]
    fn errores_con_ruta_y_sugerencia() {
        let src = r#"{"program":"p","entry":"A","actors":[{"name":"A","capabilities":["RENDER"],
          "on_start":[{"op":"DEF_VAR","name":"n","value":0}],
          "on_message":{"bind":"m","body":[
            {"op":"MATH","target":"n","operator":"plus","left":{"var":"n"},"right":1},
            {"op":"IF_COND","cond":{"var":"ok"},"body":[]},
            {"op":"SEND","to":"Actor_B","value":{"variable":"n"}},
            {"op":"IO_STREAM","mode":"out","value":{"var":"n"}}
          ]},
          "view":{"type":"labl","text":{"var":"n"}}}]}"#;
        let e = errs(src).join("\n");
        assert!(e.contains("actors[0].on_message.body[0].operator"), "{e}");
        assert!(e.contains("IF_COND usa \"then\""), "{e}");
        assert!(e.contains("¿quisiste decir 'var'?"), "{e}");
        assert!(e.contains("¿quisiste decir 'label'?"), "{e}");
    }

    #[test]
    fn semantica_actores_capacidades_variables_y_bucles() {
        let src = r#"{"program":"p","entry":"Main","actors":[{"name":"A",
          "on_start":[
            {"op":"DEF_VAR","name":"i","value":0},
            {"op":"COMPARE","target":"seguir","operator":"<","left":{"var":"i"},"right":3},
            {"op":"LOOP","cond":{"var":"seguir"},"body":[{"op":"MATH","target":"i","operator":"+","left":{"var":"i"},"right":1}]},
            {"op":"SEND","to":"B","value":{"var":"total"}},
            {"op":"IO_STREAM","mode":"out","value":1}
          ]}]}"#;
        let e = errs(src).join("\n");
        assert!(e.contains("entry 'Main'"), "{e}");
        assert!(e.contains("bucle infinito: 'seguir'"), "{e}");
        assert!(e.contains("destino 'B' no existe"), "{e}");
        assert!(e.contains("'total' nunca se define"), "{e}");
        assert!(e.contains("sin la capacidad \"IO_STREAM\""), "{e}");
    }

    #[test]
    fn marcador_de_host_dentro_de_otra_expresion() {
        let src = r#"{"program":"p","entry":"A","actors":[{"name":"A","capabilities":["RENDER"],
          "on_message":{"bind":"m","body":[]},
          "view":{"type":"input","submit":{"parse":{"var":"$input"}}}}]}"#;
        assert!(errs(src).join("
").contains("'$input' solo puede ir tal cual"));
        let ok = r#"{"program":"p","entry":"A","actors":[{"name":"A","capabilities":["RENDER"],
          "on_message":{"bind":"m","body":[{"op":"DEF_VAR","name":"n","value":{"parse":{"var":"m"}}}]},
          "view":{"type":"input","submit":{"record":{"txt":{"var":"$input"}}}}}]}"#;
        assert!(check(ok).ok, "{:?}", check(ok).errors);
    }
}

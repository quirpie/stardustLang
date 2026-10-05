//! Bajada AST → IR (el JSON de StardustLang que ejecuta la VM).
//!
//! Lo que la IR no tiene y aquí se resuelve:
//!   * expresiones con operadores → `MATH`/`COMPARE` sobre temporales `_1`, `_2`…;
//!   * `and`/`or`/`not` → `IF_COND` con cortocircuito;
//!   * `while c` → `c` se recalcula antes del `LOOP` y al final del cuerpo;
//!   * `return` → asignación a `_r` (solo en posición final: la IR no sale antes);
//!   * vista: `for` sobre listas literales se desenrolla, y las expresiones no
//!     puras se convierten en variables derivadas `_vN`, recalculadas al final de
//!     `start` y de `on`;
//!   * capacidades deducidas del uso (o comprobadas contra `uses`).
//!
//! Cada instrucción y widget generado lleva una clave interna `__line` con su
//! línea de origen; `super::compile` la retira y construye el mapa de origen.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value as J};

use super::ast::*;
use super::Diagnostic;

/// Funciones puras: nombre → (nº de argumentos, forma IR).
fn pure_builtin(name: &str) -> Option<usize> {
    Some(match name {
        "len" | "str" | "number" | "int" | "float" | "date" | "bytes" | "text" | "base64" => 1,
        "repeat" => 2,
        _ => return None,
    })
}

const STATEMENT_ONLY: &[&str] = &["print", "send", "write", "delete", "fetch", "sock_send"];

fn d(line: usize, col: usize, message: impl Into<String>, hint: Option<&str>) -> Diagnostic {
    Diagnostic { line, col, message: message.into(), hint: hint.map(str::to_string) }
}

fn var(name: &str) -> J {
    json!({ "var": name })
}

fn with_line(mut v: J, line: usize) -> J {
    if let J::Object(o) = &mut v {
        o.insert("__line".into(), J::from(line));
    }
    v
}

/// Estado de la bajada de UN actor.
struct Ctx<'a> {
    actors: &'a BTreeSet<String>,
    procs: BTreeMap<String, usize>,
    temp: usize,
    derived: usize,
    /// Variables ya definidas en el scope actual (DEF_VAR la primera vez, ASSIGN luego).
    defined: BTreeSet<String>,
    caps: BTreeSet<&'static str>,
    /// Dentro de un `def`: hay `return`.
    in_proc: bool,
    errors: Vec<Diagnostic>,
}

impl<'a> Ctx<'a> {
    fn tmp(&mut self) -> String {
        self.temp += 1;
        format!("_{}", self.temp)
    }

    fn set(&mut self, name: &str, value: J, line: usize) -> J {
        let (op, key) = if self.defined.insert(name.to_string()) { ("DEF_VAR", "name") } else { ("ASSIGN", "target") };
        let mut m = Map::new();
        m.insert("op".into(), J::from(op));
        m.insert(key.into(), J::from(name));
        m.insert("value".into(), value);
        with_line(J::Object(m), line)
    }

    fn err(&mut self, e: &Ex, message: impl Into<String>, hint: Option<&str>) -> J {
        self.errors.push(d(e.line, e.col, message, hint));
        J::from(0)
    }

    // --- Expresiones ----------------------------------------------------------

    /// Baja una expresión: devuelve la expresión IR (pura) y deja en `out` las
    /// instrucciones que hacen falta antes para calcularla.
    fn expr(&mut self, e: &Ex, out: &mut Vec<J>) -> J {
        match &e.kind {
            E::Int(n) => J::from(*n),
            E::Float(f) => J::from(*f),
            E::Str(s) => J::from(s.clone()),
            E::Bool(b) => J::from(*b),
            E::Name(n) => {
                if self.actors.contains(n) {
                    // Un actor usado como valor es su dirección (texto), p. ej. en un record.
                    J::from(n.clone())
                } else {
                    var(n)
                }
            }
            E::Host(h) => var(h),
            E::List(items) => {
                let items: Vec<J> = items.iter().map(|x| self.expr(x, out)).collect();
                json!({ "list": items })
            }
            E::Record(fields) => {
                let mut m = Map::new();
                for (k, v) in fields {
                    let v = self.expr(v, out);
                    m.insert(k.clone(), v);
                }
                json!({ "record": m })
            }
            E::Field(obj, f) => {
                let o = self.expr(obj, out);
                json!({ "field": f, "from": o })
            }
            E::Index(obj, idx) => {
                let o = self.expr(obj, out);
                match &idx.kind {
                    E::Str(k) => json!({ "field": k, "from": o }),
                    _ => {
                        let i = self.expr(idx, out);
                        json!({ "at": i, "of": o })
                    }
                }
            }
            E::Slice(obj, from, to) => {
                let o = self.expr(obj, out);
                let f = from.as_ref().map_or(J::from(0), |x| self.expr(x, out));
                let mut m = json!({ "slice": o, "from": f });
                if let Some(t) = to {
                    m["to"] = self.expr(t, out);
                }
                m
            }
            E::Bin(..) | E::Cmp(..) | E::And(..) | E::Or(..) | E::Not(..) | E::Neg(..) => {
                let t = self.tmp();
                self.into_var(&t, e, out, true);
                var(&t)
            }
            E::In(x, xs, negated) => {
                let c = self.expr(xs, out);
                let s = self.expr(x, out);
                let contains = json!({ "contains": c, "sub": s });
                if *negated {
                    let t = self.tmp();
                    out.push(with_line(
                        json!({"op":"COMPARE","target":t,"operator":"==","left":contains,"right":false}),
                        e.line,
                    ));
                    self.defined.insert(t.clone());
                    var(&t)
                } else {
                    contains
                }
            }
            E::FStr(pieces) => self.fstring(pieces, e.line, out),
            E::Call(name, args, kwargs) => self.call_expr(e, name, args, kwargs, out),
            E::Method(recv, name, args, kwargs) => self.method(e, recv, name, args, kwargs, out),
        }
    }

    fn fstring(&mut self, pieces: &[FPiece], line: usize, out: &mut Vec<J>) -> J {
        let parts: Vec<J> = pieces
            .iter()
            .map(|p| match p {
                FPiece::Lit(s) => J::from(s.clone()),
                FPiece::Expr(x) => self.expr(x, out),
            })
            .collect();
        match parts.as_slice() {
            [] => J::from(""),
            [J::String(s)] => J::from(s.clone()),
            _ => {
                // Concatenación de izquierda a derecha; empezar por "" garantiza texto.
                let mut acc = if parts[0].is_string() { parts[0].clone() } else { J::from("") };
                let rest = if parts[0].is_string() { &parts[1..] } else { &parts[..] };
                for p in rest {
                    let t = self.tmp();
                    out.push(with_line(json!({"op":"MATH","target":t,"operator":"+","left":acc,"right":p}), line));
                    self.defined.insert(t.clone());
                    acc = var(&t);
                }
                acc
            }
        }
    }

    fn call_expr(&mut self, e: &Ex, name: &str, args: &[Ex], kwargs: &[(String, Ex)], out: &mut Vec<J>) -> J {
        if let Some(n) = pure_builtin(name) {
            if args.len() != n || !kwargs.is_empty() {
                return self.err(e, format!("{name}() recibe {n} argumento(s)"), None);
            }
            let a = self.expr(&args[0], out);
            return match name {
                "len" => json!({ "len": a }),
                "str" => json!({ "to_str": a }),
                "number" | "int" | "float" => json!({ "parse": a }),
                "bytes" => json!({ "to_bytes": a }),
                "text" => json!({ "from_bytes": a }),
                "base64" => json!({ "base64": a }),
                "date" => match &args[0].kind {
                    E::Str(s) => json!({ "lit": s, "as": "date" }),
                    _ => json!({ "parse": a }),
                },
                "repeat" => {
                    let b = self.expr(&args[1], out);
                    json!({ "repeat": a, "times": b })
                }
                _ => unreachable!(),
            };
        }
        if STATEMENT_ONLY.contains(&name) {
            return self.err(e, format!("{name}() es una acción, no devuelve un valor: ponla en su propia línea"), None);
        }
        // Instrucciones con resultado (y procedimientos): a una temporal.
        let t = self.tmp();
        self.into_var(&t, e, out, true);
        var(&t)
    }

    fn method(&mut self, e: &Ex, recv: &Ex, name: &str, args: &[Ex], kwargs: &[(String, Ex)], out: &mut Vec<J>) -> J {
        if !kwargs.is_empty() {
            return self.err(e, format!(".{name}() no admite argumentos con nombre"), None);
        }
        let want = |n: usize| args.len() == n;
        let r = self.expr(recv, out);
        let a: Vec<J> = args.iter().map(|x| self.expr(x, out)).collect();
        match name {
            "upper" | "lower" if want(0) => {
                let mut m = Map::new();
                m.insert(name.into(), r);
                J::Object(m)
            }
            "strip" if want(0) => json!({ "trim": r }),
            "split" if want(1) => json!({ "split": r, "on": a[0] }),
            "join" if want(1) => json!({ "join": a[0], "with": r }),
            "replace" if want(2) => json!({ "replace": r, "find": a[0], "with": a[1] }),
            "find" | "index" if want(1) => json!({ "index_of": r, "sub": a[0] }),
            "startswith" if want(1) => json!({ "starts_with": r, "prefix": a[0] }),
            "endswith" if want(1) => json!({ "ends_with": r, "suffix": a[0] }),
            "append" => self.err(e, "xs.append(v) es una acción: ponla en su propia línea", None),
            "upper" | "lower" | "strip" | "split" | "join" | "replace" | "find" | "index" | "startswith" | "endswith" => {
                self.err(e, format!(".{name}() con un número de argumentos incorrecto"), None)
            }
            _ => self.err(
                e,
                format!("método desconocido .{name}()"),
                Some("métodos: upper lower strip split join replace find startswith endswith append"),
            ),
        }
    }

    /// Calcula `e` y deja el resultado en la variable `target`, sin temporal
    /// intermedia cuando la expresión es una operación o una instrucción.
    fn into_var(&mut self, target: &str, e: &Ex, out: &mut Vec<J>, fresh: bool) {
        let line = e.line;
        let mark = |s: &mut Self| {
            s.defined.insert(target.to_string());
        };
        match &e.kind {
            E::Bin(op, a, b) => {
                let l = self.expr(a, out);
                let r = self.expr(b, out);
                out.push(with_line(json!({"op":"MATH","target":target,"operator":op,"left":l,"right":r}), line));
                mark(self);
            }
            E::Neg(a) => {
                let r = self.expr(a, out);
                out.push(with_line(json!({"op":"MATH","target":target,"operator":"-","left":0,"right":r}), line));
                mark(self);
            }
            E::Cmp(op, a, b) => {
                let l = self.expr(a, out);
                let r = self.expr(b, out);
                out.push(with_line(json!({"op":"COMPARE","target":target,"operator":op,"left":l,"right":r}), line));
                mark(self);
            }
            E::Not(a) => {
                let c = self.cond(a, out);
                out.push(self.set(target, J::from(true), line));
                out.push(with_line(json!({"op":"IF_COND","cond":c,"then":[{"op":"ASSIGN","target":target,"value":false}]}), line));
            }
            E::And(a, b) => {
                let ca = self.cond(a, out);
                out.push(self.set(target, J::from(false), line));
                let mut inner = Vec::new();
                let cb = self.cond(b, &mut inner);
                inner.push(with_line(json!({"op":"IF_COND","cond":cb,"then":[{"op":"ASSIGN","target":target,"value":true}]}), line));
                out.push(with_line(json!({"op":"IF_COND","cond":ca,"then":inner}), line));
            }
            E::Or(a, b) => {
                let ca = self.cond(a, out);
                out.push(self.set(target, J::from(true), line));
                let mut inner = Vec::new();
                let cb = self.cond(b, &mut inner);
                inner.push(with_line(json!({"op":"IF_COND","cond":cb,"then":[],"else":[{"op":"ASSIGN","target":target,"value":false}]}), line));
                out.push(with_line(json!({"op":"IF_COND","cond":ca,"then":[],"else":inner}), line));
            }
            E::Call(name, args, kwargs) if !fresh || pure_builtin(name).is_none() => {
                if !self.instr_call(target, e, name, args, kwargs, out) {
                    let v = self.expr(e, out);
                    out.push(self.set(target, v, line));
                }
            }
            _ => {
                let v = self.expr(e, out);
                out.push(self.set(target, v, line));
            }
        }
    }

    /// Llamadas que son instrucciones con resultado (`into`). Devuelve false si
    /// `name` no es una de ellas.
    fn instr_call(&mut self, target: &str, e: &Ex, name: &str, args: &[Ex], kwargs: &[(String, Ex)], out: &mut Vec<J>) -> bool {
        let line = e.line;
        let kw = |k: &str| kwargs.iter().find(|(n, _)| n == k).map(|(_, v)| v);
        let instr = match name {
            "input" => {
                self.caps.insert("IO_STREAM");
                let mut m = json!({"op":"IO_STREAM","mode":"in","target":target});
                match args.first().map(|a| &a.kind) {
                    Some(E::Str(p)) => m["prompt"] = J::from(p.clone()),
                    Some(_) => {
                        self.err(e, "input() admite un texto literal como pregunta", Some("x = input('¿Cuántos? ')"));
                    }
                    None => {}
                }
                m
            }
            "read" => {
                self.caps.insert("FILE");
                if args.len() != 1 {
                    self.err(e, "read(ruta) recibe la ruta", Some("t = read('notas.txt')"));
                    return true;
                }
                let p = self.expr(&args[0], out);
                let mut m = json!({"op":"FILE_READ","path":p,"into":target});
                if let Some(b) = kw("bytes") {
                    if matches!(b.kind, E::Bool(true)) {
                        m["as"] = J::from("bytes");
                    }
                }
                m
            }
            "exists" => {
                self.caps.insert("FILE");
                if args.len() != 1 {
                    self.err(e, "exists(ruta) recibe la ruta", None);
                    return true;
                }
                let p = self.expr(&args[0], out);
                json!({"op":"FILE_EXISTS","path":p,"into":target})
            }
            "files" => {
                self.caps.insert("FILE");
                let mut m = json!({"op":"FILE_LIST","into":target});
                if let Some(a) = args.first() {
                    m["path"] = self.expr(a, out);
                }
                m
            }
            "hash" | "sign" | "serialize" | "deserialize" => {
                if name == "sign" {
                    self.caps.insert("CRYPTO");
                }
                if args.len() != 1 {
                    self.err(e, format!("{name}() recibe un argumento"), None);
                    return true;
                }
                let v = self.expr(&args[0], out);
                json!({"op": name.to_uppercase(), "value": v, "into": target})
            }
            "verify" => {
                self.caps.insert("CRYPTO");
                if args.len() != 2 {
                    self.err(e, "verify(valor, firma) recibe dos argumentos", None);
                    return true;
                }
                let v = self.expr(&args[0], out);
                let s = self.expr(&args[1], out);
                json!({"op":"VERIFY","value":v,"signature":s,"into":target})
            }
            "range" | "enumerate" => {
                self.err(e, format!("{name}() solo puede usarse en la cabecera de un for"), Some("for i in range(10):"));
                return true;
            }
            "print" | "send" | "write" | "delete" | "fetch" | "sock_send" => {
                self.err(e, format!("{name}() es una acción, no devuelve un valor: ponla en su propia línea"), None);
                return true;
            }
            _ if self.procs.contains_key(name) => {
                let n = self.procs[name];
                if args.len() != n {
                    self.err(e, format!("{name}() espera {n} argumento(s) y recibe {}", args.len()), None);
                    return true;
                }
                let a: Vec<J> = args.iter().map(|x| self.expr(x, out)).collect();
                json!({"op":"CALL","proc":name,"args":a,"into":target})
            }
            _ if pure_builtin(name).is_some() => return false,
            _ => {
                let mut known: Vec<&str> = self.procs.keys().map(String::as_str).collect();
                known.extend(["len", "str", "number", "input", "read", "exists", "files", "hash"]);
                let hint = match name {
                    "parse" | "to_int" | "to_number" => Some("number(x)"),
                    "print_line" | "println" | "echo" | "log" => Some("print(x)"),
                    _ => None,
                };
                self.err(e, format!("la función '{name}' no existe en este actor"), hint);
                return true;
            }
        };
        out.push(with_line(instr, line));
        self.defined.insert(target.to_string());
        true
    }

    /// Condición para IF_COND/LOOP: expresión pura (verdadero/falso) o una temporal.
    fn cond(&mut self, e: &Ex, out: &mut Vec<J>) -> J {
        match &e.kind {
            E::Cmp(..) | E::And(..) | E::Or(..) | E::Not(..) => {
                let t = self.tmp();
                self.into_var(&t, e, out, true);
                var(&t)
            }
            _ => self.expr(e, out),
        }
    }

    // --- Sentencias -----------------------------------------------------------

    fn block(&mut self, stmts: &[St]) -> Vec<J> {
        let mut out = Vec::new();
        for s in stmts {
            self.stmt(s, &mut out);
        }
        out
    }

    fn stmt(&mut self, s: &St, out: &mut Vec<J>) {
        let line = s.line;
        match &s.kind {
            S::Pass => {}
            S::Assign(targets, values) => {
                for t in targets {
                    if self.actors.contains(t) {
                        self.errors.push(d(s.line, s.col, format!("'{t}' es el nombre de un actor; usa otro nombre de variable"), None));
                    }
                    if t.starts_with('_') {
                        self.errors.push(d(s.line, s.col, format!("los nombres que empiezan por '_' están reservados ('{t}')"), None));
                    }
                }
                if targets.len() == 1 {
                    self.into_var(&targets[0], &values[0], out, false);
                } else {
                    // Asignación simultánea: primero todos los valores, luego las variables.
                    let temps: Vec<String> = values
                        .iter()
                        .map(|v| {
                            let t = self.tmp();
                            self.into_var(&t, v, out, false);
                            t
                        })
                        .collect();
                    for (t, tmp) in targets.iter().zip(temps) {
                        out.push(self.set(t, var(&tmp), line));
                    }
                }
            }
            S::AugAssign(target, op, value) => {
                let r = self.expr(value, out);
                out.push(with_line(json!({"op":"MATH","target":target,"operator":op,"left":var(target),"right":r}), line));
                self.defined.insert(target.clone());
            }
            S::If(branches, otherwise) => {
                let instr = self.if_chain(branches, otherwise.as_deref(), out);
                out.push(instr);
            }
            S::While(c, body) => {
                let (cond, recompute) = self.loop_cond(c, out);
                let mut b = self.block(body);
                b.extend(recompute);
                out.push(with_line(json!({"op":"LOOP","cond":cond,"body":b}), line));
            }
            S::For { var: v, index, iter, body } => self.for_stmt(s, v, index.as_deref(), iter, body, out),
            S::Return(e) => {
                if !self.in_proc {
                    self.errors.push(d(s.line, s.col, "'return' solo puede ir dentro de un 'def'", None));
                    return;
                }
                self.into_var("_r", e, out, false);
            }
            S::Expr(e) => self.expr_stmt(e, out),
        }
    }

    fn if_chain(&mut self, branches: &[(Ex, Vec<St>)], otherwise: Option<&[St]>, out: &mut Vec<J>) -> J {
        let (c, body) = &branches[0];
        let cond = self.cond(c, out);
        let then = self.block(body);
        let mut instr = json!({"op":"IF_COND","cond":cond,"then":then});
        if branches.len() > 1 {
            // elif → un IF_COND anidado en el else (su condición se calcula ahí, con cortocircuito).
            let mut pre = Vec::new();
            let nested = self.if_chain(&branches[1..], otherwise, &mut pre);
            pre.push(nested);
            instr["else"] = J::Array(pre);
        } else if let Some(o) = otherwise {
            instr["else"] = J::Array(self.block(o));
        }
        with_line(instr, c.line)
    }

    /// Condición de un bucle: (expresión, instrucciones para recalcularla al final del cuerpo).
    fn loop_cond(&mut self, c: &Ex, out: &mut Vec<J>) -> (J, Vec<J>) {
        let mut probe = Vec::new();
        let cond = self.cond(c, &mut probe);
        if probe.is_empty() {
            return (cond, vec![]); // condición pura: la VM ya la reevalúa
        }
        // Se calcula en una variable antes del bucle y se recalcula en la MISMA
        // variable al final de cada vuelta.
        let tc = self.tmp();
        self.into_var(&tc, c, out, true);
        let mut again = Vec::new();
        self.into_var(&tc, c, &mut again, true);
        (var(&tc), again)
    }

    fn for_stmt(&mut self, s: &St, v: &str, index: Option<&str>, iter: &Ex, body: &[St], out: &mut Vec<J>) {
        let line = s.line;
        if let E::Call(name, args, _) = &iter.kind {
            if name == "range" {
                if index.is_some() || args.is_empty() || args.len() > 2 {
                    self.errors.push(d(iter.line, iter.col, "usa range(n) o range(desde, hasta)", Some("for i in range(10):")));
                    return;
                }
                // for v in range(a, b) → v = a; while v < b: ...; v += 1
                let (from, to) = if args.len() == 1 { (None, &args[0]) } else { (Some(&args[0]), &args[1]) };
                let start = from.map_or(J::from(0), |f| self.expr(f, out));
                let end = self.expr(to, out);
                out.push(self.set(v, start, line));
                let tc = self.tmp();
                let cmp = json!({"op":"COMPARE","target":tc,"operator":"<","left":var(v),"right":end});
                out.push(with_line(cmp.clone(), line));
                self.defined.insert(tc.clone());
                let mut b = self.block(body);
                b.push(with_line(json!({"op":"MATH","target":v,"operator":"+","left":var(v),"right":1}), line));
                b.push(with_line(cmp, line));
                out.push(with_line(json!({"op":"LOOP","cond":var(&tc),"body":b}), line));
                return;
            }
        }
        let it = self.expr(iter, out);
        self.defined.insert(v.to_string());
        let mut m = json!({"op":"FOREACH","in":it,"var":v});
        if let Some(i) = index {
            self.defined.insert(i.to_string());
            m["index"] = J::from(i);
        }
        m["body"] = J::Array(self.block(body));
        out.push(with_line(m, line));
    }

    fn expr_stmt(&mut self, e: &Ex, out: &mut Vec<J>) {
        let line = e.line;
        let kw = |kwargs: &[(String, Ex)], k: &str| kwargs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        match &e.kind {
            E::Call(name, args, kwargs) => match name.as_str() {
                "print" => {
                    self.caps.insert("IO_STREAM");
                    let instr = match args.as_slice() {
                        [] => json!({"op":"IO_STREAM","mode":"out","prompt":""}),
                        [Ex { kind: E::Str(p), .. }] => json!({"op":"IO_STREAM","mode":"out","prompt":p}),
                        [Ex { kind: E::Str(p), .. }, v] => {
                            let v = self.expr(v, out);
                            json!({"op":"IO_STREAM","mode":"out","prompt":p,"value":v})
                        }
                        [v] => {
                            let v = self.expr(v, out);
                            json!({"op":"IO_STREAM","mode":"out","value":v})
                        }
                        many => {
                            let pieces: Vec<FPiece> = many.iter().map(|x| FPiece::Expr(x.clone())).collect();
                            let v = self.fstring(&pieces, line, out);
                            json!({"op":"IO_STREAM","mode":"out","value":v})
                        }
                    };
                    out.push(with_line(instr, line));
                }
                "send" => {
                    if args.len() != 2 {
                        self.err(e, "send(destino, valor) recibe dos argumentos", Some("send(UI, resultado)"));
                        return;
                    }
                    let to = match &args[0].kind {
                        E::Name(n) if self.actors.contains(n) => J::from(n.clone()),
                        E::Str(s) => {
                            if s.starts_with("actor://") {
                                self.caps.insert("NET");
                            }
                            J::from(s.clone())
                        }
                        _ => self.expr(&args[0], out),
                    };
                    let value = self.expr(&args[1], out);
                    let mut m = json!({"op":"SEND","to":to,"value":value});
                    if let Some(c) = kw(kwargs, "cap") {
                        match c.kind {
                            E::Str(s) => m["cap"] = J::from(s),
                            _ => {
                                self.err(&c, "cap debe ser un texto literal", None);
                            }
                        }
                    }
                    out.push(with_line(m, line));
                }
                "write" | "delete" => {
                    self.caps.insert("FILE");
                    let want = if name == "write" { 2 } else { 1 };
                    if args.len() != want {
                        let hint = if name == "write" { "write('notas.txt', texto)" } else { "delete('notas.txt')" };
                        self.err(e, format!("{name}() recibe {want} argumento(s)"), Some(hint));
                        return;
                    }
                    let p = self.expr(&args[0], out);
                    let m = if name == "delete" {
                        json!({"op":"FILE_DELETE","path":p})
                    } else {
                        let v = self.expr(&args[1], out);
                        let append = matches!(kw(kwargs, "append").map(|x| x.kind), Some(E::Bool(true)));
                        let op = if append { "FILE_APPEND" } else { "FILE_WRITE" };
                        json!({"op": op, "path": p, "value": v})
                    };
                    out.push(with_line(m, line));
                }
                "fetch" => {
                    self.caps.insert("NET");
                    if args.len() != 1 {
                        self.err(e, "fetch(url, method=…, headers=…, body=…, tag=…)", None);
                        return;
                    }
                    let u = self.expr(&args[0], out);
                    let mut m = json!({"op":"NET_FETCH","url":u});
                    for k in ["method", "headers", "body", "tag"] {
                        if let Some(x) = kw(kwargs, k) {
                            m[k] = if k == "method" {
                                match &x.kind {
                                    E::Str(s) => J::from(s.to_uppercase()),
                                    _ => self.err(&x, "method debe ser un texto literal ('GET', 'POST'…)", None),
                                }
                            } else {
                                self.expr(&x, out)
                            };
                        }
                    }
                    out.push(with_line(m, line));
                }
                "sock_send" => {
                    self.caps.insert("NET");
                    if args.len() != 2 {
                        self.err(e, "sock_send(direccion, cuerpo, tag=…)", None);
                        return;
                    }
                    let a = self.expr(&args[0], out);
                    let b = self.expr(&args[1], out);
                    let mut m = json!({"op":"SOCK_SEND","addr":a,"body":b});
                    if let Some(x) = kw(kwargs, "tag") {
                        m["tag"] = self.expr(&x, out);
                    }
                    out.push(with_line(m, line));
                }
                _ if self.procs.contains_key(name.as_str()) => {
                    let n = self.procs[name.as_str()];
                    if args.len() != n {
                        self.err(e, format!("{name}() espera {n} argumento(s) y recibe {}", args.len()), None);
                        return;
                    }
                    let a: Vec<J> = args.iter().map(|x| self.expr(x, out)).collect();
                    out.push(with_line(json!({"op":"CALL","proc":name,"args":a}), line));
                }
                _ => {
                    let t = self.tmp();
                    self.into_var(&t, e, out, true);
                }
            },
            E::Method(recv, name, args, _) if name == "append" => match (&recv.kind, args.as_slice()) {
                (E::Name(target), [v]) => {
                    let v = self.expr(v, out);
                    out.push(with_line(json!({"op":"APPEND","target":target,"value":v}), line));
                    self.defined.insert(target.clone());
                }
                _ => {
                    self.err(e, "append se usa como lista.append(valor), sobre una variable", Some("tareas.append(m)"));
                }
            },
            _ => {
                self.err(e, "esta expresión no hace nada: asígnala a una variable o envíala", Some("x = ..."));
            }
        }
    }

    // --- Vista ----------------------------------------------------------------

    /// Expresión de la vista: pura, o una variable derivada calculada en `derived`.
    fn view_expr(&mut self, e: &Ex, loop_vars: &BTreeSet<String>, derived: &mut Vec<J>) -> J {
        let mut pre = Vec::new();
        let v = self.expr(e, &mut pre);
        if pre.is_empty() {
            return v;
        }
        if mentions(e, loop_vars) {
            return self.err(
                e,
                "dentro de un 'for' de la vista solo caben expresiones simples (sin operaciones); prepara el texto en la lista",
                None,
            );
        }
        self.derived += 1;
        let name = format!("_v{}", self.derived);
        derived.extend(pre);
        derived.push(with_line(json!({"op":"DEF_VAR","name":name,"value":v}), e.line));
        var(&name)
    }

    fn widgets(&mut self, ws: &[W], loop_vars: &BTreeSet<String>, derived: &mut Vec<J>) -> Vec<J> {
        let mut out = Vec::new();
        for w in ws {
            match w {
                W::For { var: v, index, iter, body, empty, line, .. } if matches!(iter.kind, E::List(_)) && index.is_none() => {
                    // Lista literal: se desenrolla al compilar.
                    let E::List(items) = &iter.kind else { unreachable!() };
                    if empty.is_some() {
                        self.errors.push(d(*line, 1, "'else' no tiene sentido sobre una lista literal", None));
                    }
                    for item in items {
                        let subst: Vec<W> = body.iter().map(|b| subst_w(b, v, item)).collect();
                        out.extend(self.widgets(&subst, loop_vars, derived));
                    }
                }
                _ => out.push(self.widget(w, loop_vars, derived)),
            }
        }
        out
    }

    fn widget(&mut self, w: &W, loop_vars: &BTreeSet<String>, derived: &mut Vec<J>) -> J {
        match w {
            W::Container { kind, args, children, line, col } => {
                let kids = self.widgets(children, loop_vars, derived);
                let m = match kind.as_str() {
                    "column" | "row" => json!({"type": kind, "children": kids}),
                    "grid" => match args.as_slice() {
                        [Ex { kind: E::Int(n), .. }] if *n > 0 => json!({"type":"grid","columns":n,"children":kids}),
                        _ => {
                            self.errors.push(d(*line, *col, "grid necesita el número de columnas: grid(3):", None));
                            json!({"type":"column","children":kids})
                        }
                    },
                    other => {
                        self.errors.push(d(*line, *col, format!("'{other}' no es un contenedor; usa column:, row: o grid(n):"), None));
                        json!({"type":"column","children":kids})
                    }
                };
                with_line(m, *line)
            }
            W::For { var: v, index, iter, body, empty, line, .. } => {
                let bind = self.view_expr(iter, loop_vars, derived);
                let mut inner = loop_vars.clone();
                inner.insert(v.clone());
                if let Some(i) = index {
                    inner.insert(i.clone());
                }
                let mut items = self.widgets(body, &inner, derived);
                let item = if items.len() == 1 { items.pop().unwrap() } else { json!({"type":"column","children":items}) };
                let mut m = json!({"type":"list","bind":bind,"as":v,"item":item});
                if let Some(i) = index {
                    m["index"] = J::from(i.clone());
                }
                if let Some(e) = empty {
                    let mut es = self.widgets(e, loop_vars, derived);
                    m["empty"] = if es.len() == 1 { es.pop().unwrap() } else { json!({"type":"column","children":es}) };
                }
                with_line(m, *line)
            }
            W::Leaf(e) => {
                let line = e.line;
                let E::Call(name, args, kwargs) = &e.kind else {
                    return self.err(e, "en la vista cada línea es un widget: label(...), button(...), input(...)…", None);
                };
                let mut m = Map::new();
                m.insert("type".into(), J::from(name.clone()));
                let pos = |i: usize| args.get(i);
                let kw = |k: &str| kwargs.iter().find(|(n, _)| n == k).map(|(_, v)| v);
                let allowed: &[&str] = match name.as_str() {
                    "label" => &[],
                    "button" => &["send", "to"],
                    "input" | "textarea" => &["placeholder", "submit", "to", "src", "label"],
                    "image" => &["alt"],
                    "html" => &[],
                    "filedrop" => &["drop", "to", "label"],
                    "column" | "row" | "grid" => {
                        return self.err(e, format!("{name} es un contenedor: escríbelo con ':' y sus hijos indentados"), Some("column:\n  label(x)"));
                    }
                    _ => {
                        return self.err(e, format!("widget desconocido '{name}'"), Some("label, button, input, textarea, image, html, filedrop, column, row, grid"));
                    }
                };
                for (k, v) in kwargs {
                    if !allowed.contains(&k.as_str()) {
                        self.err(v, format!("{name}() no tiene el argumento '{k}'"), None);
                    }
                }
                match name.as_str() {
                    "label" => match pos(0) {
                        Some(x) => {
                            m.insert("text".into(), self.view_expr(x, loop_vars, derived));
                        }
                        None => {
                            self.err(e, "label(texto)", None);
                        }
                    },
                    "button" => {
                        match pos(0) {
                            Some(x) => m.insert("label".into(), self.view_expr(x, loop_vars, derived)),
                            None => return self.err(e, "button(etiqueta, send=valor)", Some("button('+1', send=1)")),
                        };
                        match kw("send") {
                            Some(x) => {
                                let s = self.send_expr(x, loop_vars, derived);
                                m.insert("send".into(), s);
                            }
                            None => return self.err(e, "a button le falta send=: el mensaje que envía al pulsarlo", Some("button('+1', send=1)")),
                        }
                    }
                    "input" | "textarea" => {
                        if let Some(Ex { kind: E::Str(s), .. }) = pos(0).or(kw("label")) {
                            m.insert("label".into(), J::from(s.clone()));
                        }
                        if let Some(Ex { kind: E::Str(s), .. }) = kw("placeholder") {
                            m.insert("placeholder".into(), J::from(s.clone()));
                        }
                        if let Some(x) = kw("src") {
                            m.insert("src".into(), self.view_expr(x, loop_vars, derived));
                        }
                        let submit = match kw("submit") {
                            Some(x) => self.send_expr(x, loop_vars, derived),
                            None => var("$input"),
                        };
                        m.insert("submit".into(), submit);
                    }
                    "image" | "html" => {
                        match pos(0) {
                            Some(x) => m.insert("src".into(), self.view_expr(x, loop_vars, derived)),
                            None => return self.err(e, format!("{name}(fuente)"), None),
                        };
                        if let Some(x) = kw("alt") {
                            m.insert("alt".into(), self.view_expr(x, loop_vars, derived));
                        }
                    }
                    "filedrop" => {
                        if let Some(Ex { kind: E::Str(s), .. }) = pos(0).or(kw("label")) {
                            m.insert("label".into(), J::from(s.clone()));
                        }
                        let drop = match kw("drop") {
                            Some(x) => self.send_expr(x, loop_vars, derived),
                            None => json!({"record":{"name":{"var":"$name"},"bytes":{"var":"$bytes"}}}),
                        };
                        m.insert("drop".into(), drop);
                    }
                    _ => unreachable!(),
                }
                if let Some(to) = kw("to") {
                    match &to.kind {
                        E::Name(n) if self.actors.contains(n) => {
                            m.insert("to".into(), J::from(n.clone()));
                        }
                        E::Name(n) | E::Str(n) => {
                            self.err(to, format!("to={n}: no hay ningún actor con ese nombre"), None);
                        }
                        _ => {
                            self.err(to, "to= es el nombre de un actor", Some("to=Logica"));
                        }
                    }
                }
                with_line(J::Object(m), line)
            }
        }
    }

    /// `send=`/`submit=`/`drop=` de la vista: los marcadores `$…` solo pueden ir
    /// directos o dentro de listas/records.
    fn send_expr(&mut self, e: &Ex, loop_vars: &BTreeSet<String>, derived: &mut Vec<J>) -> J {
        match &e.kind {
            E::Host(h) => var(h),
            E::Record(fields) => {
                let mut m = Map::new();
                for (k, v) in fields {
                    m.insert(k.clone(), self.send_expr(v, loop_vars, derived));
                }
                json!({ "record": m })
            }
            E::List(items) => {
                let items: Vec<J> = items.iter().map(|x| self.send_expr(x, loop_vars, derived)).collect();
                json!({ "list": items })
            }
            _ if has_host(e) => self.err(
                e,
                "$input/$name/$bytes van tal cual (o dentro de {…}/[…]); conviértelos después, en el 'on' del actor",
                Some("input('OK', submit=$input)   y en on m:  n = number(m)"),
            ),
            _ => self.view_expr(e, loop_vars, derived),
        }
    }
}

/// ¿La expresión menciona alguna de estas variables?
fn mentions(e: &Ex, names: &BTreeSet<String>) -> bool {
    let mut found = false;
    walk(e, &mut |x| {
        if let E::Name(n) = &x.kind {
            found |= names.contains(n);
        }
    });
    found
}

fn has_host(e: &Ex) -> bool {
    let mut found = false;
    walk(e, &mut |x| found |= matches!(x.kind, E::Host(_)));
    found
}

fn walk(e: &Ex, f: &mut dyn FnMut(&Ex)) {
    f(e);
    match &e.kind {
        E::List(xs) => xs.iter().for_each(|x| walk(x, f)),
        E::Record(fs) => fs.iter().for_each(|(_, x)| walk(x, f)),
        E::Field(a, _) | E::Not(a) | E::Neg(a) => walk(a, f),
        E::Index(a, b) | E::Bin(_, a, b) | E::Cmp(_, a, b) | E::And(a, b) | E::Or(a, b) | E::In(a, b, _) => {
            walk(a, f);
            walk(b, f);
        }
        E::Slice(a, b, c) => {
            walk(a, f);
            b.iter().for_each(|x| walk(x, f));
            c.iter().for_each(|x| walk(x, f));
        }
        E::Call(_, args, kw) => {
            args.iter().for_each(|x| walk(x, f));
            kw.iter().for_each(|(_, x)| walk(x, f));
        }
        E::Method(r, _, args, kw) => {
            walk(r, f);
            args.iter().for_each(|x| walk(x, f));
            kw.iter().for_each(|(_, x)| walk(x, f));
        }
        E::FStr(ps) => ps.iter().for_each(|p| {
            if let FPiece::Expr(x) = p {
                walk(x, f)
            }
        }),
        _ => {}
    }
}

/// Sustituye una variable por una expresión en un widget (desenrollado de `for`).
fn subst_w(w: &W, name: &str, with: &Ex) -> W {
    match w {
        W::Leaf(e) => W::Leaf(subst(e, name, with)),
        W::Container { kind, args, children, line, col } => W::Container {
            kind: kind.clone(),
            args: args.iter().map(|a| subst(a, name, with)).collect(),
            children: children.iter().map(|c| subst_w(c, name, with)).collect(),
            line: *line,
            col: *col,
        },
        W::For { var, index, iter, body, empty, line, col } => W::For {
            var: var.clone(),
            index: index.clone(),
            iter: subst(iter, name, with),
            body: if var == name { body.clone() } else { body.iter().map(|c| subst_w(c, name, with)).collect() },
            empty: empty.as_ref().map(|e| e.iter().map(|c| subst_w(c, name, with)).collect()),
            line: *line,
            col: *col,
        },
    }
}

fn subst(e: &Ex, name: &str, with: &Ex) -> Ex {
    let s = |x: &Ex| Box::new(subst(x, name, with));
    let kind = match &e.kind {
        E::Name(n) if n == name => return Ex { kind: with.kind.clone(), line: e.line, col: e.col },
        E::List(xs) => E::List(xs.iter().map(|x| subst(x, name, with)).collect()),
        E::Record(fs) => E::Record(fs.iter().map(|(k, x)| (k.clone(), subst(x, name, with))).collect()),
        E::Field(a, f) => E::Field(s(a), f.clone()),
        E::Index(a, b) => E::Index(s(a), s(b)),
        E::Slice(a, b, c) => E::Slice(s(a), b.as_ref().map(|x| s(x)), c.as_ref().map(|x| s(x))),
        E::Bin(o, a, b) => E::Bin(o, s(a), s(b)),
        E::Cmp(o, a, b) => E::Cmp(o, s(a), s(b)),
        E::And(a, b) => E::And(s(a), s(b)),
        E::Or(a, b) => E::Or(s(a), s(b)),
        E::Not(a) => E::Not(s(a)),
        E::Neg(a) => E::Neg(s(a)),
        E::In(a, b, n) => E::In(s(a), s(b), *n),
        E::Call(f, args, kw) => E::Call(
            f.clone(),
            args.iter().map(|x| subst(x, name, with)).collect(),
            kw.iter().map(|(k, x)| (k.clone(), subst(x, name, with))).collect(),
        ),
        E::Method(r, m, args, kw) => E::Method(
            s(r),
            m.clone(),
            args.iter().map(|x| subst(x, name, with)).collect(),
            kw.iter().map(|(k, x)| (k.clone(), subst(x, name, with))).collect(),
        ),
        E::FStr(ps) => E::FStr(
            ps.iter()
                .map(|p| match p {
                    FPiece::Expr(x) => FPiece::Expr(subst(x, name, with)),
                    lit => lit.clone(),
                })
                .collect(),
        ),
        other => other.clone(),
    };
    Ex { kind, line: e.line, col: e.col }
}

/// `return` solo en posición final (la IR no tiene salida anticipada).
fn check_returns(stmts: &[St], tail: bool, errors: &mut Vec<Diagnostic>) -> bool {
    let mut any = false;
    for (i, s) in stmts.iter().enumerate() {
        let last = tail && i == stmts.len() - 1;
        match &s.kind {
            S::Return(_) => {
                any = true;
                if !last {
                    errors.push(d(
                        s.line,
                        s.col,
                        "'return' solo puede ir al final de la función (no hay salida anticipada)",
                        Some("guarda el resultado en una variable y haz 'return' al final, o usa if/else con 'return' al final de cada rama"),
                    ));
                }
            }
            S::If(branches, otherwise) => {
                for (_, b) in branches {
                    any |= check_returns(b, last, errors);
                }
                if let Some(o) = otherwise {
                    any |= check_returns(o, last, errors);
                }
            }
            S::While(_, b) | S::For { body: b, .. } => any |= check_returns(b, false, errors),
            _ => {}
        }
    }
    any
}

const CAPS: &[&str] = &["IO_STREAM", "FILE", "CRYPTO", "NET", "RENDER"];

/// Baja la app completa a la IR (con claves `__line`).
pub fn lower(app: &App) -> Result<J, Vec<Diagnostic>> {
    let mut errors = Vec::new();
    let names: BTreeSet<String> = app.actors.iter().map(|a| a.name.clone()).collect();
    let mut seen = BTreeSet::new();
    let mut actors_json = Vec::new();

    for a in &app.actors {
        if !seen.insert(a.name.clone()) {
            errors.push(d(a.line, 1, format!("actor duplicado '{}'", a.name), None));
        }
        let procs: BTreeMap<String, usize> = a.defs.iter().map(|f| (f.name.clone(), f.params.len())).collect();
        let mut cx = Ctx {
            actors: &names,
            procs: procs.clone(),
            temp: 0,
            derived: 0,
            defined: BTreeSet::new(),
            caps: BTreeSet::new(),
            in_proc: false,
            errors: Vec::new(),
        };

        // Procedimientos: scope aislado, retorno en `_r`.
        let mut procs_json = Map::new();
        for f in &a.defs {
            let has_return = check_returns(&f.body, true, &mut cx.errors);
            let saved = std::mem::replace(&mut cx.defined, f.params.iter().cloned().collect());
            cx.in_proc = true;
            let body = cx.block(&f.body);
            cx.in_proc = false;
            cx.defined = saved;
            let mut p = json!({"params": f.params, "body": body});
            if has_return {
                p["returns"] = J::from("_r");
            }
            procs_json.insert(f.name.clone(), with_line(p, f.line));
        }

        let mut start = a.start.as_ref().map(|b| cx.block(b));
        let mut on = a.on.as_ref().map(|o| {
            cx.defined.insert(o.bind.clone());
            if let Some(f) = &o.from {
                cx.defined.insert(f.clone());
            }
            (o, cx.block(&o.body))
        });

        // Vista (con sus variables derivadas).
        let mut derived = Vec::new();
        let view = a.view.as_ref().map(|ws| {
            cx.caps.insert("RENDER");
            let mut roots = cx.widgets(ws, &BTreeSet::new(), &mut derived);
            if roots.len() == 1 {
                roots.pop().unwrap()
            } else {
                json!({"type":"column","children":roots})
            }
        });
        if !derived.is_empty() {
            start.get_or_insert_with(Vec::new).extend(derived.iter().cloned());
            if let Some((_, body)) = on.as_mut() {
                body.extend(derived.iter().cloned());
            }
        }

        // Capacidades: deducidas, o comprobadas contra `uses`.
        let caps: Vec<String> = match &a.uses {
            None => cx.caps.iter().map(|c| c.to_string()).collect(),
            Some(declared) => {
                for c in declared {
                    if !CAPS.contains(&c.as_str()) {
                        cx.errors.push(d(a.line, 1, format!("capacidad desconocida '{c}'; las válidas son {}", CAPS.join(", ")), None));
                    }
                }
                for c in &cx.caps {
                    if !declared.iter().any(|x| x == c) {
                        cx.errors.push(d(a.line, 1, format!("el actor '{}' usa {c} pero no está en 'uses'", a.name), Some("uses FILE, NET")));
                    }
                }
                declared.clone()
            }
        };

        let mut actor = json!({"name": a.name, "capabilities": caps});
        if let Some(s) = start {
            actor["on_start"] = J::Array(s);
        }
        if let Some((o, body)) = on {
            let mut m = json!({"bind": o.bind, "body": body});
            if let Some(f) = &o.from {
                m["reply_to"] = J::from(f.clone());
            }
            if let Some(ex) = &o.expects {
                m["expects"] = J::Object(ex.iter().map(|(k, t)| (k.clone(), J::from(t.clone()))).collect());
            }
            actor["on_message"] = with_line(m, o.line);
        }
        if !procs_json.is_empty() {
            actor["procedures"] = J::Object(procs_json);
        }
        if let Some(v) = view {
            actor["view"] = v;
        }
        errors.extend(cx.errors);
        actors_json.push(with_line(actor, a.line));
    }

    let entry = match &app.entry {
        Some((e, line)) => {
            if !names.contains(e) {
                errors.push(d(*line, 1, format!("entry '{e}' no es ningún actor"), None));
            }
            e.clone()
        }
        None => app.actors[0].name.clone(),
    };
    for e in &app.exports {
        if !names.contains(e) {
            errors.push(d(1, 1, format!("export '{e}': no hay ningún actor con ese nombre"), None));
        }
    }

    let mut program = json!({"program": app.name, "entry": entry, "actors": actors_json});
    if !app.allow.is_empty() {
        program["net_allow"] = json!(app.allow);
    }
    if !app.exports.is_empty() {
        program["exports"] = json!(app.exports);
    }
    if !app.grants.is_empty() {
        program["grants"] = J::Object(app.grants.iter().map(|(t, a)| (t.clone(), json!(a))).collect());
    }
    if errors.is_empty() {
        Ok(program)
    } else {
        // Una expresión puede bajarse más de una vez (condiciones de bucle): sin duplicados.
        errors.sort_by(|a, b| (a.line, a.col, &a.message).cmp(&(b.line, b.col, &b.message)));
        errors.dedup_by(|a, b| a.line == b.line && a.col == b.col && a.message == b.message);
        Err(errors)
    }
}

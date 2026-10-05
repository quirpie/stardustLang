//! Parser descendente recursivo: tokens → [`App`]. Se detiene en el primer error
//! de sintaxis (como Python) con línea, columna y una pista.

use super::ast::*;
use super::lexer::{lex_at, FPart, Tok, Token};
use super::Diagnostic;

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

type R<T> = Result<T, Diagnostic>;

fn diag(line: usize, col: usize, message: impl Into<String>, hint: Option<&str>) -> Diagnostic {
    Diagnostic { line, col, message: message.into(), hint: hint.map(str::to_string) }
}

/// Palabras que no pueden usarse como nombre de variable.
const KEYWORDS: &[&str] = &[
    "and", "or", "not", "in", "if", "elif", "else", "while", "for", "def", "return", "pass", "True", "False", "true",
    "false",
];

const ACTOR_HINT: &str = "actor UI:\n  start:\n    x = 0\n  on m:\n    ...\n  view:\n    ...";

impl Parser {
    pub fn new(toks: Vec<Token>) -> Self {
        Parser { toks, pos: 0 }
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }
    fn here(&self) -> (usize, usize) {
        let t = &self.toks[self.pos];
        (t.line, t.col)
    }
    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), Tok::Op(o) if *o == op)
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Name(n) if n == kw)
    }
    fn eat_op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn found(&self) -> String {
        match self.peek() {
            Tok::Name(n) => format!("'{n}'"),
            Tok::Host(n) => format!("'{n}'"),
            Tok::Int(n) => format!("el número {n}"),
            Tok::Float(n) => format!("el número {n}"),
            Tok::Str(s) => format!("el texto '{s}'"),
            Tok::FStr(_) => "una f-string".into(),
            Tok::Op(o) => format!("'{o}'"),
            Tok::Newline => "el final de la línea".into(),
            Tok::Indent => "una indentación de más".into(),
            Tok::Dedent => "el final del bloque".into(),
            Tok::Eof => "el final del programa".into(),
        }
    }
    fn expect_op(&mut self, op: &str, hint: Option<&str>) -> R<()> {
        if self.eat_op(op) {
            return Ok(());
        }
        let (l, c) = self.here();
        Err(diag(l, c, format!("se esperaba '{op}' y hay {}", self.found()), hint))
    }
    fn expect_newline(&mut self) -> R<()> {
        if matches!(self.peek(), Tok::Newline) {
            self.bump();
            return Ok(());
        }
        if matches!(self.peek(), Tok::Eof | Tok::Dedent) {
            return Ok(());
        }
        let (l, c) = self.here();
        Err(diag(l, c, format!("se esperaba el final de la línea y hay {}", self.found()), None))
    }
    fn name(&mut self, what: &str) -> R<String> {
        let (l, c) = self.here();
        match self.peek().clone() {
            Tok::Name(n) if !KEYWORDS.contains(&n.as_str()) => {
                self.bump();
                Ok(n)
            }
            _ => Err(diag(l, c, format!("se esperaba {what} y hay {}", self.found()), None)),
        }
    }

    // --- Programa -------------------------------------------------------------

    pub fn app(&mut self) -> R<App> {
        let mut app = App::default();
        while matches!(self.peek(), Tok::Newline) {
            self.bump();
        }
        let (l, c) = self.here();
        if !self.eat_kw("app") {
            return Err(diag(l, c, "el programa debe empezar con 'app <nombre>'", Some("app mi-app")));
        }
        app.name = self.line_text(l, c)?;
        loop {
            let (l, c) = self.here();
            match self.peek().clone() {
                Tok::Eof => break,
                Tok::Newline => {
                    self.bump();
                }
                Tok::Name(k) if k == "allow" => {
                    self.bump();
                    app.allow.push(self.string("una URL entre comillas")?);
                    self.expect_newline()?;
                }
                Tok::Name(k) if k == "export" => {
                    self.bump();
                    app.exports.extend(self.name_list("el nombre de un actor")?);
                    self.expect_newline()?;
                }
                Tok::Name(k) if k == "grant" => {
                    self.bump();
                    let token = self.string("el token entre comillas")?;
                    self.expect_op(":", Some("grant 'mi-token': Actor"))?;
                    app.grants.push((token, self.name_list("el nombre de un actor")?));
                    self.expect_newline()?;
                }
                Tok::Name(k) if k == "entry" => {
                    self.bump();
                    let (el, _) = self.here();
                    app.entry = Some((self.name("el nombre de un actor")?, el));
                    self.expect_newline()?;
                }
                Tok::Name(k) if k == "actor" => {
                    self.bump();
                    app.actors.push(self.actor(l)?);
                }
                Tok::Indent => {
                    return Err(diag(l, c, "indentación inesperada fuera de un actor", Some(ACTOR_HINT)));
                }
                _ => {
                    return Err(diag(
                        l,
                        c,
                        format!("se esperaba 'actor', 'allow', 'export', 'grant' o 'entry' y hay {}", self.found()),
                        Some(ACTOR_HINT),
                    ))
                }
            }
        }
        if app.actors.is_empty() {
            return Err(diag(l_or(&self.toks), 1, "el programa no tiene ningún actor", Some(ACTOR_HINT)));
        }
        Ok(app)
    }

    /// El resto de la línea como texto (nombres con guiones: `app par-impar`).
    fn line_text(&mut self, l: usize, c: usize) -> R<String> {
        if let Tok::Str(s) = self.peek().clone() {
            self.bump();
            self.expect_newline()?;
            return Ok(s);
        }
        let mut s = String::new();
        loop {
            match self.peek().clone() {
                Tok::Name(n) => s.push_str(&n),
                Tok::Int(n) => s.push_str(&n.to_string()),
                Tok::Op(o) if matches!(o, "-" | "." | "/") => s.push_str(o),
                _ => break,
            }
            self.bump();
        }
        if s.is_empty() {
            return Err(diag(l, c, "falta el nombre de la app", Some("app mi-app")));
        }
        self.expect_newline()?;
        Ok(s)
    }

    fn string(&mut self, what: &str) -> R<String> {
        let (l, c) = self.here();
        match self.peek().clone() {
            Tok::Str(s) => {
                self.bump();
                Ok(s)
            }
            _ => Err(diag(l, c, format!("se esperaba {what} y hay {}", self.found()), None)),
        }
    }

    fn name_list(&mut self, what: &str) -> R<Vec<String>> {
        let mut v = vec![self.name(what)?];
        while self.eat_op(",") {
            v.push(self.name(what)?);
        }
        Ok(v)
    }

    fn actor(&mut self, line: usize) -> R<Actor> {
        let name = self.name("el nombre del actor")?;
        self.expect_op(":", Some(ACTOR_HINT))?;
        self.expect_newline()?;
        let (l, c) = self.here();
        if !matches!(self.peek(), Tok::Indent) {
            return Err(diag(l, c, format!("el actor '{name}' está vacío: indenta su contenido"), Some(ACTOR_HINT)));
        }
        self.bump();
        let mut a = Actor { name, uses: None, start: None, on: None, defs: vec![], view: None, line };
        loop {
            let (l, c) = self.here();
            match self.peek().clone() {
                Tok::Dedent => {
                    self.bump();
                    break;
                }
                Tok::Eof => break,
                Tok::Newline => {
                    self.bump();
                }
                Tok::Name(k) if k == "uses" => {
                    self.bump();
                    a.uses = Some(self.name_list("una capacidad (FILE, NET, …)")?);
                    self.expect_newline()?;
                }
                Tok::Name(k) if k == "start" => {
                    self.bump();
                    if a.start.is_some() {
                        return Err(diag(l, c, "el actor ya tiene un 'start'", None));
                    }
                    self.expect_op(":", Some("start:"))?;
                    a.start = Some(self.block()?);
                }
                Tok::Name(k) if k == "on" => {
                    self.bump();
                    if a.on.is_some() {
                        return Err(diag(
                            l,
                            c,
                            "un actor tiene un solo 'on'; distingue los mensajes dentro con if",
                            Some("on m:\n  if m.cmd == 'sumar':\n    ..."),
                        ));
                    }
                    a.on = Some(self.on(l)?);
                }
                Tok::Name(k) if k == "def" => {
                    self.bump();
                    a.defs.push(self.def(l)?);
                }
                Tok::Name(k) if k == "view" => {
                    self.bump();
                    self.expect_op(":", Some("view:\n  column:\n    label(x)"))?;
                    self.expect_newline()?;
                    a.view = Some(self.widget_block()?);
                }
                _ => {
                    return Err(diag(
                        l,
                        c,
                        format!("dentro de un actor se esperaba 'start:', 'on m:', 'def f():', 'view:' o 'uses' y hay {}", self.found()),
                        Some(ACTOR_HINT),
                    ))
                }
            }
        }
        Ok(a)
    }

    fn on(&mut self, line: usize) -> R<On> {
        let bind = self.name("el nombre de la variable del mensaje (p. ej. 'on m:')")?;
        let mut expects = None;
        if self.eat_op("(") {
            let mut fields = Vec::new();
            while !self.is_op(")") {
                let f = self.name("un campo")?;
                self.expect_op(":", Some("on p(base: int, altura: int):"))?;
                let t = self.name("un tipo (int, float, string, bool, date, list, record, bytes)")?;
                fields.push((f, t));
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op(")", None)?;
            expects = Some(fields);
        }
        let from = if self.eat_kw("from") { Some(self.name("el nombre de la variable de respuesta")?) } else { None };
        self.expect_op(":", Some("on m:"))?;
        let body = self.block()?;
        Ok(On { bind, expects, from, body, line })
    }

    fn def(&mut self, line: usize) -> R<Def> {
        let name = self.name("el nombre de la función")?;
        self.expect_op("(", Some("def doble(n):"))?;
        let mut params = Vec::new();
        while !self.is_op(")") {
            params.push(self.name("un parámetro")?);
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")", None)?;
        self.expect_op(":", Some("def doble(n):"))?;
        let body = self.block()?;
        Ok(Def { name, params, body, line })
    }

    // --- Sentencias -----------------------------------------------------------

    /// Bloque tras ':' — indentado en las líneas siguientes, o una sentencia simple en la misma línea.
    fn block(&mut self) -> R<Vec<St>> {
        if !matches!(self.peek(), Tok::Newline) {
            let s = self.simple_stmt()?;
            self.expect_newline()?;
            return Ok(vec![s]);
        }
        self.bump();
        let (l, c) = self.here();
        if !matches!(self.peek(), Tok::Indent) {
            return Err(diag(l, c, "se esperaba un bloque indentado después de ':'", Some("if x > 0:\n  y = 1")));
        }
        self.bump();
        let mut out = Vec::new();
        loop {
            match self.peek() {
                Tok::Dedent => {
                    self.bump();
                    break;
                }
                Tok::Eof => break,
                Tok::Newline => {
                    self.bump();
                }
                _ => out.push(self.stmt()?),
            }
        }
        Ok(out)
    }

    fn stmt(&mut self) -> R<St> {
        let (line, col) = self.here();
        let kind = if self.eat_kw("if") {
            let mut branches = vec![(self.expr()?, self.colon_block()?)];
            let mut otherwise = None;
            loop {
                if self.eat_kw("elif") {
                    branches.push((self.expr()?, self.colon_block()?));
                } else if self.eat_kw("else") {
                    otherwise = Some(self.colon_block()?);
                    break;
                } else {
                    break;
                }
            }
            S::If(branches, otherwise)
        } else if self.eat_kw("while") {
            S::While(self.expr()?, self.colon_block()?)
        } else if self.eat_kw("for") {
            let (var, index, iter) = self.for_head()?;
            S::For { var, index, iter, body: self.colon_block()? }
        } else if self.is_kw("elif") || self.is_kw("else") {
            return Err(diag(line, col, format!("{} sin un 'if' antes (revisa la indentación)", self.found()), None));
        } else {
            let s = self.simple_stmt()?;
            self.expect_newline()?;
            return Ok(s);
        };
        Ok(St { kind, line, col })
    }

    fn colon_block(&mut self) -> R<Vec<St>> {
        self.expect_op(":", Some("if x > 0:\n  ..."))?;
        self.block()
    }

    /// `x in xs` / `i, x in enumerate(xs)` (sin el ':').
    fn for_head(&mut self) -> R<(String, Option<String>, Ex)> {
        let first = self.name("la variable del bucle")?;
        let second = if self.eat_op(",") { Some(self.name("la segunda variable del bucle")?) } else { None };
        let (l, c) = self.here();
        if !self.eat_kw("in") {
            return Err(diag(l, c, format!("se esperaba 'in' y hay {}", self.found()), Some("for x in lista:")));
        }
        let iter = self.expr()?;
        match second {
            None => Ok((first, None, iter)),
            Some(var) => match iter.kind {
                E::Call(ref f, ref args, _) if f == "enumerate" && args.len() == 1 => Ok((var, Some(first), args[0].clone())),
                _ => Err(diag(l, c, "con dos variables, el bucle debe ser 'for i, x in enumerate(lista)'", None)),
            },
        }
    }

    fn simple_stmt(&mut self) -> R<St> {
        let (line, col) = self.here();
        if self.eat_kw("return") {
            return Ok(St { kind: S::Return(self.expr()?), line, col });
        }
        if self.eat_kw("pass") {
            return Ok(St { kind: S::Pass, line, col });
        }
        let first = self.expr()?;
        let mut lhs = vec![first];
        while self.eat_op(",") {
            lhs.push(self.expr()?);
        }
        for aug in ["+=", "-=", "*=", "/="] {
            if self.eat_op(aug) {
                let target = assign_target(&lhs, line, col)?;
                if target.len() != 1 {
                    return Err(diag(line, col, format!("'{aug}' admite una sola variable"), None));
                }
                let op: &'static str = &aug[..1];
                return Ok(St { kind: S::AugAssign(target[0].clone(), op, self.expr()?), line, col });
            }
        }
        if self.eat_op("=") {
            let targets = assign_target(&lhs, line, col)?;
            let mut rhs = vec![self.expr()?];
            while self.eat_op(",") {
                rhs.push(self.expr()?);
            }
            if rhs.len() != targets.len() {
                return Err(diag(line, col, format!("{} variable(s) a la izquierda y {} valor(es) a la derecha", targets.len(), rhs.len()), None));
            }
            return Ok(St { kind: S::Assign(targets, rhs), line, col });
        }
        if lhs.len() > 1 {
            return Err(diag(line, col, "lista de expresiones sin asignar", None));
        }
        Ok(St { kind: S::Expr(lhs.pop().unwrap()), line, col })
    }

    // --- Vista ----------------------------------------------------------------

    fn widget_block(&mut self) -> R<Vec<W>> {
        let (l, c) = self.here();
        if !matches!(self.peek(), Tok::Indent) {
            return Err(diag(l, c, "se esperaba un bloque indentado de widgets", Some("view:\n  column:\n    label(x)")));
        }
        self.bump();
        let mut out = Vec::new();
        loop {
            match self.peek() {
                Tok::Dedent => {
                    self.bump();
                    break;
                }
                Tok::Eof => break,
                Tok::Newline => {
                    self.bump();
                }
                _ => out.push(self.widget()?),
            }
        }
        Ok(out)
    }

    fn widget(&mut self) -> R<W> {
        let (line, col) = self.here();
        if self.eat_kw("for") {
            let (var, index, iter) = self.for_head()?;
            self.expect_op(":", Some("for x in lista:\n  label(x)"))?;
            self.expect_newline()?;
            let body = self.widget_block()?;
            let empty = if self.eat_kw("else") {
                self.expect_op(":", None)?;
                self.expect_newline()?;
                Some(self.widget_block()?)
            } else {
                None
            };
            return Ok(W::For { var, index, iter, body, empty, line, col });
        }
        let e = self.expr()?;
        if self.eat_op(":") {
            self.expect_newline()?;
            let (kind, args) = match e.kind {
                E::Name(n) => (n, vec![]),
                E::Call(n, a, _) => (n, a),
                _ => return Err(diag(line, col, "un contenedor es 'column:', 'row:' o 'grid(3):'", None)),
            };
            let children = self.widget_block()?;
            return Ok(W::Container { kind, args, children, line, col });
        }
        self.expect_newline()?;
        Ok(W::Leaf(e))
    }

    // --- Expresiones ----------------------------------------------------------

    pub fn expr(&mut self) -> R<Ex> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> R<Ex> {
        let mut a = self.and_expr()?;
        while self.is_kw("or") {
            let (l, c) = self.here();
            self.bump();
            let b = self.and_expr()?;
            a = Ex { kind: E::Or(Box::new(a), Box::new(b)), line: l, col: c };
        }
        Ok(a)
    }

    fn and_expr(&mut self) -> R<Ex> {
        let mut a = self.not_expr()?;
        while self.is_kw("and") {
            let (l, c) = self.here();
            self.bump();
            let b = self.not_expr()?;
            a = Ex { kind: E::And(Box::new(a), Box::new(b)), line: l, col: c };
        }
        Ok(a)
    }

    fn not_expr(&mut self) -> R<Ex> {
        let (l, c) = self.here();
        if self.eat_kw("not") {
            let a = self.not_expr()?;
            return Ok(Ex { kind: E::Not(Box::new(a)), line: l, col: c });
        }
        self.comparison()
    }

    fn comparison(&mut self) -> R<Ex> {
        let a = self.sum()?;
        let (l, c) = self.here();
        let op = match self.peek() {
            Tok::Op(o) if matches!(*o, "==" | "!=" | "<" | ">" | "<=" | ">=") => Some(*o),
            _ => None,
        };
        let res = if let Some(op) = op {
            self.bump();
            let b = self.sum()?;
            Ex { kind: E::Cmp(op, Box::new(a), Box::new(b)), line: l, col: c }
        } else if self.is_kw("in") || (self.is_kw("not") && matches!(self.peek_at(1), Tok::Name(n) if n == "in")) {
            let negated = self.eat_kw("not");
            self.bump();
            let b = self.sum()?;
            Ex { kind: E::In(Box::new(a), Box::new(b), negated), line: l, col: c }
        } else {
            return Ok(a);
        };
        if matches!(self.peek(), Tok::Op(o) if matches!(*o, "==" | "!=" | "<" | ">" | "<=" | ">=")) {
            let (l, c) = self.here();
            return Err(diag(l, c, "comparaciones encadenadas no admitidas; usa 'and'", Some("a < b and b < c")));
        }
        Ok(res)
    }

    fn sum(&mut self) -> R<Ex> {
        let mut a = self.term()?;
        loop {
            let (l, c) = self.here();
            let op = match self.peek() {
                Tok::Op("+") => "+",
                Tok::Op("-") => "-",
                _ => break,
            };
            self.bump();
            let b = self.term()?;
            a = Ex { kind: E::Bin(op, Box::new(a), Box::new(b)), line: l, col: c };
        }
        Ok(a)
    }

    fn term(&mut self) -> R<Ex> {
        let mut a = self.unary()?;
        loop {
            let (l, c) = self.here();
            let op = match self.peek() {
                Tok::Op("*") => "*",
                Tok::Op("/") | Tok::Op("//") => "/",
                Tok::Op("%") => "%",
                _ => break,
            };
            self.bump();
            let b = self.unary()?;
            a = Ex { kind: E::Bin(op, Box::new(a), Box::new(b)), line: l, col: c };
        }
        Ok(a)
    }

    fn unary(&mut self) -> R<Ex> {
        let (l, c) = self.here();
        if self.eat_op("-") {
            let a = self.unary()?;
            return Ok(match a.kind {
                E::Int(n) => Ex { kind: E::Int(-n), line: l, col: c },
                E::Float(f) => Ex { kind: E::Float(-f), line: l, col: c },
                k => Ex { kind: E::Neg(Box::new(Ex { kind: k, line: a.line, col: a.col })), line: l, col: c },
            });
        }
        self.postfix()
    }

    fn postfix(&mut self) -> R<Ex> {
        let mut a = self.atom()?;
        loop {
            let (l, c) = self.here();
            if self.eat_op(".") {
                let name = self.name("un campo o método después de '.'")?;
                if self.eat_op("(") {
                    let (args, kwargs) = self.args()?;
                    a = Ex { kind: E::Method(Box::new(a), name, args, kwargs), line: l, col: c };
                } else {
                    a = Ex { kind: E::Field(Box::new(a), name), line: l, col: c };
                }
            } else if self.eat_op("[") {
                // Índice o rebanada.
                let from = if self.is_op(":") { None } else { Some(Box::new(self.expr()?)) };
                if self.eat_op(":") {
                    let to = if self.is_op("]") { None } else { Some(Box::new(self.expr()?)) };
                    self.expect_op("]", None)?;
                    a = Ex { kind: E::Slice(Box::new(a), from, to), line: l, col: c };
                } else {
                    self.expect_op("]", None)?;
                    a = Ex { kind: E::Index(Box::new(a), from.unwrap()), line: l, col: c };
                }
            } else if self.is_op("(") && matches!(a.kind, E::Name(_)) {
                self.bump();
                let (args, kwargs) = self.args()?;
                let E::Name(n) = a.kind else { unreachable!() };
                a = Ex { kind: E::Call(n, args, kwargs), line: a.line, col: a.col };
            } else {
                break;
            }
        }
        Ok(a)
    }

    /// Argumentos tras '(' hasta ')': posicionales y luego `nombre=valor`.
    fn args(&mut self) -> R<(Vec<Ex>, Vec<(String, Ex)>)> {
        let mut args = Vec::new();
        let mut kwargs = Vec::new();
        while !self.is_op(")") {
            if matches!(self.peek(), Tok::Name(_)) && matches!(self.peek_at(1), Tok::Op("=")) {
                let k = self.name("un argumento")?;
                self.bump();
                kwargs.push((k, self.expr()?));
            } else {
                if !kwargs.is_empty() {
                    let (l, c) = self.here();
                    return Err(diag(l, c, "los argumentos con nombre van después de los posicionales", None));
                }
                args.push(self.expr()?);
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")", None)?;
        Ok((args, kwargs))
    }

    fn atom(&mut self) -> R<Ex> {
        let (l, c) = self.here();
        let mk = |kind| Ex { kind, line: l, col: c };
        match self.peek().clone() {
            Tok::Int(n) => {
                self.bump();
                Ok(mk(E::Int(n)))
            }
            Tok::Float(f) => {
                self.bump();
                Ok(mk(E::Float(f)))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(mk(E::Str(s)))
            }
            Tok::Host(h) => {
                self.bump();
                Ok(mk(E::Host(h)))
            }
            Tok::FStr(parts) => {
                self.bump();
                let mut pieces = Vec::new();
                for p in parts {
                    match p {
                        FPart::Lit(s) => pieces.push(FPiece::Lit(s)),
                        FPart::Expr { src, line, col } => {
                            let toks = lex_at(&src, line, col, false)?;
                            let mut sub = Parser::new(toks);
                            let e = sub.expr()?;
                            if !matches!(sub.peek(), Tok::Eof) {
                                let (l, c) = sub.here();
                                return Err(diag(l, c, format!("f-string: sobra {} dentro de {{}}", sub.found()), None));
                            }
                            pieces.push(FPiece::Expr(e));
                        }
                    }
                }
                Ok(mk(E::FStr(pieces)))
            }
            Tok::Name(n) => match n.as_str() {
                "True" | "true" => {
                    self.bump();
                    Ok(mk(E::Bool(true)))
                }
                "False" | "false" => {
                    self.bump();
                    Ok(mk(E::Bool(false)))
                }
                "None" | "null" => Err(diag(l, c, "no hay valor nulo; usa '' , 0 o False", None)),
                _ if KEYWORDS.contains(&n.as_str()) => Err(diag(l, c, format!("'{n}' no puede ir aquí"), None)),
                _ => {
                    self.bump();
                    Ok(mk(E::Name(n)))
                }
            },
            Tok::Op("(") => {
                self.bump();
                let e = self.expr()?;
                self.expect_op(")", None)?;
                Ok(e)
            }
            Tok::Op("[") => {
                self.bump();
                let mut items = Vec::new();
                while !self.is_op("]") {
                    items.push(self.expr()?);
                    if !self.eat_op(",") {
                        break;
                    }
                }
                self.expect_op("]", Some("[1, 2, 3]"))?;
                Ok(mk(E::List(items)))
            }
            Tok::Op("{") => {
                self.bump();
                let mut fields = Vec::new();
                while !self.is_op("}") {
                    let (kl, kc) = self.here();
                    let key = match self.peek().clone() {
                        Tok::Name(n) => n,
                        Tok::Str(s) => s,
                        _ => return Err(diag(kl, kc, format!("se esperaba el nombre de un campo y hay {}", self.found()), Some("{nombre: 'Ana', edad: 30}"))),
                    };
                    self.bump();
                    self.expect_op(":", Some("{nombre: 'Ana', edad: 30}"))?;
                    fields.push((key, self.expr()?));
                    if !self.eat_op(",") {
                        break;
                    }
                }
                self.expect_op("}", Some("{nombre: 'Ana', edad: 30}"))?;
                Ok(mk(E::Record(fields)))
            }
            _ => Err(diag(l, c, format!("se esperaba un valor y hay {}", self.found()), None)),
        }
    }
}

/// Valida que el lado izquierdo de una asignación sean nombres de variable.
fn assign_target(lhs: &[Ex], line: usize, col: usize) -> R<Vec<String>> {
    lhs.iter()
        .map(|e| match &e.kind {
            E::Name(n) => Ok(n.clone()),
            E::Field(..) | E::Index(..) => Err(diag(
                e.line,
                e.col,
                "no se puede asignar a un campo o elemento; crea el valor completo de nuevo",
                Some("p = {nombre: p.nombre, edad: 31}"),
            )),
            _ => Err(diag(line, col, "a la izquierda de '=' solo puede ir un nombre de variable", None)),
        })
        .collect()
}

fn l_or(toks: &[Token]) -> usize {
    toks.last().map_or(1, |t| t.line)
}

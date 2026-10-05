//! Árbol sintáctico de la sintaxis de texto. Cada nodo guarda su línea/columna
//! para los errores y el mapa de origen.

#[derive(Debug, Clone)]
pub struct Ex {
    pub kind: E,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone)]
pub enum E {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Name(String),
    Host(String),
    List(Vec<Ex>),
    Record(Vec<(String, Ex)>),
    Field(Box<Ex>, String),
    Index(Box<Ex>, Box<Ex>),
    Slice(Box<Ex>, Option<Box<Ex>>, Option<Box<Ex>>),
    /// Aritmética: `+ - * / %` (`//` se normaliza a `/`).
    Bin(&'static str, Box<Ex>, Box<Ex>),
    /// Comparación: `== != < > <= >=`.
    Cmp(&'static str, Box<Ex>, Box<Ex>),
    And(Box<Ex>, Box<Ex>),
    Or(Box<Ex>, Box<Ex>),
    Not(Box<Ex>),
    Neg(Box<Ex>),
    /// `x in xs` (o `not in` si el bool es true).
    In(Box<Ex>, Box<Ex>, bool),
    /// Llamada a función: nombre, posicionales, con nombre.
    Call(String, Vec<Ex>, Vec<(String, Ex)>),
    /// Llamada a método: receptor, nombre, posicionales, con nombre.
    Method(Box<Ex>, String, Vec<Ex>, Vec<(String, Ex)>),
    /// f-string ya parseada: literales y expresiones.
    FStr(Vec<FPiece>),
}

#[derive(Debug, Clone)]
pub enum FPiece {
    Lit(String),
    Expr(Ex),
}

#[derive(Debug, Clone)]
pub struct St {
    pub kind: S,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone)]
pub enum S {
    /// `a = e` o `a, b = e1, e2`.
    Assign(Vec<String>, Vec<Ex>),
    /// `a += e`.
    AugAssign(String, &'static str, Ex),
    If(Vec<(Ex, Vec<St>)>, Option<Vec<St>>),
    While(Ex, Vec<St>),
    /// `for var in iter` / `for index, var in enumerate(iter)`.
    For { var: String, index: Option<String>, iter: Ex, body: Vec<St> },
    Return(Ex),
    Pass,
    Expr(Ex),
}

#[derive(Debug, Clone)]
pub struct Def {
    pub name: String,
    pub params: Vec<String>,
    pub body: Vec<St>,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct On {
    pub bind: String,
    pub expects: Option<Vec<(String, String)>>,
    pub from: Option<String>,
    pub body: Vec<St>,
    pub line: usize,
}

/// Nodo de la vista.
#[derive(Debug, Clone)]
pub enum W {
    /// `column:` / `row:` / `grid(n):` con hijos.
    Container { kind: String, args: Vec<Ex>, children: Vec<W>, line: usize, col: usize },
    /// Hoja: `label(...)`, `button(...)`, …
    Leaf(Ex),
    /// `for x in xs:` (+ `else:`).
    For { var: String, index: Option<String>, iter: Ex, body: Vec<W>, empty: Option<Vec<W>>, line: usize, col: usize },
}

#[derive(Debug, Clone)]
pub struct Actor {
    pub name: String,
    pub uses: Option<Vec<String>>,
    pub start: Option<Vec<St>>,
    pub on: Option<On>,
    pub defs: Vec<Def>,
    pub view: Option<Vec<W>>,
    pub line: usize,
}

#[derive(Debug, Clone, Default)]
pub struct App {
    pub name: String,
    pub entry: Option<(String, usize)>,
    pub allow: Vec<String>,
    pub exports: Vec<String>,
    pub grants: Vec<(String, Vec<String>)>,
    pub actors: Vec<Actor>,
}

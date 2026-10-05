//! Lexer de la sintaxis de texto (`.stardust`): tokens con línea/columna y bloques
//! por indentación al estilo Python (`Indent`/`Dedent`). Dentro de `()`, `[]` y
//! `{}` los saltos de línea se ignoran (unión implícita de líneas).

use super::Diagnostic;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Name(String),
    /// Variable de host: `$input`, `$name`, `$bytes`.
    Host(String),
    Int(i64),
    Float(f64),
    Str(String),
    /// f-string: trozos literales y expresiones (texto fuente + posición).
    FStr(Vec<FPart>),
    /// Puntuación y operadores: `( ) [ ] { } , : . = += -= *= /= == != < > <= >= + - * / // %`.
    Op(&'static str),
    Newline,
    Indent,
    Dedent,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FPart {
    Lit(String),
    Expr { src: String, line: usize, col: usize },
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub line: usize,
    pub col: usize,
}

const OPS: &[&str] = &[
    "//", "==", "!=", "<=", ">=", "+=", "-=", "*=", "/=", "(", ")", "[", "]", "{", "}", ",", ":", ".", "=", "<", ">", "+",
    "-", "*", "/", "%",
];

fn err(line: usize, col: usize, message: impl Into<String>) -> Diagnostic {
    Diagnostic { line, col, message: message.into(), hint: None }
}

/// Convierte el texto fuente en tokens. `line0`/`col0` desplazan las posiciones
/// (para re-lexear las expresiones de una f-string en su sitio real).
pub fn lex(src: &str) -> Result<Vec<Token>, Diagnostic> {
    lex_at(src, 1, 1, true)
}

pub fn lex_at(src: &str, line0: usize, col0: usize, layout: bool) -> Result<Vec<Token>, Diagnostic> {
    let chars: Vec<char> = src.chars().collect();
    let mut out: Vec<Token> = Vec::new();
    let mut indents: Vec<usize> = vec![0];
    let mut depth = 0usize; // anidamiento de paréntesis
    let mut i = 0usize;
    let mut line = line0;
    let mut col = col0;
    let mut at_line_start = layout;

    let push = |out: &mut Vec<Token>, tok: Tok, line: usize, col: usize| out.push(Token { tok, line, col });

    while i < chars.len() {
        // Inicio de línea lógica: medir indentación.
        if at_line_start {
            let mut width = 0usize;
            let start = i;
            while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                width += if chars[i] == '\t' { 4 - width % 4 } else { 1 };
                i += 1;
            }
            col += i - start;
            // Línea en blanco o solo comentario: no cuenta.
            if i >= chars.len() || chars[i] == '\n' || chars[i] == '\r' || chars[i] == '#' {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                    line += 1;
                    col = 1;
                }
                continue;
            }
            at_line_start = false;
            let cur = *indents.last().unwrap();
            if width > cur {
                indents.push(width);
                push(&mut out, Tok::Indent, line, col);
            } else if width < cur {
                while width < *indents.last().unwrap() {
                    indents.pop();
                    push(&mut out, Tok::Dedent, line, col);
                }
                if width != *indents.last().unwrap() {
                    return Err(err(line, col, "la indentación no coincide con ningún bloque anterior"));
                }
            }
        }

        let c = chars[i];
        match c {
            '\n' => {
                if depth == 0 && layout {
                    if !matches!(out.last().map(|t| &t.tok), Some(Tok::Newline) | None) {
                        push(&mut out, Tok::Newline, line, col);
                    }
                    at_line_start = true;
                }
                i += 1;
                line += 1;
                col = 1;
            }
            ' ' | '\t' | '\r' => {
                i += 1;
                col += 1;
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '\'' | '"' => {
                let (s, n, nl) = read_string(&chars, i, line, col)?;
                push(&mut out, Tok::Str(s), line, col);
                i += n;
                line += nl.0;
                col = if nl.0 > 0 { nl.1 } else { col + n };
            }
            'f' if i + 1 < chars.len() && (chars[i + 1] == '\'' || chars[i + 1] == '"') => {
                let (parts, n) = read_fstring(&chars, i + 1, line, col + 1)?;
                push(&mut out, Tok::FStr(parts), line, col);
                i += n + 1;
                col += n + 1;
            }
            '$' => {
                let start = i;
                i += 1;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                if name.len() == 1 {
                    return Err(err(line, col, "'$' debe ir seguido de un nombre: $input, $name o $bytes"));
                }
                push(&mut out, Tok::Host(name), line, col);
                col += i - start;
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
                    i += 1;
                }
                let mut is_float = false;
                if i + 1 < chars.len() && chars[i] == '.' && chars[i + 1].is_ascii_digit() {
                    is_float = true;
                    i += 1;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let text: String = chars[start..i].iter().filter(|c| **c != '_').collect();
                let tok = if is_float {
                    Tok::Float(text.parse().map_err(|_| err(line, col, format!("número inválido '{text}'")))?)
                } else {
                    Tok::Int(text.parse().map_err(|_| err(line, col, format!("entero fuera de rango '{text}'")))?)
                };
                push(&mut out, tok, line, col);
                col += i - start;
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                push(&mut out, Tok::Name(name), line, col);
                col += i - start;
            }
            _ => {
                let rest: String = chars[i..chars.len().min(i + 2)].iter().collect();
                let Some(op) = OPS.iter().find(|op| rest.starts_with(**op)) else {
                    return Err(err(line, col, format!("carácter inesperado '{c}'")));
                };
                match *op {
                    "(" | "[" | "{" => depth += 1,
                    ")" | "]" | "}" => depth = depth.saturating_sub(1),
                    _ => {}
                }
                push(&mut out, Tok::Op(op), line, col);
                i += op.len();
                col += op.len();
            }
        }
    }
    if layout {
        if !matches!(out.last().map(|t| &t.tok), Some(Tok::Newline) | None) {
            push(&mut out, Tok::Newline, line, col);
        }
        while indents.len() > 1 {
            indents.pop();
            push(&mut out, Tok::Dedent, line, col);
        }
    }
    push(&mut out, Tok::Eof, line, col);
    Ok(out)
}

/// Lee un literal de texto empezando en la comilla. Devuelve (texto, caracteres
/// consumidos, (saltos de línea, columna final)).
fn read_string(chars: &[char], start: usize, line: usize, col: usize) -> Result<(String, usize, (usize, usize)), Diagnostic> {
    let q = chars[start];
    let mut s = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            c if c == q => return Ok((s, i + 1 - start, (0, 0))),
            '\n' => break,
            '\\' if i + 1 < chars.len() => {
                s.push(match chars[i + 1] {
                    'n' => '\n',
                    't' => '\t',
                    other => other,
                });
                i += 2;
            }
            c => {
                s.push(c);
                i += 1;
            }
        }
    }
    Err(Diagnostic {
        line,
        col,
        message: "texto sin cerrar: falta la comilla final".into(),
        hint: Some("cada texto empieza y termina con la misma comilla en la misma línea: 'hola'".into()),
    })
}

/// Lee una f-string (empezando en la comilla) y la parte en literales y `{expr}`.
fn read_fstring(chars: &[char], start: usize, line: usize, col: usize) -> Result<(Vec<FPart>, usize), Diagnostic> {
    let q = chars[start];
    let mut parts = Vec::new();
    let mut lit = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == q {
            if !lit.is_empty() {
                parts.push(FPart::Lit(lit));
            }
            return Ok((parts, i + 1 - start));
        }
        match c {
            '\n' => break,
            '{' if chars.get(i + 1) == Some(&'{') => {
                lit.push('{');
                i += 2;
            }
            '}' if chars.get(i + 1) == Some(&'}') => {
                lit.push('}');
                i += 2;
            }
            '{' => {
                if !lit.is_empty() {
                    parts.push(FPart::Lit(std::mem::take(&mut lit)));
                }
                let expr_col = col + (i + 1 - start);
                let mut depth = 0;
                let mut j = i + 1;
                let mut inner_q: Option<char> = None;
                while j < chars.len() {
                    let d = chars[j];
                    match inner_q {
                        Some(iq) if d == iq => inner_q = None,
                        Some(_) => {}
                        None => match d {
                            '\'' | '"' if d != q => inner_q = Some(d),
                            '(' | '[' | '{' => depth += 1,
                            ')' | ']' => depth -= 1,
                            '}' if depth > 0 => depth -= 1,
                            '}' => break,
                            _ if d == q || d == '\n' => break,
                            _ => {}
                        },
                    }
                    j += 1;
                }
                if j >= chars.len() || chars[j] != '}' {
                    return Err(Diagnostic {
                        line,
                        col: expr_col,
                        message: "f-string: falta la '}' de cierre".into(),
                        hint: Some("f'Total: {n}'".into()),
                    });
                }
                let src: String = chars[i + 1..j].iter().collect();
                if src.trim().is_empty() {
                    return Err(Diagnostic { line, col: expr_col, message: "f-string: '{}' vacío".into(), hint: None });
                }
                parts.push(FPart::Expr { src, line, col: expr_col });
                i = j + 1;
            }
            '\\' if i + 1 < chars.len() => {
                lit.push(match chars[i + 1] {
                    'n' => '\n',
                    't' => '\t',
                    other => other,
                });
                i += 2;
            }
            c => {
                lit.push(c);
                i += 1;
            }
        }
    }
    Err(Diagnostic {
        line,
        col,
        message: "f-string sin cerrar: falta la comilla final".into(),
        hint: Some("f'Total: {n}'".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<Tok> {
        lex(src).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn indentacion_y_union_de_lineas() {
        let t = kinds("a:\n  b = [1,\n    2]\nc\n");
        assert_eq!(
            t,
            vec![
                Tok::Name("a".into()),
                Tok::Op(":"),
                Tok::Newline,
                Tok::Indent,
                Tok::Name("b".into()),
                Tok::Op("="),
                Tok::Op("["),
                Tok::Int(1),
                Tok::Op(","),
                Tok::Int(2),
                Tok::Op("]"),
                Tok::Newline,
                Tok::Dedent,
                Tok::Name("c".into()),
                Tok::Newline,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn fstring_partes() {
        let t = kinds("f'Total: {len(xs)} €'");
        let Tok::FStr(parts) = &t[0] else { panic!() };
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[1], FPart::Expr { src, .. } if src == "len(xs)"));
    }

    #[test]
    fn dedent_inconsistente_es_error() {
        assert!(lex("a:\n    b\n  c\n").is_err());
    }
}

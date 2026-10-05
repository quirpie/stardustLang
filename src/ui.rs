//! Pintor de terminal: el primer backend del UI-IR de StardustLang.
//!
//! Ya **no** resuelve el estado — de eso se encarga el núcleo ([`crate::render`]),
//! común a todos los hosts. Aquí solo se traduce un [`RenderNode`] ya resuelto a
//! texto y se recolectan los elementos activables (botones y campos). Un backend
//! gráfico (web) consume el mismo árbol; por eso este módulo es deliberadamente
//! delgado.

use crate::render::{Event, RenderNode};

/// Un elemento activable desde la terminal: su etiqueta (lo que el usuario teclea
/// para dispararlo), el evento que emite y si además pide un valor por teclado
/// (los campos `Input`, cuya entrada rellena el marcador `$input`).
#[derive(Clone)]
pub struct Activatable {
    pub label: String,
    pub event: Event,
    pub prompts: bool,
}

/// Resultado de pintar una vista: líneas a imprimir + elementos activables.
pub struct Rendered {
    pub lines: Vec<String>,
    pub actions: Vec<Activatable>,
}

/// Traduce un árbol resuelto a texto de terminal.
pub fn paint(node: &RenderNode) -> Rendered {
    let mut r = Rendered {
        lines: Vec::new(),
        actions: Vec::new(),
    };
    walk(node, &mut r);
    r
}

fn walk(node: &RenderNode, r: &mut Rendered) {
    match node {
        RenderNode::Label { text } => {
            let width = 14;
            let inner = format!("{:>w$}", text, w = width - 2);
            r.lines.push(format!("┌{}┐", "─".repeat(width)));
            r.lines.push(format!("│ {inner} │"));
            r.lines.push(format!("└{}┘", "─".repeat(width)));
        }
        RenderNode::Button { label, event } => {
            r.lines.push(format!("[ {label} ]"));
            r.actions.push(Activatable {
                label: label.clone(),
                event: event.clone(),
                prompts: false,
            });
        }
        RenderNode::Column { children } | RenderNode::List { children } => {
            for c in children {
                walk(c, r);
            }
        }
        RenderNode::Row { children } => {
            let mut cells = Vec::new();
            for c in children {
                if let RenderNode::Button { label, event } = c {
                    cells.push(format!("[ {label} ]"));
                    r.actions.push(Activatable {
                        label: label.clone(),
                        event: event.clone(),
                        prompts: false,
                    });
                } else {
                    walk(c, r); // los no-botones se apilan (simplificación del POC)
                }
            }
            if !cells.is_empty() {
                r.lines.push(cells.join(" "));
            }
        }
        RenderNode::Grid { columns, children } => {
            let mut line = String::new();
            let mut col = 0;
            for c in children {
                if let RenderNode::Button { label, event } = c {
                    line.push_str(&format!("[ {label:^3} ] "));
                    r.actions.push(Activatable {
                        label: label.clone(),
                        event: event.clone(),
                        prompts: false,
                    });
                    col += 1;
                    if col % *columns == 0 {
                        r.lines.push(line.trim_end().to_string());
                        line.clear();
                    }
                }
            }
            if !line.is_empty() {
                r.lines.push(line.trim_end().to_string());
            }
        }
        RenderNode::Input {
            value,
            placeholder,
            label,
            event,
        } => {
            let key = if !label.is_empty() {
                label.clone()
            } else if !placeholder.is_empty() {
                placeholder.clone()
            } else {
                "campo".to_string()
            };
            let shown = if value.is_empty() {
                String::new()
            } else {
                format!(" [{value}]")
            };
            r.lines
                .push(format!("[ {key}… ]{shown}  (teclea «{key}» para escribir)"));
            r.actions.push(Activatable {
                label: key,
                event: event.clone(),
                prompts: true,
            });
        }
        RenderNode::Image { alt, src_b64 } => {
            let bytes = src_b64.as_ref().map(|s| s.len() * 3 / 4).unwrap_or(0);
            let desc = if alt.is_empty() { "imagen" } else { alt };
            r.lines.push(format!("🖼  {desc}  (~{bytes} bytes)"));
        }
        RenderNode::FileDrop { label, .. } => {
            r.lines.push(format!("⇩  {label}  (captura de archivos: solo web)"));
        }
        RenderNode::Textarea {
            value,
            placeholder,
            label,
            event,
        } => {
            if !value.is_empty() {
                r.lines.push("┌─ texto actual ─".to_string());
                for l in value.lines() {
                    r.lines.push(format!("│ {l}"));
                }
                r.lines.push("└────────────────".to_string());
            }
            let hint = if placeholder.is_empty() { "una línea" } else { placeholder };
            r.lines
                .push(format!("[ {label}… ]  (teclea «{label}» para escribir: {hint})"));
            r.actions.push(Activatable {
                label: label.clone(),
                event: event.clone(),
                prompts: true,
            });
        }
        RenderNode::Html { html } => {
            for l in html.lines() {
                r.lines.push(l.to_string());
            }
        }
    }
}

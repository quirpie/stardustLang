//! Núcleo de render del UI-IR: resuelve el árbol de [`Widget`] contra el estado
//! de un actor y produce un [`RenderNode`] **ya resuelto** (texto evaluado,
//! eventos con su payload concreto). Es host-agnóstico: el pintor de terminal
//! ([`crate::ui`]) y el pintor web (JS, vía `Engine::render`) consumen este mismo
//! árbol, de modo que la lógica de binding (listas, variables de host) vive en un
//! solo sitio y no puede divergir entre hosts.
//!
//! Las variables de host — `$input` (texto tecleado), `$name`/`$bytes` (archivo
//! soltado) — no existen en el estado del actor: dependen de la interacción. Por
//! eso se dejan como marcadores `{"$host":"input"|"name"|"bytes"}` dentro del
//! payload, y el host los rellena con [`fill_host`] justo antes de inyectar el
//! mensaje.

use std::collections::HashMap;

use serde::Serialize;

use crate::interpreter::eval;
use crate::program::{Expr, Widget};
use crate::value::{self, Value};

/// Un evento de UI resuelto: a qué actor va (`to = None` ⇒ el propio `Actor_UI`)
/// y el payload en JSON **etiquetado**. El payload puede contener marcadores
/// `{"$host":<nombre>}` que el host sustituye por el dato de la interacción.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub to: Option<String>,
    pub send: serde_json::Value,
}

/// Nodo de UI ya resuelto. Serializa a JSON para el pintor web; el pintor de
/// terminal lo consume directamente. El discriminante es el campo `kind`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RenderNode {
    Label {
        text: String,
    },
    Button {
        label: String,
        event: Event,
    },
    Row {
        children: Vec<RenderNode>,
    },
    Column {
        children: Vec<RenderNode>,
    },
    Grid {
        columns: usize,
        children: Vec<RenderNode>,
    },
    /// Lista ya **expandida**: un hijo por elemento (o el `empty` si estaba vacía).
    List {
        children: Vec<RenderNode>,
    },
    Input {
        value: String,
        placeholder: String,
        label: String,
        event: Event,
    },
    /// Imagen con sus bytes en base64 (`None` si `src` no resolvió a `Bytes`).
    Image {
        alt: String,
        src_b64: Option<String>,
    },
    #[serde(rename = "filedrop")]
    FileDrop {
        label: String,
        event: Event,
    },
    Textarea {
        value: String,
        placeholder: String,
        label: String,
        event: Event,
    },
    Html {
        html: String,
    },
}

/// Resuelve una vista contra la memoria (estado) de un actor.
pub fn build(view: &Widget, mem: &HashMap<String, Value>) -> RenderNode {
    match view {
        Widget::Label { text } => RenderNode::Label {
            text: eval_text(text, mem),
        },
        Widget::Button { label, send, to } => RenderNode::Button {
            label: eval_text(label, mem),
            event: Event {
                to: to.clone(),
                send: resolve_send(send, mem),
            },
        },
        Widget::Row { children } => RenderNode::Row {
            children: build_all(children, mem),
        },
        Widget::Column { children } => RenderNode::Column {
            children: build_all(children, mem),
        },
        Widget::Grid { columns, children } => RenderNode::Grid {
            columns: *columns,
            children: build_all(children, mem),
        },
        Widget::List {
            bind,
            as_var,
            index,
            item,
            empty,
        } => {
            let items = match eval(bind, mem) {
                Ok(Value::List(v)) => v,
                _ => Vec::new(),
            };
            if items.is_empty() {
                let children = empty
                    .as_ref()
                    .map(|e| vec![build(e, mem)])
                    .unwrap_or_default();
                return RenderNode::List { children };
            }
            let mut children = Vec::with_capacity(items.len());
            for (i, el) in items.into_iter().enumerate() {
                // Memoria aumentada: el elemento (y su índice) visibles solo en el
                // scope de la plantilla, sin tocar el estado real del actor.
                let mut m2 = mem.clone();
                m2.insert(as_var.clone(), el);
                if let Some(idx) = index {
                    m2.insert(idx.clone(), Value::Int(i as i64));
                }
                children.push(build(item, &m2));
            }
            RenderNode::List { children }
        }
        Widget::Input {
            src,
            placeholder,
            label,
            submit,
            to,
        } => RenderNode::Input {
            value: src.as_ref().map(|e| eval_text(e, mem)).unwrap_or_default(),
            placeholder: placeholder.clone().unwrap_or_default(),
            label: label.clone().unwrap_or_default(),
            event: Event {
                to: to.clone(),
                send: resolve_send(submit, mem),
            },
        },
        Widget::Image { src, alt } => {
            let src_b64 = match eval(src, mem) {
                Ok(Value::Bytes(b)) => Some(crate::crypto::base64_encode(&b)),
                _ => None,
            };
            RenderNode::Image {
                alt: alt.as_ref().map(|a| eval_text(a, mem)).unwrap_or_default(),
                src_b64,
            }
        }
        Widget::FileDrop { label, drop, to } => RenderNode::FileDrop {
            label: label
                .clone()
                .unwrap_or_else(|| "Arrastra archivos aquí".into()),
            event: Event {
                to: to.clone(),
                send: resolve_send(drop, mem),
            },
        },
        Widget::Textarea {
            src,
            placeholder,
            label,
            submit,
            to,
        } => RenderNode::Textarea {
            value: src.as_ref().map(|e| eval_text(e, mem)).unwrap_or_default(),
            placeholder: placeholder.clone().unwrap_or_default(),
            label: label.clone().unwrap_or_else(|| "Guardar".into()),
            event: Event {
                to: to.clone(),
                send: resolve_send(submit, mem),
            },
        },
        Widget::Html { src } => RenderNode::Html {
            html: eval_text(src, mem),
        },
    }
}

fn build_all(ws: &[Widget], mem: &HashMap<String, Value>) -> Vec<RenderNode> {
    ws.iter().map(|w| build(w, mem)).collect()
}

fn eval_text(e: &Expr, mem: &HashMap<String, Value>) -> String {
    eval(e, mem)
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "?".into())
}

/// Resuelve el `send` de un evento a JSON etiquetado, dejando las variables de
/// host (prefijo `$`) como marcadores `{"$host":<nombre>}`. Se recorre la
/// estructura de records/listas para que los marcadores puedan anidarse (p. ej.
/// `{"record":{"datos":{"var":"$bytes"}}}`); las demás sub-expresiones se
/// evalúan por completo contra el estado.
fn resolve_send(e: &Expr, mem: &HashMap<String, Value>) -> serde_json::Value {
    match e {
        Expr::Var { var } if var.starts_with('$') => {
            serde_json::json!({ "$host": &var[1..] })
        }
        Expr::Record { record } => serde_json::Value::Object(
            record
                .iter()
                .map(|(k, v)| (k.clone(), resolve_send(v, mem)))
                .collect(),
        ),
        Expr::List { list } => {
            serde_json::Value::Array(list.iter().map(|v| resolve_send(v, mem)).collect())
        }
        _ => match eval(e, mem) {
            Ok(v) => value::to_tagged(&v),
            Err(_) => serde_json::Value::Null,
        },
    }
}

/// Rellena los marcadores `{"$host":<nombre>}` de un payload con los valores de
/// la interacción y devuelve el [`Value`] listo para inyectar como mensaje. Un
/// payload sin marcadores (el caso de un botón normal) simplemente se des-etiqueta.
pub fn fill_host(send: &serde_json::Value, host: &HashMap<String, Value>) -> Value {
    value::from_tagged(&substitute(send, host))
}

fn substitute(j: &serde_json::Value, host: &HashMap<String, Value>) -> serde_json::Value {
    use serde_json::Value as J;
    match j {
        J::Object(m) => {
            if m.len() == 1 {
                if let Some(J::String(name)) = m.get("$host") {
                    return host.get(name).map(value::to_tagged).unwrap_or(J::Null);
                }
            }
            J::Object(
                m.iter()
                    .map(|(k, v)| (k.clone(), substitute(v, host)))
                    .collect(),
            )
        }
        J::Array(a) => J::Array(a.iter().map(|v| substitute(v, host)).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::Widget;

    fn widget(j: &str) -> Widget {
        serde_json::from_str(j).unwrap()
    }

    #[test]
    fn lista_expande_y_liga_cada_elemento() {
        let view = widget(
            r#"{ "type":"list", "bind":{"var":"archivos"}, "as":"x",
                 "item":{ "type":"button", "label":{"var":"x"},
                          "send":{"record":{"cmd":"ver","nombre":{"var":"x"}}} } }"#,
        );
        let mut mem = HashMap::new();
        mem.insert(
            "archivos".to_string(),
            Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]),
        );
        let j = serde_json::to_value(build(&view, &mem)).unwrap();
        assert_eq!(j["kind"], "list");
        let kids = j["children"].as_array().unwrap();
        assert_eq!(kids.len(), 2);
        // Cada botón obtiene su etiqueta y su payload ligados al elemento.
        assert_eq!(kids[0]["label"], "a");
        assert_eq!(kids[0]["event"]["send"]["nombre"], "a");
        assert_eq!(kids[1]["label"], "b");
        assert_eq!(kids[1]["event"]["send"]["nombre"], "b");
    }

    #[test]
    fn lista_vacia_cae_al_widget_empty() {
        let view = widget(
            r#"{ "type":"list", "bind":{"var":"xs"}, "as":"x",
                 "item":{"type":"label","text":{"var":"x"}},
                 "empty":{"type":"label","text":"nada"} }"#,
        );
        let j = serde_json::to_value(build(&view, &HashMap::new())).unwrap();
        let kids = j["children"].as_array().unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0]["text"], "nada");
    }

    #[test]
    fn filedrop_deja_marcadores_de_host_y_fill_host_los_rellena() {
        let view = widget(
            r#"{ "type":"filedrop", "label":"sube",
                 "drop":{"record":{"cmd":"guardar","nombre":{"var":"$name"},"datos":{"var":"$bytes"}}} }"#,
        );
        let j = serde_json::to_value(build(&view, &HashMap::new())).unwrap();
        let send = &j["event"]["send"];
        // Las variables de host quedan como marcadores, no se resuelven al render.
        assert_eq!(send["nombre"]["$host"], "name");
        assert_eq!(send["datos"]["$host"], "bytes");

        // El host las rellena -> un `Value` concreto listo para inyectar.
        let mut host = HashMap::new();
        host.insert("name".to_string(), Value::Str("f.png".into()));
        host.insert("bytes".to_string(), Value::Bytes(vec![1, 2, 3]));
        match fill_host(send, &host) {
            Value::Record(m) => {
                assert_eq!(m.get("cmd"), Some(&Value::Str("guardar".into())));
                assert_eq!(m.get("nombre"), Some(&Value::Str("f.png".into())));
                assert_eq!(m.get("datos"), Some(&Value::Bytes(vec![1, 2, 3])));
            }
            other => panic!("se esperaba un record, llegó {other}"),
        }
    }
}

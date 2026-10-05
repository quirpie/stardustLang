//! El sobre de mensaje entre actores. Vive en su propio módulo para que tanto el
//! planificador de terminal (`vm`) como el motor WASM (`wasm`) y el intérprete
//! lo compartan sin acoplarse entre sí.

use crate::value::Value;

/// Un mensaje asíncrono entre actores. Las direcciones son `actor` (local) o
/// `app/actor` (con app anfitriona). `cap` es el token de capacidad presentado
/// para cruzar la frontera de otra app.
#[derive(Debug, Clone)]
pub struct Message {
    pub from: String,
    pub to: String,
    pub payload: Value,
    pub cap: Option<String>,
}

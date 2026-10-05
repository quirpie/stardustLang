//! Núcleo de la StardustVM como librería, reutilizable por dos hosts:
//!   * el binario de terminal (`src/main.rs` + `vm` + `ui`), y
//!   * el motor WASM (`wasm`) que corre en el navegador — **el mismo intérprete**.
//!
//! Los módulos de terminal usan E/S de consola, así que se excluyen del build a
//! WebAssembly; el núcleo de tipos e intérprete es común a ambos.

pub mod check;
pub mod crypto;
pub mod date;
pub mod error;
pub mod interpreter;
pub mod lang;
pub mod message;
pub mod program;
pub mod render;
pub mod value;
pub mod wire;

#[cfg(not(target_arch = "wasm32"))]
pub mod ui;
#[cfg(not(target_arch = "wasm32"))]
pub mod vm;

#[cfg(feature = "wasm")]
pub mod wasm;

/// Servidor de programas (`stardust-server`); ver `docs/servidor.md`.
#[cfg(all(feature = "server", not(target_arch = "wasm32")))]
pub mod server;

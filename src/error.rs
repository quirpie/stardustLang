//! Errores de ejecución compartidos por el intérprete y el sistema de tipos.
//!
//! Cualquier variante aquí es tratada por la StardustVM como un **fallo aislado**
//! del actor: se registra, el actor se reinicia y el resto del sistema continúa.

#[derive(Debug)]
pub enum RuntimeError {
    UndefinedVar(String),
    TypeError(String),
    DivByZero,
    CapabilityDenied(String),
    UnknownActor(String),
    BadOperator(String),
    InfiniteLoop,
    /// Literal tipado inválido, p. ej. `{"lit":"2026-13-40","as":"date"}`.
    BadLiteral(String),
    /// Nombre de tipo desconocido en un literal tipado.
    UnknownType(String),
    /// `CALL` a un procedimiento que el actor no declara.
    UnknownProc(String),
    /// Número de argumentos distinto al de parámetros del procedimiento.
    ArityMismatch {
        proc: String,
        expected: usize,
        got: usize,
    },
    /// Recursión demasiado profunda (protección contra recursión infinita).
    CallDepthExceeded,
    Io(String),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeError::UndefinedVar(v) => write!(f, "variable no definida: '{v}'"),
            RuntimeError::TypeError(m) => write!(f, "error de tipo: {m}"),
            RuntimeError::DivByZero => write!(f, "división por cero"),
            RuntimeError::CapabilityDenied(c) => {
                write!(f, "capacidad denegada: el actor no posee '{c}'")
            }
            RuntimeError::UnknownActor(a) => write!(f, "actor destino inexistente: '{a}'"),
            RuntimeError::BadOperator(o) => write!(f, "operador no soportado: '{o}'"),
            RuntimeError::InfiniteLoop => write!(f, "bucle excedió el tope de seguridad"),
            RuntimeError::BadLiteral(m) => write!(f, "literal inválido: {m}"),
            RuntimeError::UnknownType(t) => write!(f, "tipo desconocido: '{t}'"),
            RuntimeError::UnknownProc(p) => write!(f, "procedimiento no declarado: '{p}'"),
            RuntimeError::ArityMismatch {
                proc,
                expected,
                got,
            } => write!(
                f,
                "'{proc}' espera {expected} argumento(s), recibió {got}"
            ),
            RuntimeError::CallDepthExceeded => {
                write!(f, "recursión excedió el tope de profundidad")
            }
            RuntimeError::Io(m) => write!(f, "fallo de E/S: {m}"),
        }
    }
}

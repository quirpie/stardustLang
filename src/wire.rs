//! Protocolo de cable para la mensajería entre nodos (`actor://`).
//!
//! Un mensaje viaja como un **sobre firmado asimétricamente** (Ed25519):
//! `{"body":"<json interno>","pk":"<clave pública>","sig":"<firma>"}`. El emisor
//! firma los bytes **exactos** del `body` con su **clave privada**; el receptor
//! verifica con la **clave pública** incluida y, además, comprueba que esa clave
//! esté en su lista de **autorizadas** (estilo `authorized_keys`).
//!
//! Frente al HMAC de secreto compartido, esto no requiere un secreto común: el
//! nodo `--serve` decide quién puede hablar por su clave pública, y comprometer un
//! nodo no permite firmar en nombre de otro. Es host-agnóstico: nativo y navegador
//! usan estas mismas funciones (la firma Ed25519 es determinista, sin RNG).

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use crate::crypto;
use crate::value::{self, Value};

/// Semilla (clave privada) de identidad de demostración: sirve para que los demos
/// funcionen sin configurar claves. En producción cada nodo tendría la suya.
pub const DEMO_SEED: [u8; 32] = *b"stardust-demo-node-identity-seed";

/// Decodifica una cadena hex a bytes.
fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

/// Decodifica una clave/semilla de 32 bytes desde hex (64 caracteres).
pub fn key32_from_hex(s: &str) -> Option<[u8; 32]> {
    from_hex(s).and_then(|b| b.try_into().ok())
}

/// Clave pública (32 bytes) derivada de una semilla privada.
pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

/// Clave pública en hex (para configurar allowlists o mostrarla).
pub fn public_key_hex(seed: &[u8; 32]) -> String {
    crypto::to_hex(&public_key(seed))
}

/// Construye el JSON interno de un mensaje (`from`/`to`/`cap`/`payload` etiquetado).
pub fn inner(from: &str, to: &str, cap: &Option<String>, payload: &Value) -> String {
    serde_json::json!({
        "from": from,
        "to": to,
        "cap": cap,
        "payload": value::to_tagged(payload),
    })
    .to_string()
}

/// Firma un `body` interno con la semilla privada y lo envuelve para el cable:
/// `{"body":..,"pk":<pública hex>,"sig":<firma hex>}`.
pub fn wrap_signed(seed: &[u8; 32], body: &str) -> String {
    let sk = SigningKey::from_bytes(seed);
    let sig = sk.sign(body.as_bytes());
    serde_json::json!({
        "body": body,
        "pk": crypto::to_hex(&sk.verifying_key().to_bytes()),
        "sig": crypto::to_hex(&sig.to_bytes()),
    })
    .to_string()
}

/// Desenvuelve y **verifica** un sobre firmado. Comprueba que la firma corresponde
/// al `body` bajo la clave pública incluida y, si `authorized` es `Some`, que esa
/// clave esté autorizada. Devuelve el `body` interno solo si todo cuadra.
///
/// `authorized = None` acepta cualquier firma **válida** (autenticidad/integridad
/// sin control de acceso), útil para verificar respuestas de un nodo al que uno ya
/// decidió llamar. `authorized = Some(lista)` exige además pertenencia.
pub fn unwrap_verified(wire: &[u8], authorized: Option<&[[u8; 32]]>) -> Result<String, String> {
    let v: serde_json::Value =
        serde_json::from_slice(wire).map_err(|e| format!("sobre ilegible: {e}"))?;
    let body = v.get("body").and_then(|b| b.as_str()).ok_or("sobre sin 'body'")?;
    let pk_hex = v.get("pk").and_then(|s| s.as_str()).ok_or("sobre sin 'pk'")?;
    let sig_hex = v.get("sig").and_then(|s| s.as_str()).ok_or("sobre sin 'sig'")?;

    let pk_arr: [u8; 32] = from_hex(pk_hex)
        .and_then(|b| b.try_into().ok())
        .ok_or("clave pública mal formada")?;
    let sig_bytes = from_hex(sig_hex).ok_or("firma no es hex")?;
    let vk = VerifyingKey::from_bytes(&pk_arr).map_err(|_| "clave pública inválida")?;
    let sig = Signature::from_slice(&sig_bytes).map_err(|_| "firma mal formada")?;
    vk.verify(body.as_bytes(), &sig)
        .map_err(|_| "firma inválida (no corresponde a la clave)".to_string())?;

    if let Some(list) = authorized {
        if !list.iter().any(|k| k == &pk_arr) {
            return Err("clave pública no autorizada".into());
        }
    }
    Ok(body.to_string())
}

/// Extrae el `payload` (des-etiquetado) de un `body` interno.
pub fn payload_of(inner_body: &str) -> Option<Value> {
    let v: serde_json::Value = serde_json::from_str(inner_body).ok()?;
    v.get("payload").map(value::from_tagged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firma_asimetrica_roundtrip_autorizacion_y_alteracion() {
        let seed = DEMO_SEED;
        let pk = public_key(&seed);
        let body = inner("@caller", "Saludador", &None, &Value::Int(42));
        let envelope = wrap_signed(&seed, &body);

        // Firma válida y clave autorizada -> se acepta.
        assert_eq!(
            unwrap_verified(envelope.as_bytes(), Some(&[pk])).as_deref(),
            Ok(body.as_str())
        );
        // Firma válida sin exigir autorización -> se acepta.
        assert!(unwrap_verified(envelope.as_bytes(), None).is_ok());

        // Firma válida pero clave NO autorizada -> se rechaza.
        let otra_pk = public_key(b"otra-semilla-de-identidad-distin");
        assert!(unwrap_verified(envelope.as_bytes(), Some(&[otra_pk])).is_err());

        // Body alterado (misma firma) -> se rechaza.
        let tampered = envelope.replace("Saludador", "Atacante");
        assert!(unwrap_verified(tampered.as_bytes(), None).is_err());

        // Firma de otra clave privada -> no verifica contra su pk declarada? La pk
        // va en el sobre, así que un atacante puede re-firmar con SU par de claves;
        // por eso el control de acceso (allowlist) es lo que autentica de verdad.
        let atacante = wrap_signed(b"otra-semilla-de-identidad-distin", &body);
        assert!(unwrap_verified(atacante.as_bytes(), None).is_ok()); // firma válida...
        assert!(unwrap_verified(atacante.as_bytes(), Some(&[pk])).is_err()); // ...pero no autorizada
    }
}

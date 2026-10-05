#!/usr/bin/env bash
# Ensambla la demo web de persistencia (contador + localStorage) en un HTML único.
# Requisitos: rustup target add wasm32-unknown-unknown ; cargo install wasm-bindgen-cli
set -euo pipefail
cd "$(dirname "$0")/.."

# Python 3 en UTF-8 (en Windows `python3` puede ser el stub de la Store y la
# codificación por defecto no es UTF-8).
PY=python3; "$PY" -c "" 2>/dev/null || PY=python
export PYTHONUTF8=1

echo "1/3  Compilando el núcleo a wasm32…"
cargo build --release --lib --features wasm --target wasm32-unknown-unknown

echo "2/3  Generando el glue (wasm-bindgen --target web)…"
wasm-bindgen --target web --out-dir web/pkg target/wasm32-unknown-unknown/release/stardust_vm.wasm

echo "3/3  Ensamblando web/persist.html…"
"$PY" - <<'PY'
import base64, pathlib
prog = pathlib.Path("programs/contador_ui.json").read_text().strip()
glue = pathlib.Path("web/pkg/stardust_vm.js").read_text()
runtime = pathlib.Path("web/runtime.js").read_text()
wasm_b64 = base64.b64encode(pathlib.Path("web/pkg/stardust_vm_bg.wasm").read_bytes()).decode()
tpl = pathlib.Path("web/persist.template.html").read_text()
html = (tpl.replace("__PROGRAM__", prog).replace("/*__GLUE__*/", glue).replace("/*__RUNTIME__*/", runtime).replace("__WASM__", wasm_b64))
for m in ("__PROGRAM__", "/*__GLUE__*/", "/*__RUNTIME__*/", "__WASM__"):
    assert m not in html, f"placeholder {m} sin sustituir"
pathlib.Path("web/persist.html").write_text(html)
print(f"    OK  web/persist.html  ({len(html)/1024:.0f} KB)")
PY
echo "Listo. Abre web/persist.html; pulsa +1 y recarga: el valor persiste."

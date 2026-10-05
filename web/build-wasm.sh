#!/usr/bin/env bash
# Compila la StardustVM a WebAssembly y ensambla un renderer web autocontenido
# (HTML único con el .wasm embebido en base64, sin fetch — apto para CSP estricto).
#
# Requisitos (una vez):
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli   # versión = la del crate en Cargo.toml
#
# Uso:  ./web/build-wasm.sh  [programa.json]
set -euo pipefail
cd "$(dirname "$0")/.."

# Python 3 en UTF-8 (en Windows `python3` puede ser el stub de la Store y la
# codificación por defecto no es UTF-8).
PY=python3; "$PY" -c "" 2>/dev/null || PY=python
export PYTHONUTF8=1

PROGRAM="${1:-programs/calculadora.json}"
OUT="web/renderer-wasm.html"

echo "1/3  Compilando el núcleo a wasm32…"
cargo build --release --lib --features wasm --target wasm32-unknown-unknown

echo "2/3  Generando el glue (wasm-bindgen --target web)…"
wasm-bindgen --target web --out-dir web/pkg \
  target/wasm32-unknown-unknown/release/stardust_vm.wasm

echo "3/3  Ensamblando $OUT (con $PROGRAM embebido)…"
"$PY" - "$PROGRAM" "$OUT" <<'PY'
import base64, sys, pathlib
program_path, out_path = sys.argv[1], sys.argv[2]
program = pathlib.Path(program_path).read_text().strip()
glue = pathlib.Path("web/pkg/stardust_vm.js").read_text()
runtime = pathlib.Path("web/runtime.js").read_text()
wasm_b64 = base64.b64encode(pathlib.Path("web/pkg/stardust_vm_bg.wasm").read_bytes()).decode()
tpl = pathlib.Path("web/renderer-wasm.template.html").read_text()
html = (tpl.replace("__PROGRAM__", program)
           .replace("/*__GLUE__*/", glue).replace("/*__RUNTIME__*/", runtime)
           .replace("__WASM__", wasm_b64))
for m in ("__PROGRAM__", "/*__GLUE__*/", "/*__RUNTIME__*/", "__WASM__"):
    assert m not in html, f"placeholder {m} sin sustituir"
pathlib.Path(out_path).write_text(html)
print(f"    OK  {out_path}  ({len(html)/1024:.0f} KB)")
PY
echo "Listo. Abre $OUT en un navegador."

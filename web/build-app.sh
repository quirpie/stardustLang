#!/usr/bin/env bash
# Ensambla CUALQUIER programa StardustLang con vista en un HTML autónomo, usando el
# renderer genérico (web/app.template.html): pinta lo que devuelve engine.render()
# y no lleva lógica de UI propia. El mismo programa corre igual en la terminal.
#
# Uso:  web/build-app.sh <programa.json> [salida.html]
#       (por defecto la salida es web/<nombre-del-programa>.html)
# Requisitos: rustup target add wasm32-unknown-unknown ; cargo install wasm-bindgen-cli
set -euo pipefail
cd "$(dirname "$0")/.."

# Python 3 en UTF-8 (en Windows `python3` puede ser el stub de la Store y la
# codificación por defecto no es UTF-8).
PY=python3; "$PY" -c "" 2>/dev/null || PY=python
export PYTHONUTF8=1

PROG="${1:?uso: web/build-app.sh <programa.json> [salida.html]}"
OUT="${2:-web/$(basename "${PROG%.json}").html}"

echo "1/3  Compilando el núcleo a wasm32…"
cargo build --release --lib --features wasm --target wasm32-unknown-unknown

echo "2/3  Generando el glue (wasm-bindgen --target web)…"
wasm-bindgen --target web --out-dir web/pkg target/wasm32-unknown-unknown/release/stardust_vm.wasm

echo "3/3  Ensamblando $OUT…"
PROG="$PROG" OUT="$OUT" "$PY" - <<'PY'
import base64, os, pathlib
prog = pathlib.Path(os.environ["PROG"]).read_text().strip()
out  = pathlib.Path(os.environ["OUT"])
glue = pathlib.Path("web/pkg/stardust_vm.js").read_text()
runtime = pathlib.Path("web/runtime.js").read_text()
wasm_b64 = base64.b64encode(pathlib.Path("web/pkg/stardust_vm_bg.wasm").read_bytes()).decode()
tpl = pathlib.Path("web/app.template.html").read_text()
html = (tpl.replace("__PROGRAM__", prog).replace("/*__GLUE__*/", glue)
           .replace("/*__RUNTIME__*/", runtime).replace("__WASM__", wasm_b64))
for m in ("__PROGRAM__", "/*__GLUE__*/", "/*__RUNTIME__*/", "__WASM__"):
    assert m not in html, f"placeholder {m} sin sustituir"
out.write_text(html)
print(f"    OK  {out}  ({len(html)/1024:.0f} KB)")
PY
echo "Listo. Abre $OUT en el navegador."

#!/usr/bin/env bash
# Ensambla el Playground de StardustLang en HTML autónomos:
#   playground.html — editor + biblioteca + escritorio. Abierto suelto funciona en
#                     modo local (Biblioteca en IndexedDB); servido por
#                     stardust-server, en modo servidor (API, sesión, versiones).
#   runner.html     — ejecuta UN programa dentro de un iframe, en el origen del
#                     runner (solo lo usa el modo servidor).
# Ver docs/servidor.md, «Playground y runner».
#
# Uso:  web/build-playground.sh [salida.html]   (por defecto web/playground.html;
#       runner.html se escribe en el mismo directorio)
# Requisitos: rustup target add wasm32-unknown-unknown ; cargo install wasm-bindgen-cli
set -euo pipefail
cd "$(dirname "$0")/.."

# Python 3 en UTF-8 (en Windows `python3` puede ser el stub de la Store y la
# codificación por defecto no es UTF-8).
PY=python3; "$PY" -c "" 2>/dev/null || PY=python
export PYTHONUTF8=1

OUT="${1:-web/playground.html}"

echo "1/3  Compilando el núcleo a wasm32…"
cargo build --release --lib --features wasm --target wasm32-unknown-unknown

echo "2/3  Generando el glue (wasm-bindgen --target web)…"
wasm-bindgen --target web --out-dir web/pkg target/wasm32-unknown-unknown/release/stardust_vm.wasm

echo "3/3  Ensamblando $OUT y runner.html…"
OUT="$OUT" "$PY" - <<'PY'
import base64, os, pathlib
out  = pathlib.Path(os.environ["OUT"])
glue = pathlib.Path("web/pkg/stardust_vm.js").read_text()
runtime = pathlib.Path("web/runtime.js").read_text()
css = pathlib.Path("web/stardust.css").read_text()
biblio = pathlib.Path("programs/biblioteca.json").read_text().strip()
wasm_b64 = base64.b64encode(pathlib.Path("web/pkg/stardust_vm_bg.wasm").read_bytes()).decode()
assert "`" not in biblio and "${" not in biblio, "biblioteca.json no puede contener ` ni ${ (va en un template literal)"

def build(template, target, extra=()):
    html = (pathlib.Path(template).read_text().replace("/*__STARDUST_CSS__*/", css)
            .replace("/*__GLUE__*/", glue).replace("/*__RUNTIME__*/", runtime))
    for marker, value in extra:
        html = html.replace(marker, value)
    html = html.replace("__WASM__", wasm_b64)
    for m in ("/*__STARDUST_CSS__*/", "/*__GLUE__*/", "/*__RUNTIME__*/", "__BIBLIO__", "__WASM__"):
        assert m not in html, f"{target}: placeholder {m} sin sustituir"
    # Lo sustituye el servidor al servir la página (en modo local queda null).
    assert html.count("/*__STARDUST_CONFIG__*/null") == 1, f"{target}: falta el marcador de configuración"
    target.write_text(html)
    print(f"    OK  {target}  ({len(html)/1024:.0f} KB)")

build("web/playground.template.html", out, [("__BIBLIO__", biblio)])
build("web/runner.template.html", out.parent / "runner.html")
PY
echo "Listo. Abre $OUT en el navegador (sírvelo por HTTP para IndexedDB/módulos),"
echo "o sírvelo con stardust-server (STARDUST_WEB=$(dirname "$OUT")) para el modo servidor."

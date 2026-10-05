#!/usr/bin/env bash
# Ensambla la skill de StardustLang para Google AI Edge Gallery (JS skill) en
# dist/skills/stardustlang/, lista para importar en el teléfono o publicar en la web.
#
#   dist/skills/stardustlang/
#   ├── SKILL.md              instrucciones para el modelo (Gemma)
#   ├── index.html            página de instalación (URL + QR)
#   ├── scripts/index.html    validador oculto: StardustVM WASM + check_program()
#   └── assets/webview.html   la app generada, funcionando en el chat
#
# Cada HTML lleva el .wasm embebido en base64 (sin fetch de ficheros locales, que
# el webview puede bloquear), el glue de wasm-bindgen y web/runtime.js.
#
# Uso:  skills/build-skill.sh
#       SKILL_URL=https://… skills/build-skill.sh   (fija la URL pública en index.html)
# Requisitos: rustup target add wasm32-unknown-unknown ; cargo install wasm-bindgen-cli
set -euo pipefail
cd "$(dirname "$0")/.."

# Python 3 en UTF-8 (en Windows `python3` puede ser el stub de la Store y la
# codificación por defecto no es UTF-8).
PY=python3; "$PY" -c "" 2>/dev/null || PY=python
export PYTHONUTF8=1

OUT="dist/skills/stardustlang"

echo "1/3  Compilando el núcleo a wasm32…"
cargo build --release --lib --features wasm --target wasm32-unknown-unknown

echo "2/3  Generando el glue (wasm-bindgen --target web)…"
wasm-bindgen --target web --out-dir web/pkg target/wasm32-unknown-unknown/release/stardust_vm.wasm

echo "3/3  Ensamblando $OUT…"
OUT="$OUT" "$PY" - <<'PY'
import base64, os, pathlib, shutil
out = pathlib.Path(os.environ["OUT"])
src = pathlib.Path("skills/stardustlang")
glue = pathlib.Path("web/pkg/stardust_vm.js").read_text()
runtime = pathlib.Path("web/runtime.js").read_text()
wasm_b64 = base64.b64encode(pathlib.Path("web/pkg/stardust_vm_bg.wasm").read_bytes()).decode()

if out.exists():
    shutil.rmtree(out)
(out / "scripts").mkdir(parents=True)
(out / "assets").mkdir(parents=True)
shutil.copy(src / "SKILL.md", out / "SKILL.md")
# Página de instalación (URL + QR). La URL real se fija con SKILL_URL al construir.
landing = (src / "index.html").read_text()
if os.environ.get("SKILL_URL"):
    landing = landing.replace("__SKILL_URL__", os.environ["SKILL_URL"].rstrip("/"))
(out / "index.html").write_text(landing)
# GitHub Pages: sin Jekyll, para que sirva SKILL.md crudo.
(out.parent / ".nojekyll").write_text("")
# Cloudflare (Workers/Pages) sirve .md como application/octet-stream; que sea texto.
(out.parent / "_headers").write_text("/*.md\n  Content-Type: text/markdown; charset=utf-8\n")
# Raíz del sitio: sin esto, abrir el dominio (p. ej. «Visit» en Cloudflare) da 404,
# porque la skill vive en la subcarpeta stardustlang/.
(out.parent / "index.html").write_text(
    '<!doctype html><meta charset="utf-8"><title>Skills StardustLang</title>'
    '<meta http-equiv="refresh" content="0; url=stardustlang/">'
    '<p>La skill está en <a href="stardustlang/">stardustlang/</a>.</p>\n')

for tpl, dst in [("scripts/index.template.html", "scripts/index.html"),
                 ("assets/webview.template.html", "assets/webview.html")]:
    html = ((src / tpl).read_text().replace("/*__GLUE__*/", glue)
            .replace("/*__RUNTIME__*/", runtime).replace("__WASM__", wasm_b64))
    for m in ("/*__GLUE__*/", "/*__RUNTIME__*/", "__WASM__"):
        assert m not in html, f"placeholder {m} sin sustituir en {tpl}"
    (out / dst).write_text(html)
    print(f"    OK  {out / dst}  ({len(html)/1024:.0f} KB)")
print(f"    OK  {out / 'SKILL.md'}")
PY
echo "Listo. Instálala en AI Edge Gallery: copia $OUT al teléfono"
echo "  (adb push $OUT /sdcard/Download/) y usa «Import local skill», o publícala"
echo "  en un hosting web (GitHub Pages…) y usa «Load skill from URL»."

#!/usr/bin/env bash
# Reconstruye TODAS las páginas web que embeben el wasm, con el núcleo actual.
#
# Cada página autónoma congela un snapshot del .wasm en tiempo de build; al cambiar
# el núcleo Rust (nuevas instrucciones, ops o widgets) hay que regenerarlas o darán
# errores de parseo con programas que usen lo nuevo. Ejecuta esto tras tocar `src/`.
set -euo pipefail
cd "$(dirname "$0")/.."

# El primer build compila el wasm; los siguientes reusan la caché de cargo (rápidos).
echo "== Playground =="
web/build-playground.sh

for prog in explorador calculadora red_widgets markdown; do
  echo "== $prog =="
  web/build-app.sh "programs/$prog.json"
done

echo
echo "Listo. Todas las páginas del renderer genérico + el playground están al día."

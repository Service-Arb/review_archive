#!/usr/bin/env bash
# The bundle as the server serves it under /mfe/: wasm-bindgen's module, the entry that
# registers the element, its registry manifest, the stylesheet.
# $1: the built .wasm; $2: the directory it goes to.
set -euo pipefail
wasm="$1"
out="$2"
here="$(dirname "$0")"
mkdir -p "$out"
wasm-bindgen --target web --out-dir "$out" --out-name review_archive_web "$wasm"
printf 'import init from "./review_archive_web.js";\nawait init();\n' >"$out/mfe-review-archive-dashboard.js"
printf '%s\n' '{"name":"review-archive.dashboard","tag":"mfe-review-archive-dashboard","kind":"page"}' >"$out/mfe.json"
tailwindcss -i "$here/mfe.css" -o "$out/mfe.css"

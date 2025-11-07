#!/bin/bash
set -e
wasm-pack build --target web -s filen --out-name sdk-rs --out-dir web/tmp --profile web-release --no-pack . -- -F wasm-full -Z build-std=panic_abort,std
rm web/tmp/.gitignore
rsync -av web/tmp/ web
rm -rf web/tmp
# don't need support for atomics in service worker
export RUSTFLAGS="-C target-feature=-nontrapping-fptoint,+bulk-memory --cfg getrandom_backend=\"wasm_js\""
wasm-pack build --target web -s filen --out-name sdk-rs --out-dir web/tmp --profile web-release --no-pack . -- --no-default-features -F service-worker -Z build-std=panic_abort,std
rm web/tmp/.gitignore
rsync -av web/tmp/ web/service-worker
rm -rf web/tmp

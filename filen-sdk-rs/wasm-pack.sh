set -e
wasm-pack build --target web -s filen --out-name sdk-rs --out-dir web/tmp --profile web-release --no-pack . -- -Z build-std=panic_abort,std
rm web/tmp/.gitignore
rsync -av web/tmp/ web/browser
rm -rf web/tmp

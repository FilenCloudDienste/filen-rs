set -e
wasm-pack build --target web -s filen --out-name sdk-rs --out-dir web/browser --profile web-release --no-pack . -- -Z build-std=panic_abort,std
rm web/browser/.gitignore
rsync -av web/browser/ web/
rm -rf web/browser

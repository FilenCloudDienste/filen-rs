#!/usr/bin/env zsh

# adapted from https://github.com/ianthetechie/uniffi-starter/blob/main/rust/build-ios.sh

set -e
set -u

# NOTE: You MUST run this every time you make changes to the core. Unfortunately, calling this from Xcode directly
# does not work so well.

# In release mode, we create a ZIP archive of the xcframework and update Package.swift with the computed checksum.
# This is only needed when cutting a new release, not for local development.
release=false

for arg in "$@"
do
    case $arg in
        --release)
            release=true
            shift # Remove --release from processing
            ;;
        *)
            shift # Ignore other argument from processing
            ;;
    esac
done


# Potential optimizations for the future:
#
# * Option to do debug builds instead for local development

generate_ffi() {
  echo "Generating framework module mapping and FFI bindings"
  # NOTE: Convention requires the modulemap be named module.modulemap
  cargo run -p uniffi-bindgen-swift -- target/aarch64-apple-ios/release/lib$1.a target/uniffi-xcframework-staging --swift-sources --headers --modulemap --module-name $1FFI --modulemap-filename module.modulemap

  mv target/uniffi-xcframework-staging/module.modulemap target/uniffi-xcframework-staging/module.modulemap
}

build_xcframework() {
  # Builds an XCFramework
  echo "Generating XCFramework"
  rm -rf target/ios  # Delete the output folder so we can regenerate it
  xcodebuild -create-xcframework \
    -library target/aarch64-apple-ios/release/lib$1.a -headers target/uniffi-xcframework-staging \
    -library target/aarch64-apple-ios-sim/release/lib$1.a -headers target/uniffi-xcframework-staging \
    -output target/ios/lib$1.xcframework

  if $release; then
    echo "Building xcframework archive"
    ditto -c -k --sequesterRsrc --keepParent target/ios/lib$1.xcframework target/ios/lib$1.xcframework.zip
    checksum=$(swift package compute-checksum target/ios/lib$1.xcframework.zip)
    version=$(cargo metadata --format-version 1 | jq -r --arg pkg_name "$1" '.packages[] | select(.name==$pkg_name) .version')
    # sed -i "" -E "s/(let releaseTag = \")[^\"]+(\")/\1$version\2/g" ../Package.swift
    # sed -i "" -E "s/(let releaseChecksum = \")[^\"]+(\")/\1$checksum\2/g" ../Package.swift
  fi
}

basename=filen-mobile-native-cache
lib_name=$(echo "$basename" | tr '-' '_')

cargo build -p $basename --lib --release --target aarch64-apple-ios-sim
cargo build -p $basename --lib --release --target aarch64-apple-ios

generate_ffi $lib_name
build_xcframework $lib_name

#!/usr/bin/env bash
# Downloads the rclone release the managed installation pins
# (filen-sdk-rs/src/rclone/binary/managed.rs) for this machine into <dir>, checks the zip against
# the release's SHA256SUMS line, and prints the binary's path (a Windows path under Git Bash).
# The tests that run a real rclone take it as FILEN_TEST_RCLONE.
#
#   scripts/fetch-rclone.sh <dir>

set -euo pipefail

dir=${1:?usage: fetch-rclone.sh <dir>}
version=1.74.4
exe=""
case "$(uname -s)-$(uname -m)" in
	Linux-x86_64) platform=linux-amd64 sum=fe435e0c36228e7c2f116a8701f01127bb1f694005fc11d1f27186c8bca4115d ;;
	Linux-aarch64) platform=linux-arm64 sum=97685285c9ad6a0cf17d5844115d2a67245af6444db672187074bd9c358de419 ;;
	Darwin-x86_64) platform=osx-amd64 sum=4188aa84043d7a6240912923f47639a9d2da21f3b40a521c065c8d92e66563f6 ;;
	Darwin-arm64) platform=osx-arm64 sum=c2100e2d4a4b3be04c55cd45380cafe7647e1ad772bb055f52f00876ed701167 ;;
	MINGW*-x86_64 | MSYS*-x86_64) platform=windows-amd64 sum=ef097ef9de37a57feb7d9f9c7afb34148ad3c65be8025f1d8f7f521554a701ea exe=.exe ;;
	MINGW*-aarch64 | MSYS*-aarch64) platform=windows-arm64 sum=72194ad0aaf210d7a55808801191fecc7e175444dab7be7491b7a63074521f3a exe=.exe ;;
	*)
		echo "no rclone release for $(uname -s) $(uname -m)" >&2
		exit 1
		;;
esac

name=rclone-v$version-$platform
mkdir -p "$dir"
curl -sSfL --retry 3 -o "$dir/$name.zip" \
	"https://github.com/rclone/rclone/releases/download/v$version/$name.zip"
if command -v sha256sum >/dev/null 2>&1; then
	actual=$(sha256sum "$dir/$name.zip" | cut -d' ' -f1)
else
	actual=$(shasum -a 256 "$dir/$name.zip" | cut -d' ' -f1)
fi
if [ "$actual" != "$sum" ]; then
	echo "$name.zip has SHA-256 $actual, expected $sum" >&2
	exit 1
fi
unzip -q -o "$dir/$name.zip" "$name/rclone$exe" -d "$dir"

binary=$dir/$name/rclone$exe
if command -v cygpath >/dev/null 2>&1; then
	binary=$(cygpath -w "$binary")
fi
echo "$binary"

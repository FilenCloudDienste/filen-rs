#!/usr/bin/env bash

# This script exports the API key using a temporary download of the filen-cli binary
# It is especially useful for filen-rclone

COLOR_OFF='\033[0m'
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BOLD_PURPLE='\033[1;35m'
CHECK_MARK="${GREEN}\u2714${COLOR_OFF}"
CROSS_MARK="${RED}\u2718${COLOR_OFF}"
WARNING_SIGN="${YELLOW}\u26A0${COLOR_OFF}"
# see https://stackoverflow.com/a/28938235/13164753

# determine platform as "linux" or "macos"
if [[ "$(uname -s)" == "Linux" ]] ; then
  platform=linux
elif [[ "$(uname -s)" == "Darwin" ]] ; then
  platform=macos
else
  echo -e "${CROSS_MARK} Unsupported operating system: $(uname -s)"
  exit 1
fi

# determine glibc vs musl
if [[ $platform == "linux" ]] ; then
  if ldd --version 2>&1 | grep -q musl ; then
    platform="${platform}-musl"
  else
    platform="${platform}-gnu"
  fi
fi

# determine architecture as "x64" or "arm64"
if [[ "$(uname -m)" == "aarch64" || "$(uname -m)" == "arm64" ]] ; then
  arch=aarch64
else
  arch=x86_64
fi

# fetch release info
latest_release=$(curl -s https://api.github.com/repos/FilenCloudDienste/filen-cli-releases/releases/latest)
version=$(echo "$latest_release" | grep "tag_name" | cut -d \" -f 4)
download_url=$(echo "$latest_release" | grep "browser_download_url.*$arch.*$platform" | cut -d \" -f 4)

if [[ ${#download_url} == 0 ]] ; then

  echo -e "${CROSS_MARK} Filen CLI $version is not available for $platform, $arch"
  exit 1

else

  echo -e "${BOLD_PURPLE}Downloading Filen CLI v$version ($platform, $arch)${COLOR_OFF}"

  # prepare install location ~/.filen-cli
  if [ ! -d ~/.filen-cli ] ; then mkdir -p ~/.filen-cli/bin ; fi

  # temporary download and install location
  download_location=$(mktemp -d)

  # download binary and make executable
  echo "Downloading $download_url to temporary location..."
  curl -L -s -S $download_url --output $download_location/filen
  chmod +x $download_location/filen
  echo -e "${CHECK_MARK} ${GREEN}Filen CLI downloaded${COLOR_OFF}"

  # export API key using the downloaded binary
  echo -e "${BOLD_PURPLE}Exporting API key...${COLOR_OFF} (when asked, you don't need to \"keep me logged in\")"
  $download_location/filen export-api-key

  rm -r $download_location

fi
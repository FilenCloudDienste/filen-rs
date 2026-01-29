#!/usr/bin/env bash

COLOR_OFF='\033[0m'
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BOLD_PURPLE='\033[1;35m'
CHECK_MARK="${GREEN}\u2714${COLOR_OFF}"
CROSS_MARK="${RED}\u2718${COLOR_OFF}"
WARNING_SIGN="${YELLOW}\u26A0${COLOR_OFF}"
# see https://stackoverflow.com/a/28938235/13164753

# check if filen-cli is already installed
# especially if this is the old filen-cli
if [[ ! -z $(which filen) ]] ; then
  echo -e "${WARNING_SIGN} Filen CLI is already installed"
  echo "To reinstall, please first uninstall the existing version."
else

  # determine platform
  if [[ "$(uname -s)" == "Linux" ]] ; then
    platform=linux
  elif [[ "$(uname -s)" == "Darwin" ]] ; then
    platform=darwin
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

  # determine architecture
  if [[ "$(uname -m)" == "aarch64" || "$(uname -m)" == "arm64" ]] ; then
    arch=aarch64
  else
    arch=x86_64
  fi

  # for darwin-aarch64, use the darwin-x86_64 binary instead (might be changed later)
  if [[ $platform == "darwin" && $arch == "aarch64" ]] ; then
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

    echo -e "${BOLD_PURPLE}Installing Filen CLI v$version ($platform, $arch)${COLOR_OFF}"

    # prepare install location ~/.filen-cli
    if [ ! -d ~/.filen-cli ] ; then mkdir -p ~/.filen-cli/bin ; fi

    # download binary and make executable
    echo "Downloading $download_url..."
    curl -L -s -S $download_url --output ~/.filen-cli/bin/filen
    chmod +x ~/.filen-cli/bin/filen

    # add to PATH
    if [[ $PATH == *$(echo ~)/\.filen-cli* ]] ; then
      echo -e "${CHECK_MARK} \$PATH already contains ~/.filen-cli"
    else
      export PATH=$PATH:~/.filen-cli/bin
      profileFileFound=0
      for profileFile in ~/.bashrc ~/.bash_profile ~/.zshrc ~/.profile ; do
        if [[ -f $profileFile ]] ; then
          profileFileFound=1
          printf "\n\n# filen-cli\nPATH=\$PATH:~/.filen-cli/bin\n" >> $profileFile
          echo -e "${CHECK_MARK} Added ~/.filen-cli/bin to \$PATH in $profileFile"
        fi
      done
      if [[ $profileFileFound == "0" ]] ; then
        echo -e "${WARNING_SIGN} ERR: No shell profile file found (checked: ~/.bashrc ~/.bash_profile ~/.zshrc ~/.profile)"
      fi
    fi
    echo -e "${CHECK_MARK} ${GREEN}Filen CLI installed as \`filen\`${COLOR_OFF} (you might need to restart your shell)"

    echo "To uninstall, delete ~/.filen-cli and ~/.config/filen-cli and revert changes to your shell profile(s)"

  fi

fi
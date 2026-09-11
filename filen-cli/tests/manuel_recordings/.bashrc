MANUEL_ENV_FILE="$MANUEL_CWD/../.env"
if [ -f "$MANUEL_ENV_FILE" ]; then
    source "$MANUEL_ENV_FILE"
fi
if [ -z "$TEST_EMAIL" ] || [ -z "$TEST_PASSWORD" ]; then
    echo "Error: TEST_EMAIL and TEST_PASSWORD must be set"
    exit 1
fi
if [ -z "$TEST_AUTH_CONFIG_PATH" ] && [ -z "$OVERRIDE_TEST_AUTH_CONFIG_PATH" ]; then
    echo "Error: TEST_AUTH_CONFIG_PATH (or OVERRIDE_TEST_AUTH_CONFIG_PATH) must be set"
    exit 1
fi
if [ -n "$OVERRIDE_TEST_AUTH_CONFIG_PATH" ]; then
    export TEST_AUTH_CONFIG_PATH="$OVERRIDE_TEST_AUTH_CONFIG_PATH"
fi
export MANUEL_EMAIL="$TEST_EMAIL"
export MANUEL_PASSWORD="$TEST_PASSWORD"
export INTERNAL_FLAG_FOR_FILEN_CLI_AUTH_CONFIG_PATH=" --auth-config-path $TEST_AUTH_CONFIG_PATH"
export FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG="1"
export FILEN_CLI_TESTING_DISABLE_KEYRING="1"
no-auth() {
    export INTERNAL_FLAG_FOR_FILEN_CLI_AUTH_CONFIG_PATH=""
}

# make a uniquely named temporary directory for this test run
export MANUEL_TMP="$(mktemp -d /tmp/filen-cli-test-XXXXXX)"
# (will cd $MANUEL_TMP at the end of the script)
# generate a unique remote cwd for this test run
MANUEL_REMOTE_CWD_candidate="filen-cli-testing/manuel-$RANDOM"
set-remote-cwd() {
    export MANUEL_REMOTE_CWD="$MANUEL_REMOTE_CWD_candidate"
}

# make filen-cli binary available
cd $MANUEL_CWD
CARGO_TARGET_DIRECTORY=$(cargo metadata --format-version=1 --no-deps 2>/dev/null | jq --raw-output '.target_directory')
export FILEN_CLI_BINARY="$CARGO_TARGET_DIRECTORY/debug/filen-cli"
if [ ! -x "$FILEN_CLI_BINARY" ]; then
    echo "Error: filen-cli binary not found at $FILEN_CLI_BINARY (build it: cargo build -p filen-cli)"
    exit 1
fi
export INTERNAL_FLAG_FOR_FILEN_CLI_AUTOCOMPLETE=" --reluctant-autocomplete"
filen() {
    working_path_flag=" "
    if [ -n "$MANUEL_REMOTE_CWD" ]; then
        working_path_flag=" --working-path $MANUEL_REMOTE_CWD"
    fi

    $FILEN_CLI_BINARY$INTERNAL_FLAG_FOR_FILEN_CLI_AUTH_CONFIG_PATH$INTERNAL_FLAG_FOR_FILEN_CLI_AUTOCOMPLETE --make-output-environment-agnostic-for-replay-testing$working_path_flag "$@"
}

enable-autocomplete() {
    export INTERNAL_FLAG_FOR_FILEN_CLI_AUTOCOMPLETE=""
}

# change bash prompt to include the exit code
export ORIGINAL_PS1="$PS1"
export PROMPT_COMMAND=__prompt_command
__prompt_command() {
    local EXIT="$?"
    local Red='\[\e[0;31m\]'
    local ResetColor='\[\e[0m\]'
    if [ $EXIT != 0 ]; then
        PS1="${Red}(exit code: ${EXIT})${ResetColor} ${ORIGINAL_PS1}"
    else
        PS1="${ORIGINAL_PS1}"
    fi
}

# utilities

create-basic-local-file() {
    local FILE="$1"
    if [ -z "$FILE" ]; then
        echo "Error: create-basic-local-file requires a file path argument"
        return 1
    fi
    echo "This is a basic local file for testing." > "$FILE"
}

create-local-file-with-many-lines() {
    local FILE="$1"
    if [ -z "$FILE" ]; then
        echo "Error: create-local-file-with-many-lines requires a file path argument"
        return 1
    fi
    for i in {1..100}; do
        echo "This is line $i of a local file with many lines for testing." >> "$FILE"
    done
}

# nagivate to temp directory for this test run
cd $MANUEL_TMP
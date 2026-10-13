#!/bin/bash

# todo: this script might be used in CI to generate documentation

# This script generates OpenRPC documentation for the filen-cli JSON-RPC server.
# Environment variables:
#   FILEN_CLI_BINARY: The command to run the filen-cli binary (default: "cargo run --bin filen-cli --")
#   JSON_RPC_PORT: The port on which the JSON-RPC server will listen (default: 8080)
#   OUTPUT_DIR: The directory where the OpenRPC documentation will be saved (default: ./filen-cli-openrpc-docs)

FILEN_CLI_BINARY="${FILEN_CLI_BINARY:-cargo run --bin filen-cli --}"
JSON_RPC_PORT="${JSON_RPC_PORT:-8080}"
OUTPUT_DIR="${OUTPUT_DIR:-./filen-cli-openrpc-docs}"
mkdir -p "$OUTPUT_DIR"

$FILEN_CLI_BINARY json-rpc-server --addr 127.0.0.1:$JSON_RPC_PORT &
json_rpc_pid=$!
trap "kill $json_rpc_pid" EXIT INT TERM
echo "Starting JSON-RPC server with PID $json_rpc_pid on port $JSON_RPC_PORT"
# wait for it to start up
while [ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:$JSON_RPC_PORT)" -ne 405 ]; do 
  echo "Service is offline. Retrying in 1 second..."
  sleep 1
done
echo "Service is online. Proceeding with OpenRPC document generation."

curl -X POST http://127.0.0.1:$JSON_RPC_PORT -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"rpc.discover","id":1}' | jq '.result' > "$OUTPUT_DIR/openrpc.json"
if [ $? -ne 0 ]; then
    echo "Error: Failed to retrieve OpenRPC document from the JSON-RPC server."
    exit 1
fi

npx @open-rpc/markdown-generator "$OUTPUT_DIR/openrpc.json" -m "$OUTPUT_DIR" -t md
if [ $? -ne 0 ]; then
    echo "Error: Failed to generate OpenRPC Markdown documentation."
    exit 1
fi

echo "OpenRPC Markdown documentation and openrpc.json for filen-cli generated in $OUTPUT_DIR"

# todo: integrate this into the CLI, or its docs
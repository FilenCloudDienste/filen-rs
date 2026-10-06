#!/bin/bash

JSON_RPC_PORT=8080
OUTPUT_DIR=./filen-cli-openrpc-docs

mkdir -p $OUTPUT_DIR

curl -X POST http://127.0.0.1:8080 -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"rpc.discover","id":1}' | jq '.result' > $OUT_DIR/openrpc.json
trap "rm -f $temp_file" EXIT INT TERM
if [ $? -ne 0 ]; then
    echo "Error: Failed to retrieve OpenRPC document from the JSON-RPC server."
    exit 1
fi

npx @open-rpc/markdown-generator $temp_file -m $OUTPUT_DIR -t md
if [ $? -ne 0 ]; then
    echo "Error: Failed to generate OpenRPC Markdown documentation."
    exit 1
fi

echo "OpenRPC Markdown documentation and openrpc.json for filen-cli generated in $OUTPUT_DIR"

# todo: integrate this into the CLI, or its docs
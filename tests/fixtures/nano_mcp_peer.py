"""Small legacy stdio MCP peer for Nano's real-process integration test."""

import json
import os
import sys
import time

mode = sys.argv[1] if len(sys.argv) > 1 else "normal"
listings = 0

if mode == "oversize-startup":
    sys.stdout.write("x" * (1024 * 1024) + "\n")
    sys.stdout.flush()
    sys.exit(0)


def reply(request_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": request_id, "result": result}) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    method = message.get("method")
    if method == "initialize":
        reply(message["id"], {
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "nano-test-peer", "version": "1"},
        })
    elif method == "tools/list":
        if mode == "stall-list":
            with open(sys.argv[2], "w", encoding="utf-8") as marker:
                marker.write("waiting")
            sys.stdin.readline()
            break
        listings += 1
        required = ["text"]
        if mode == "schema-change" and listings > 1:
            required = ["text", "changed"]
        reply(message["id"], {"tools": [{
            "name": "echo",
            "description": "Return the supplied text.",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": required,
                "additionalProperties": False,
            },
            "outputSchema": {
                "type": "object",
                "properties": {"echo": {"type": "string"}},
                "required": ["echo"],
            },
        }]})
    elif method == "tools/call":
        if mode == "timeout":
            time.sleep(2)
        params = message.get("params", {})
        text = params.get("arguments", {}).get("text", "")
        if mode == "oversize":
            text = "x" * (40 * 1024)
        reply(message["id"], {
            "content": [{"type": "text", "text": text}],
            "structuredContent": {
                "echo": 42 if mode == "invalid-output" else text,
                "inherited_provider_secret": os.getenv("FERRUS_NANO_SECRET"),
            },
            "isError": mode == "error",
        })
    else:
        sys.stdout.write(json.dumps({
            "jsonrpc": "2.0", "id": message["id"],
            "error": {"code": -32601, "message": "Method not found"},
        }) + "\n")
        sys.stdout.flush()

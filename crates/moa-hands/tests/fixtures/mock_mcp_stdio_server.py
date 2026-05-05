#!/usr/bin/env python3
"""Tiny MCP stdio server fixture for integration tests."""

import json
import sys


def read_message():
    content_length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.decode("utf-8").strip()
        if not line:
            break
        if line.lower().startswith("content-length:"):
            content_length = int(line.split(":", 1)[1].strip())
    if content_length is None:
        return None
    payload = sys.stdin.buffer.read(content_length)
    return json.loads(payload.decode("utf-8"))


def write_message(message):
    payload = json.dumps(message).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(payload)}\r\n\r\n".encode("utf-8"))
    sys.stdout.buffer.write(payload)
    sys.stdout.buffer.flush()


while True:
    message = read_message()
    if message is None:
        break

    method = message.get("method")
    if method == "initialize":
        write_message(
            {
                "jsonrpc": "2.0",
                "id": message["id"],
                "result": {"protocolVersion": "2024-11-05", "capabilities": {}},
            }
        )
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        write_message(
            {
                "jsonrpc": "2.0",
                "id": message["id"],
                "result": {
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echo back a string",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"text": {"type": "string"}},
                                "required": ["text"],
                            },
                        }
                    ]
                },
            }
        )
    elif method == "tools/call":
        text = message.get("params", {}).get("arguments", {}).get("text", "")
        write_message(
            {
                "jsonrpc": "2.0",
                "id": message["id"],
                "result": {
                    "content": [{"type": "text", "text": text}],
                },
            }
        )
    else:
        write_message(
            {
                "jsonrpc": "2.0",
                "id": message.get("id"),
                "error": {"code": -32601, "message": f"Unknown method {method}"},
            }
        )

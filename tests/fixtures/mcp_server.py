"""Deterministic newline JSON-RPC stdio fixture, with concurrent cancellable calls."""
import json
import os
import sys
import threading
import time

lock = threading.Lock()
cancelled = set()

def send(identifier, result):
    with lock:
        print(json.dumps({"jsonrpc": "2.0", "id": identifier, "result": result}), flush=True)

def call(identifier, params):
    name = params["name"]
    if name == "crash":
        os._exit(17)
    if name == "slow":
        time.sleep(0.4)
        if identifier in cancelled:
            return
    if name == "cancellations":
        text = str(len(cancelled))
    elif name == "big":
        text = "x" * 100000
    else:
        text = json.dumps(params.get("arguments", {}), sort_keys=True)
    send(identifier, {"content": [{"type": "text", "text": text}], "isError": name == "fail"})

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    identifier = message.get("id")
    if method == "initialize":
        if len(sys.argv) > 1 and sys.argv[1] == "hang":
            time.sleep(60)
        send(identifier, {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}}, "serverInfo": {"name": "fixture", "version": "1"}})
    elif method == "tools/list":
        names = ["echo", "slow", "big", "fail", "crash", "bad.name", "bad name", "reference", "cancellations"]
        if message.get("params", {}).get("cursor"):
            names = names[4:]
            cursor = None
        else:
            names = names[:4]
            cursor = "second"
        tools = [{"name": name, "description": name, "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}} for name in names]
        for tool in tools:
            if tool["name"] == "reference":
                tool["inputSchema"] = {"$defs": {"text": {"type": "string"}}, "properties": {"text": {"$ref": "#/$defs/text"}}}
        send(identifier, {"tools": tools, "nextCursor": cursor})
    elif method == "tools/call":
        threading.Thread(target=call, args=(identifier, message["params"]), daemon=True).start()
    elif method == "notifications/cancelled":
        cancelled.add(message["params"]["requestId"])
    elif method == "ping":
        send(identifier, {})

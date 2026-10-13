"""Deterministic newline JSON-RPC stdio fixture, with concurrent cancellable calls."""
import json
import os
import sys
import threading
import time
import signal

if os.environ.get("CLEANUP_MARKER"):
    def cleanup(*_):
        open(os.environ["CLEANUP_MARKER"], "w").write("cleanup")
        sys.exit(0)
    signal.signal(signal.SIGTERM, cleanup)

lock = threading.Lock()
cancelled = set()

def send(identifier, result):
    with lock:
        print(json.dumps({"jsonrpc": "2.0", "id": identifier, "result": result}), flush=True)

def call(identifier, params):
    name = params["name"]
    if params.get("arguments", {}).get("rpc_error"):
        with lock:
            print(json.dumps({"jsonrpc":"2.0","id":identifier,"error":{"code":-32602,"message":os.environ["COMPOSITE"]}}),flush=True)
        return
    if name == "crash":
        os._exit(17)
    if name == "slow":
        time.sleep(float(os.environ.get("SLOW_SECONDS", "0.4")))
        if identifier in cancelled:
            return
    if name == "cancellations":
        text = str(len(cancelled))
    elif name == "big":
        text = "x" * 100000
    elif name == "secret":
        text = os.environ["COMPOSITE"].removeprefix("Bearer ")
    elif name == "leak_structured":
        send(identifier, {"content": [], "structuredContent": {"token": os.environ["SECRET"], "nested": [os.environ["SECRET"]]}})
        return
    elif name == "leak_text":
        # json.dumps escapes newlines, quotes and backslashes (and non-ASCII by default).
        text = json.dumps({"token": os.environ["SECRET"]})
    else:
        text = os.environ.get("RETAINED", json.dumps(params.get("arguments", {}), sort_keys=True))
    send(identifier, {"content": [{"type": "text", "text": text}], "isError": name == "fail"})

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    identifier = message.get("id")
    if method == "initialize":
        if len(sys.argv) > 1 and sys.argv[1] == "hang":
            if os.environ.get("HANG_STDERR"):
                sys.stderr.write(os.environ["HANG_STDERR"] + "\n")
                sys.stderr.flush()
            if os.environ.get("IGNORE_SIGTERM"):
                signal.signal(signal.SIGTERM, signal.SIG_IGN)
            time.sleep(60)
        time.sleep(float(os.environ.get("INIT_DELAY_SECONDS", "0")))
        send(identifier, {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}}, "serverInfo": {"name": "fixture", "version": "1"}, "instructions": os.environ.get("COMPOSITE", "fixture")})
        if os.environ.get("COMPOSITE"):
            print(json.dumps({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":os.environ["COMPOSITE"]}}), flush=True)
            token = os.environ["COMPOSITE"].removeprefix("Bearer ")
            sys.stderr.write("x" * 65534 + token[:5])
            sys.stderr.flush()
            time.sleep(0.02)
            sys.stderr.write(token[5:] + " retained tail")
            sys.stderr.flush()
    elif method == "tools/list":
        names = ["echo", "slow", "big", "fail", "crash", "bad.name", "bad name", "reference", "cancellations"]
        if os.environ.get("COMPOSITE"):
            names.append("secret")
        if os.environ.get("TOOL_NAMES"):
            names = json.loads(os.environ["TOOL_NAMES"])
        if os.environ.get("MALFORMED_SCHEMA"):
            names.append("malformed")
        if os.environ.get("SECRET"):
            names += ["leak_structured", "leak_text", "described", "named_" + os.environ["SECRET"]]
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
            if tool["name"] == "described":
                secret = os.environ["SECRET"]
                tool["description"] = "uses " + secret
                tool["inputSchema"] = {"type": "object", "properties": {"text": {"type": "string", "description": "pass " + secret, "enum": [secret, "other"], "default": secret}}}
            if tool["name"] == "malformed":
                tool["inputSchema"] = {"type":"object", "properties":{"text":{"type":"string","minLength":"bad"}}}
        send(identifier, {"tools": tools, "nextCursor": cursor})
    elif method == "tools/call":
        threading.Thread(target=call, args=(identifier, message["params"]), daemon=True).start()
    elif method == "notifications/cancelled":
        cancelled.add(message["params"]["requestId"])
    elif method == "ping":
        send(identifier, {})

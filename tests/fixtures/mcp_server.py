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

def error(identifier, code, text):
    with lock:
        print(json.dumps({"jsonrpc": "2.0", "id": identifier, "error": {"code": code, "message": text}}), flush=True)

def resources_page(identifier, params):
    """Two pages of resources (RESOURCES=1); LOOP_CURSOR makes the server repeat its cursor."""
    if os.environ.get("BIG_LISTING"):
        page = int(params.get("cursor") or 0)
        resources = [{"uri": "x://big/%d/%d" % (page, n), "name": "n", "description": "d" * 300} for n in range(200)]
        send(identifier, {"resources": resources, "nextCursor": str(page + 1)})
    elif os.environ.get("DESCRIBE_SECRET") and not params.get("cursor"):
        send(identifier, {"resources": [{"uri": "x://described", "name": "described", "description": "x" * 290 + os.environ["SECRET"]}]})
    elif os.environ.get("LOOP_CURSOR"):
        send(identifier, {"resources": [{"uri": "x://loop", "name": "loop"}], "nextCursor": "again"})
    elif not params.get("cursor"):
        send(identifier, {"resources": [{"uri": "file:///notes/a.md", "name": "a", "mimeType": "text/markdown", "description": "First\n  note"}], "nextCursor": "second"})
    else:
        resources = [{"uri": "x://multi", "name": "multi", "title": "Several pieces"}]
        if os.environ.get("SECRET"):
            resources.append({"uri": "x://" + os.environ["SECRET"], "name": "hidden"})
        send(identifier, {"resources": resources})

def templates_page(identifier):
    if os.environ.get("NO_TEMPLATES"):
        error(identifier, -32601, "Method not found")
        return
    if os.environ.get("SLOW_TEMPLATES"):
        time.sleep(float(os.environ["SLOW_TEMPLATES"]))
    description = "One note by id"
    if os.environ.get("DESCRIBE_SECRET"):
        description = "see " + os.environ["SECRET"]
    send(identifier, {"resourceTemplates": [{"uriTemplate": "x://notes/{id}", "name": "note", "description": description}]})

def read_resource(identifier, uri):
    if uri == "x://slow":
        time.sleep(float(os.environ.get("SLOW_SECONDS", "0.4")))
        if identifier in cancelled:
            return
    if uri == "x://multi":
        contents = [
            {"uri": "x://multi", "mimeType": "text/plain", "text": "first piece"},
            {"uri": "x://multi#image", "mimeType": "image/png", "blob": "iVBORw0KGgo="},
            {"uri": "x://multi#notes", "text": "second piece"},
        ]
    elif uri == "x://secret":
        contents = [{"uri": uri, "text": "token " + os.environ.get("SECRET", "")}]
    elif uri == "x://big":
        contents = [{"uri": uri, "text": "y" * 100000}]
    elif uri == "x://empty":
        contents = []
    elif uri == "x://missing":
        error(identifier, -32002, "Resource not found: " + os.environ.get("SECRET", ""))
        return
    else:
        contents = [{"uri": uri, "mimeType": "text/html;profile=mcp-app", "text": "<!doctype html><title>App</title>"}]
    send(identifier, {"contents": contents})

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
                sys.stderr.write("hung-init-stderr-line\n")
                sys.stderr.flush()
            if os.environ.get("IGNORE_SIGTERM"):
                signal.signal(signal.SIGTERM, signal.SIG_IGN)
            time.sleep(60)
        time.sleep(float(os.environ.get("INIT_DELAY_SECONDS", "0")))
        capabilities = {"tools": {}}
        if os.environ.get("RESOURCES"):
            capabilities["resources"] = {}
        if os.environ.get("RESOURCES_ONLY"):
            capabilities = {"resources": {}}
        send(identifier, {"protocolVersion": "2025-11-25", "capabilities": capabilities, "serverInfo": {"name": "fixture", "version": "1"}, "instructions": os.environ.get("COMPOSITE", "fixture")})
        if os.environ.get("COMPOSITE"):
            print(json.dumps({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":os.environ["COMPOSITE"]}}), flush=True)
            token = os.environ["COMPOSITE"].removeprefix("Bearer ")
            sys.stderr.write("x" * 65534 + token[:5])
            sys.stderr.flush()
            time.sleep(0.02)
            sys.stderr.write(token[5:] + " retained tail")
            sys.stderr.flush()
    elif method == "tools/list" and os.environ.get("RESOURCES_ONLY"):
        error(identifier, -32601, "Method not found")
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
            if tool["name"] == "echo":
                tool["_meta"] = {"ui": {"resourceUri": "ui://fixture/app.html"}}
            if tool["name"] == "reference":
                tool["inputSchema"] = {"$defs": {"text": {"type": "string"}}, "properties": {"text": {"$ref": "#/$defs/text"}}}
            if tool["name"] == "described":
                secret = os.environ["SECRET"]
                tool["description"] = "uses " + secret
                tool["inputSchema"] = {"type": "object", "properties": {"text": {"type": "string", "description": "pass " + secret, "enum": [secret, "other"], "default": secret}}}
            if tool["name"] == "malformed":
                tool["inputSchema"] = {"type":"object", "properties":{"text":{"type":"string","minLength":"bad"}}}
        send(identifier, {"tools": tools, "nextCursor": cursor})
    elif method == "resources/list":
        resources_page(identifier, message.get("params") or {})
    elif method == "resources/templates/list":
        threading.Thread(target=templates_page, args=(identifier,), daemon=True).start()
    elif method == "resources/read":
        uri = message.get("params", {}).get("uri", "")
        threading.Thread(target=read_resource, args=(identifier, uri), daemon=True).start()
    elif method == "tools/call":
        threading.Thread(target=call, args=(identifier, message["params"]), daemon=True).start()
    elif method == "notifications/cancelled":
        cancelled.add(message["params"]["requestId"])
    elif method == "ping":
        send(identifier, {})

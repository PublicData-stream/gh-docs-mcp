"""Independent HTTP/JSON-RPC assertions against the TLS ingress."""

from contextlib import closing
import http.client
import json
import ssl

VERSION = "2026-07-28"
PREFIX = "io.modelcontextprotocol/"
TOOL = "phase0_read_fixture"


def message(method, **params):
    params["_meta"] = {
        PREFIX + "protocolVersion": VERSION,
        PREFIX + "clientInfo": {"name": "phase-zero", "version": "1"},
        PREFIX + "clientCapabilities": {},
    }
    return {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}


def probe(port, ca):
    context = ssl.create_default_context(cafile=ca)
    origin = f"https://127.0.0.1:{port}"

    def exchange(body=None, *, verb="POST", path="/mcp", override=None, extra=(), raw=None):
        headers = [
            ("Host", f"127.0.0.1:{port}"),
            ("Accept", "application/json, text/event-stream"),
            ("Content-Type", "application/json"),
            ("MCP-Protocol-Version", VERSION),
        ]
        if body is not None:
            headers.append(("Mcp-Method", body["method"]))
            if body["method"] == "tools/call":
                headers.append(("Mcp-Name", body["params"]["name"]))
        override = override or {}
        headers = [(key, override.get(key, value)) for key, value in headers]
        payload = raw if raw is not None else (json.dumps(body).encode() if body is not None else b"")
        with closing(http.client.HTTPSConnection("127.0.0.1", port, context=context, timeout=5)) as client:
            client.putrequest(verb, path, skip_host=True)
            for key, value in [*headers, *extra, ("Content-Length", str(len(payload)))]:
                if value is not None:
                    client.putheader(key, value)
            client.endheaders(payload)
            response = client.getresponse()
            assert response.getheader("Mcp-Session-Id") is None
            raw_body = response.read()
            is_json = response.getheader("Content-Type", "").startswith("application/json")
            return response.status, json.loads(raw_body) if raw_body and is_json else {}

    def success(body, **kwargs):
        status, response = exchange(body, **kwargs)
        result = response.get("result", {})
        assert status == 200 and response.get("id") == 1, (status, response)
        assert result["resultType"] == "complete", response
        assert result.get("_meta", {}).get(PREFIX + "serverInfo", {}).get("name") == "docs-mcp", response
        return result

    def reject(status, code=None, body=None, **kwargs):
        actual, response = exchange(body, **kwargs)
        assert actual == status, (actual, status, body, kwargs, response)
        if code is not None:
            assert response["error"]["code"] == code, response
        if code == -32022:
            assert response["error"]["data"]["supported"] == [VERSION], response

    call = message("tools/call", name=TOOL, arguments={})
    assert success(call)["content"] == [{"type": "text", "text": "phase-zero fixture"}]
    discover = success(message("server/discover"))
    assert discover["supportedVersions"] == [VERSION], discover
    assert set(discover["capabilities"]) == {"tools"}, discover
    tools = success(message("tools/list"))["tools"]
    assert [tool["name"] for tool in tools] == [TOOL], tools
    assert tools[0]["inputSchema"] == {"type": "object", "properties": {}, "additionalProperties": False}
    assert tools[0]["annotations"]["readOnlyHint"] is True
    success(call, extra=[("Mcp-Session-Id", "ignored"), ("Last-Event-ID", "ignored"), ("Origin", origin)])
    success(call, extra=[("Forwarded", "host=wrong.invalid"), ("X-Forwarded-Host", "wrong.invalid")])
    assert success(message("tools/call", name=TOOL, arguments={"invalid": 1}))["isError"] is True

    for header in ("MCP-Protocol-Version", "Mcp-Method", "Mcp-Name"):
        reject(400, -32020, call, override={header: None})
        reject(400, -32020, call, extra=[(header, "conflicting")])
    reject(400, -32020, call, override={"Mcp-Method": "tools/list"})
    reject(400, -32020, call, override={"Mcp-Name": "wrong"})
    reject(400, -32020, call, override={"MCP-Protocol-Version": "2025-11-25"})
    for version in ("2025-11-25", "2030-01-01"):
        unsupported = message("tools/list")
        unsupported["params"]["_meta"][PREFIX + "protocolVersion"] = version
        reject(400, -32022, unsupported, override={"MCP-Protocol-Version": version})

    for field in ("protocolVersion", "clientCapabilities"):
        missing = message("tools/list")
        del missing["params"]["_meta"][PREFIX + field]
        reject(400, -32602, missing)
    optional = message("tools/list")
    del optional["params"]["_meta"][PREFIX + "clientInfo"]
    success(optional)
    reject(400, -32700, raw=b"{")
    reject(400, -32600, raw=b"[]")
    reject(413, body=call, raw=b"x" * (64 * 1024 + 1))
    reject(403, body=call, extra=[("Origin", "https://wrong.invalid")])
    reject(403, body=call, extra=[("Origin", origin), ("Origin", "https://wrong.invalid")])
    reject(403, body=call, override={"Host": "127.0.0.1:1"})
    reject(403, body=call, override={"Host": "wrong.invalid"},
           extra=[("Forwarded", "host=127.0.0.1"), ("X-Forwarded-Host", "127.0.0.1")])
    reject(405, verb="GET")
    reject(405, verb="DELETE")
    reject(404, verb="GET", path="/healthz")
    initialize = message("initialize")
    reject(404, -32601, initialize)
    reject(400, -32022, initialize, override={"MCP-Protocol-Version": "2025-11-25"})
    print("A01 raw stateless wire, tool schema, header, proxy and rejection probes passed", flush=True)

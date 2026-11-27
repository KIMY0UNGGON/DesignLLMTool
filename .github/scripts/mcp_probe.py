"""MCP stdio check for a built DesignLLMTool: initialize, design_guide, workspace_list, artifact_create, render_get.

Usage: python3 mcp_probe.py <DesignLLMTool executable> <scratch workspace folder> [render.png]
Starts `--mcp --background`, so no window opens. Exits non-zero when any step fails.
"""
import base64
import json
import subprocess
import sys
import uuid

exe, workspace = sys.argv[1], sys.argv[2]
render_path = sys.argv[3] if len(sys.argv) > 3 else None
proc = subprocess.Popen(
    [exe, "--mcp", "--background", "--workspace", workspace],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=sys.stderr,
)
next_id = 0


def call(method, params=None):
    global next_id
    next_id += 1
    message = {"jsonrpc": "2.0", "id": next_id, "method": method}
    if params is not None:
        message["params"] = params
    proc.stdin.write((json.dumps(message) + "\n").encode())
    proc.stdin.flush()
    while True:
        line = proc.stdout.readline()
        if not line:
            raise SystemExit(f"{method}: the server closed stdout")
        reply = json.loads(line)
        if reply.get("id") == next_id:
            if "error" in reply:
                raise SystemExit(f"{method}: {reply['error']}")
            return reply["result"]


def tool(name, arguments):
    result = call("tools/call", {"name": name, "arguments": arguments})
    if result.get("isError"):
        raise SystemExit(f"{name}: {json.dumps(result)[:500]}")
    return result


info = call("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "ci-probe", "version": "1"}})
print("server:", info["serverInfo"])
proc.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
proc.stdin.flush()

guide = tool("design_guide", {"background": True})
print("design_guide:", len(guide["content"][0]["text"]), "characters")

listing = tool("workspace_list", {"background": True})["structuredContent"]
workspace_id = listing["workspace_id"]
print("workspace:", workspace_id)

created = tool("artifact_create", {
    "background": True,
    "request_id": str(uuid.uuid4()),
    "actor": "agent",
    "kind": "canvas",
    "title": "CI probe",
})["structuredContent"]
artifact = created.get("artifact", created)
artifact_id, revision = artifact["id"], artifact["revision"]
print("artifact:", artifact_id, "revision", revision)

rendered = tool("render_get", {"background": True, "artifact_id": artifact_id, "expected_revision": revision, "page_index": 0})
images = [item for item in rendered["content"] if item.get("type") == "image"]
if not images:
    raise SystemExit("render_get returned no image")
png = base64.b64decode(images[0]["data"])
if not png.startswith(b"\x89PNG"):
    raise SystemExit("render_get did not return a PNG")
print("render_get:", len(png), "bytes PNG")
if render_path:
    with open(render_path, "wb") as file:
        file.write(png)

proc.stdin.close()
proc.wait(timeout=30)
print("exit:", proc.returncode)

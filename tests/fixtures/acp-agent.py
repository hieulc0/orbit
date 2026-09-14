"""Offline ACP preflight adversary. Never contacts a model or loads credentials."""
import json
import os
import pathlib
import sys
import time


def emit(message):
    print(json.dumps(message), flush=True)


mode = sys.argv[1]
request = json.loads(sys.stdin.readline())
assert request["method"] == "initialize"
assert request["params"]["protocolVersion"] == 1
capabilities = request["params"].get("clientCapabilities", {})
assert not capabilities.get("terminal", False)
assert not capabilities.get("fs", {}).get("readTextFile", False)
assert not capabilities.get("fs", {}).get("writeTextFile", False)
assert not any(key in os.environ for key in (
    "ORBIT_TOKEN", "OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_HOME", "LD_PRELOAD",
))
assert os.environ["HOME"] == str(pathlib.Path.cwd())
assert pathlib.Path.cwd().stat().st_mode & 0o077 == 0

if mode == "timeout":
    time.sleep(30)
elif mode == "eof":
    sys.exit(0)
elif mode == "oversize":
    sys.stdout.write("x" * (2 * 1024 * 1024))
    sys.stdout.flush()
    time.sleep(30)
elif mode == "flood":
    for _ in range(200):
        emit({"jsonrpc": "2.0", "method": "_ignored", "params": {}})
    time.sleep(30)
elif mode == "error":
    emit({"jsonrpc": "2.0", "id": request["id"], "error": {
        "code": -32603, "message": "secret-provider-token", "data": "secret-reasoning"
    }})
    time.sleep(30)
elif mode == "callback":
    emit({"jsonrpc": "2.0", "id": "permission", "method": "session/request_permission",
          "params": {"sessionId": "unowned", "toolCall": {"toolCallId": "tool"},
                     "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}})
    response = json.loads(sys.stdin.readline())
    assert response["result"]["outcome"]["outcome"] == "cancelled"
    emit({"jsonrpc": "2.0", "id": "file", "method": "fs/read_text_file",
          "params": {"sessionId": "unowned", "path": "/etc/passwd"}})
    response = json.loads(sys.stdin.readline())
    assert "error" in response
    emit({"jsonrpc": "2.0", "id": "ext", "method": "_run_command", "params": {}})
    response = json.loads(sys.stdin.readline())
    assert "error" in response
elif mode == "child":
    if os.fork() == 0:
        time.sleep(0.8)
        pathlib.Path(sys.argv[2]).write_text("child survived")
        time.sleep(30)
        sys.exit(0)

result = {
    "protocolVersion": 2 if mode == "version" else 1,
    "agentInfo": {"name": "orbit-acp-fixture", "version": "wrong" if mode == "identity" else "1"},
    "agentCapabilities": {"loadSession": True, "mcpCapabilities": {"http": True}},
    "authMethods": [{"id": "local-session", "name": "secret-provider-token",
                     "description": "secret-auth-url"}],
}
emit({"jsonrpc": "2.0", "id": request["id"], "result": result})
# The probe must not send authentication, new session or prompt requests.
for extra in sys.stdin:
    pathlib.Path("unexpected-request").write_text(extra)
    sys.exit(2)

"""Bind native harness actions to one confined Orbit callback.

Installed as acp.orbit_correlation only in the separately pinned runtime overlay.
The harness action ID is supplied by dispatch, never by model tool arguments.
"""
from contextvars import ContextVar
from dataclasses import dataclass
import re
import uuid


BROKERED_TOOLS = {
    "client_view_file": ("fs/read_text_file", "read"),
    "client_create_file": ("fs/write_text_file", "edit"),
    "client_edit_file": ("fs/write_text_file", "edit"),
    "orbit_terminal": ("orbit/shell", "execute"),
}
_active = ContextVar("orbit_provider_invocation", default=None)
_connection = None
_atomic_shell = False


@dataclass
class Invocation:
    provider_id: str
    invocation_id: str
    session_id: str
    method: str
    callback_sent: bool = False
    active: bool = True

    def metadata(self):
        return {"orbit": {"toolInvocationId": self.invocation_id,
                          "providerToolCallId": self.provider_id}}


def _identifier(value, limit):
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9_.:-]{1,%d}" % limit, value):
        raise RuntimeError("ORBIT_PROVIDER_IDENTITY_INVALID")
    return value


def bind_connection(connection, atomic_shell):
    global _connection, _atomic_shell
    if _connection is not None and _connection is not connection:
        raise RuntimeError("ORBIT_CONNECTION_REBIND_DENIED")
    if not isinstance(atomic_shell, bool):
        raise RuntimeError("ORBIT_ATOMIC_SHELL_NEGOTIATION_INVALID")
    _connection, _atomic_shell = connection, atomic_shell


async def _update(invocation, name, kind, status, start=False):
    if _connection is None:
        raise RuntimeError("ORBIT_CONNECTION_UNAVAILABLE")
    await _connection._transport.send({
        "jsonrpc": "2.0", "method": "session/update",
        "params": {"sessionId": invocation.session_id, "update": {
            "sessionUpdate": "tool_call" if start else "tool_call_update",
            "toolCallId": invocation.provider_id, "title": name,
            "kind": kind, "status": status,
        }},
        "_meta": invocation.metadata(),
    })


async def execute_provider_call(runner, call):
    if call.name not in BROKERED_TOOLS:
        return await runner.process_tool_calls([call])
    provider_id = _identifier(call.id, 256)
    session_id = _identifier(runner._context.conversation_id, 256)
    seen = getattr(runner, "_orbit_seen_provider_ids", None)
    if seen is None:
        seen = runner._orbit_seen_provider_ids = set()
    if provider_id in seen or len(seen) >= 4096:
        raise RuntimeError("ORBIT_PROVIDER_ID_REPLAY_OR_LIMIT")
    seen.add(provider_id)
    method, kind = BROKERED_TOOLS[call.name]
    invocation = Invocation(provider_id, "oti-" + str(uuid.uuid4()), session_id, method)
    token = _active.set(invocation)
    status = "failed"
    try:
        await _update(invocation, call.name, kind, "in_progress", start=True)
        results = await runner.process_tool_calls([call])
        if len(results) != 1:
            raise RuntimeError("ORBIT_PROVIDER_RESULT_IDENTITY_INVALID")
        result = results[0]
        if result.id is not None and result.id != provider_id:
            raise RuntimeError("ORBIT_PROVIDER_RESULT_IDENTITY_INVALID")
        result.id = provider_id
        if result.error is None:
            if not invocation.callback_sent:
                raise RuntimeError("ORBIT_PROVIDER_CALLBACK_MISSING")
            status = "completed"
        return results
    finally:
        # Child tasks inherit ContextVar values. Closing the shared invocation
        # revokes their callback authority even after the parent context resets.
        invocation.active = False
        try:
            await _update(invocation, call.name, kind, status)
        finally:
            _active.reset(token)


def correlate_request(connection, payload):
    method = payload["method"]
    if method not in {"fs/read_text_file", "fs/write_text_file", "orbit/shell"}:
        if method.startswith(("fs/", "terminal/")):
            raise RuntimeError("ORBIT_UNCORRELATED_EFFECT_DENIED")
        return payload
    invocation = _active.get()
    if invocation is None or not invocation.active or connection is not _connection:
        raise RuntimeError("ORBIT_PROVIDER_CONTEXT_MISSING")
    if invocation.callback_sent or method != invocation.method:
        raise RuntimeError("ORBIT_PROVIDER_CALLBACK_REPLAY_OR_MISMATCH")
    if payload.get("params", {}).get("sessionId") != invocation.session_id:
        raise RuntimeError("ORBIT_PROVIDER_SESSION_MISMATCH")
    if method == "orbit/shell" and not _atomic_shell:
        raise RuntimeError("ORBIT_ATOMIC_SHELL_NOT_NEGOTIATED")
    invocation.callback_sent = True
    return {**payload, "_meta": invocation.metadata()}


async def atomic_terminal(client, session_id, command, workspace_path):
    if not isinstance(command, str) or not command.strip() or len(command.encode()) > 8192:
        raise ValueError("terminal command must contain 1..8192 bytes")
    result = await client._conn.send_request("orbit/shell", {
        "sessionId": session_id, "command": "sh", "args": ["-c", command],
        "cwd": workspace_path, "output_byte_limit": 65536,
    })
    if (not isinstance(result, dict) or type(result.get("exit_code")) is not int
            or not isinstance(result.get("output"), str)
            or len(result["output"].encode()) > 65536
            or type(result.get("truncated")) is not bool):
        raise RuntimeError("ORBIT_TERMINAL_RESULT_UNCONFIRMED")
    return result

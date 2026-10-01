#!/usr/bin/env python3
"""Build an identity-preserving overlay from the pinned terminal runtime."""
import argparse
import hashlib
import importlib.util
import pathlib
import shutil
import struct
import sys
import tempfile
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
BASE_SHA256 = "98890a0a1afc3ebe91f6018c15bef26b429147e4b61c408d08b2374465fc10c7"
MODULE = "google3/cloud/developer_experience/antigravity_extensions/acp_server/"
SDK = "google3/third_party/py/"
CONNECTION = SDK + "acp/connection.py"
DISPATCH = SDK + "google/antigravity/connections/local/event_processor.py"
CORRELATION = SDK + "acp/orbit_correlation.py"

spec = importlib.util.spec_from_file_location("terminal_overlay", ROOT / "scripts/patch-antigravity-terminal.py")
terminal_overlay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(terminal_overlay)


def patch_sources(original):
    replace = terminal_overlay.replace_once
    server = original.read(MODULE + "server.py").decode()
    server = replace(server,
        '    self._orbit_terminal = bool(getattr(client_capabilities, "terminal", False))',
        '    from acp.orbit_correlation import bind_connection\n'
        '    orbit_meta = (getattr(client_capabilities, "field_meta", None) or {}).get("orbit", {})\n'
        '    atomic_shell = orbit_meta.get("atomicShell", False)\n'
        '    if getattr(client_capabilities, "terminal", False) and atomic_shell is not True:\n'
        '      raise RuntimeError("ORBIT_ATOMIC_SHELL_REQUIRED")\n'
        '    bind_connection(self._client._conn, atomic_shell)\n'
        '    self._orbit_terminal = bool(getattr(client_capabilities, "terminal", False))')
    server = replace(server,
        '    disabled_builtin_tools = [sdk_types_module.BuiltinTools.VIEW_FILE,\n'
        '                              sdk_types_module.BuiltinTools.RUN_COMMAND]',
        '    disabled_builtin_tools = list(sdk_types_module.BuiltinTools)')
    # Provider-side ambient tools and hooks cannot inherit repository authority.
    browser_start = '    # Browser subagent'
    browser_end = '    # Forward the enterprise OS-sandbox flag'
    if server.count(browser_start) != 1 or server.count(browser_end) != 1:
        raise ValueError("Antigravity browser source anchor mismatch")
    start, end = server.index(browser_start), server.index(browser_end)
    if end <= start:
        raise ValueError("Antigravity browser source layout mismatch")
    server = (server[:start] + '    subagents = []\n'
              '    self._browser_notices.pop(session_id, None)\n\n' + server[end:])
    server = replace(server,
        '    hooks_config = await acp_hooks.load_hooks_config_async(resolved_cwd)',
        '    hooks_config = {}  # Orbit does not load ambient execution hooks.')
    server = replace(server, '        "mcp_servers": mcp_configs,',
                     '        "mcp_servers": [],')
    server = replace(server, '        "subagents": subagents,',
                     '        "subagents": [],')
    # Brokered action starts are emitted at native dispatch with its exact ID.
    # Do not use the upstream permission-frame FIFO reconciliation for them.
    server = replace(server,
        '      for call in step.tool_calls:\n        if not call.id:\n          continue',
        '      for call in step.tool_calls:\n'
        '        if str(call.name) in {"client_view_file", "client_create_file", "client_edit_file", "orbit_terminal"}:\n'
        '          continue\n'
        '        if not call.id:\n          continue')
    dispatch = replace(original.read(DISPATCH).decode(),
        '          results = await self._tool_runner.process_tool_calls(\n'
        '              [types.ToolCall(name=tc.name, args=tc.args)]\n'
        '          )',
        '          from acp.orbit_correlation import execute_provider_call\n'
        '          results = await execute_provider_call(self._tool_runner, tc)')
    connection = replace(original.read(CONNECTION).decode(),
        '    self._pending[request_id] = future\n    await self._transport.send(payload)',
        '    from .orbit_correlation import correlate_request\n'
        '    payload = correlate_request(self, payload)\n'
        '    self._pending[request_id] = future\n    await self._transport.send(payload)')
    tools = original.read(MODULE + "tools.py").decode()
    anchor = "def make_orbit_client_terminal(client, workspace_path):"
    if tools.count(anchor) != 1:
        raise ValueError("Antigravity terminal source anchor mismatch")
    tools = tools[:tools.index(anchor)] + '''def make_orbit_client_terminal(client, workspace_path):
  async def orbit_terminal(command: str, ctx: tool_context.ToolContext) -> str:
    """Run a bounded repository command through Orbit's confined shell callback."""
    import json
    from acp.orbit_correlation import atomic_terminal
    result = await atomic_terminal(client, ctx.conversation_id, command, workspace_path)
    return json.dumps(result)
  return orbit_terminal
'''
    return {MODULE + "server.py": server, MODULE + "tools.py": tools,
            DISPATCH: dispatch, CONNECTION: connection,
            CORRELATION: (ROOT / "deploy/antigravity/tool_correlation.py").read_text()}


def cached_name(source):
    directory, name = source.rsplit("/", 1)
    return directory + "/__pycache__/" + name[:-3] + ".cpython-314.pyc"


def build(source, output):
    if sys.version_info[:3] != (3, 14, 7):
        raise ValueError("Python 3.14.7 is required for reproducible bytecode")
    if source.is_symlink() or not source.is_file():
        raise ValueError("input must be a regular non-symlink pinned runtime")
    with source.open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != BASE_SHA256:
            raise ValueError("Antigravity terminal-runtime digest mismatch")
    with zipfile.ZipFile(source) as original:
        changes = patch_sources(original)
        replacements = {name: text.encode() for name, text in changes.items()}
        for name, text in changes.items():
            cached = cached_name(name)
            template = original.read(cached if name != CORRELATION else cached_name(CONNECTION))
            replacements[cached] = terminal_overlay.rebuilt_bytecode(text, name, template)
        prefix_size = min(entry.header_offset for entry in original.infolist())
        with tempfile.TemporaryFile(dir=output.parent) as archive:
            with zipfile.ZipFile(archive, "w") as patched:
                for entry in original.infolist():
                    if entry.filename in replacements:
                        patched.writestr(entry, replacements.pop(entry.filename))
                    else:
                        with original.open(entry) as src, patched.open(entry, "w") as dst:
                            shutil.copyfileobj(src, dst, 1024 * 1024)
                for name, data in replacements.items():
                    entry = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
                    entry.external_attr = 0o644 << 16
                    patched.writestr(entry, data)
            archive_size = archive.tell()
            archive.seek(0)
            with source.open("rb") as src, output.open("xb") as dst:
                size_offset = 621948208 + 68 * 64 + 32
                prefix = bytearray(src.read(prefix_size))
                if len(prefix) != prefix_size or prefix[:6] != b"\x7fELF\x02\x01":
                    raise ValueError("unexpected ELF prefix")
                if struct.unpack_from("<Q", prefix, size_offset - 8)[0] != prefix_size:
                    raise ValueError("unexpected .par_data section offset")
                struct.pack_into("<Q", prefix, size_offset, archive_size)
                dst.write(prefix)
                del prefix
                shutil.copyfileobj(archive, dst, 1024 * 1024)
    with zipfile.ZipFile(output) as check:
        for name in changes:
            expected = importlib.util.source_hash(check.read(name))
            if check.read(cached_name(name))[8:16] != expected:
                raise ValueError("stale adapter bytecode retained")
    output.chmod(0o755)
    with output.open("rb") as stream:
        print(hashlib.file_digest(stream, "sha256").hexdigest())


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    args = parser.parse_args()
    build(args.source, args.output)

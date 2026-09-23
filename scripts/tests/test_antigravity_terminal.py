"""Unit checks for the build-only adapter; no vendor imports or provider calls."""
import asyncio
import importlib.util
import io
import json
import marshal
import pathlib
import tempfile
import types
import unittest
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[2]


def factory():
    scope = {"tool_context": types.SimpleNamespace(ToolContext=object)}
    path = ROOT / "deploy/antigravity/client_terminal.py"
    exec(compile(path.read_text(), str(path), "exec"), scope)
    return scope["make_orbit_client_terminal"]


class Client:
    def __init__(self, exit_code=0, error=None, output="ok"):
        self.calls = []
        self.code, self.error, self.output = exit_code, error, output

    async def create_terminal(self, **kwargs):
        self.calls.append(("create", kwargs))
        return types.SimpleNamespace(terminal_id="t")

    async def wait_for_terminal_exit(self, **kwargs):
        self.calls.append(("wait", kwargs))
        if self.error:
            raise self.error

    async def terminal_output(self, **kwargs):
        self.calls.append(("output", kwargs))
        return types.SimpleNamespace(output=self.output, truncated=False,
                                     exit_status=types.SimpleNamespace(exit_code=self.code, signal=None))

    async def release_terminal(self, **kwargs):
        self.calls.append(("release", kwargs))


class TerminalTests(unittest.IsolatedAsyncioTestCase):
    async def test_nonzero_then_success_uses_client_workspace_without_environment(self):
        client = Client(7)
        tool = factory()(client, "/orbit/home/workspace")
        ctx = types.SimpleNamespace(conversation_id="s")
        first = json.loads(await tool("exit 7", ctx))
        self.assertEqual(first["exit_code"], 7)
        client.code = 0
        self.assertEqual(json.loads(await tool("git status --short", ctx))["exit_code"], 0)
        self.assertEqual([c[0] for c in client.calls], ["create", "wait", "output", "release"] * 2)
        create = client.calls[0][1]
        self.assertEqual(create, {"session_id": "s", "command": "sh", "args": ["-c", "exit 7"],
                                  "cwd": "/orbit/home/workspace", "output_byte_limit": 65536})

    async def test_failure_and_cancellation_release_without_native_fallback(self):
        for error in (RuntimeError("transport lost"), asyncio.CancelledError()):
            client = Client(error=error)
            with self.assertRaises(type(error)):
                await factory()(client, "/orbit/home/workspace")("pwd", types.SimpleNamespace(conversation_id="s"))
            self.assertEqual(client.calls[-1][0], "release")

    async def test_output_and_command_bounds(self):
        client = Client(output="x" * 65537)
        tool = factory()(client, "/orbit/home/workspace")
        ctx = types.SimpleNamespace(conversation_id="s")
        with self.assertRaises(ValueError):
            await tool("x" * 8193, ctx)
        self.assertEqual(client.calls, [])
        with self.assertRaises(RuntimeError):
            await tool("pwd", ctx)
        self.assertEqual(client.calls[-1][0], "release")


class PackagingTests(unittest.TestCase):
    def test_rebuild_replaces_bytecode_and_preserves_other_modules(self):
        spec = importlib.util.spec_from_file_location("patcher", ROOT / "scripts/patch-antigravity-terminal.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        old, new = io.BytesIO(), io.BytesIO()
        with zipfile.ZipFile(old, "w") as archive:
            for cached in module.ADAPTER_BYTECODE:
                archive.writestr(cached, importlib.util.MAGIC_NUMBER + b"original bytecode")
            archive.writestr(module.MODULE + "server.py", "old source")
            archive.writestr(module.MODULE + "tools.py", "old source")
            archive.writestr("untouched.pyc", b"unchanged")
        old.seek(0)
        with zipfile.ZipFile(old) as archive:
            module.rebuild_zip(archive, new, {module.MODULE + "server.py": "terminal = True",
                                               module.MODULE + "tools.py": "terminal = True"})
        new.seek(0)
        with zipfile.ZipFile(new) as archive:
            for cached in module.ADAPTER_BYTECODE:
                namespace = {}
                exec(marshal.loads(archive.read(cached)[16:]), namespace)
                self.assertIs(namespace["terminal"], True)
                self.assertEqual(archive.read(cached)[8:16], importlib.util.source_hash(b"terminal = True"))
            self.assertEqual(archive.read(module.MODULE + "server.py"), b"terminal = True")
            self.assertEqual(archive.read("untouched.pyc"), b"unchanged")

    def test_unrecognized_binary_and_source_fail_closed(self):
        spec = importlib.util.spec_from_file_location("patcher", ROOT / "scripts/patch-antigravity-terminal.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with self.assertRaises(ValueError):
            module.patch_server("unexpected version")
        with tempfile.TemporaryDirectory() as root:
            source, output = pathlib.Path(root) / "input", pathlib.Path(root) / "output"
            source.write_bytes(b"wrong binary")
            with self.assertRaises(ValueError):
                module.build(source, output)
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()

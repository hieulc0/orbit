"""Identity and confinement checks without vendor imports or model calls."""
import asyncio
import importlib.util
import pathlib
import sys
import types
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]


def load_module():
    spec = importlib.util.spec_from_file_location(
        "orbit_correlation_test", ROOT / "deploy/antigravity/tool_correlation.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class Connection:
    def __init__(self, module):
        self.module, self.messages = module, []
        self._transport = types.SimpleNamespace(send=self.send)
        self._conn = self

    async def send(self, payload):
        self.messages.append(payload)

    async def send_request(self, method, params):
        payload = self.module.correlate_request(self, {
            "jsonrpc": "2.0", "id": len(self.messages), "method": method, "params": params})
        await self.send(payload)
        if method == "orbit/shell":
            return {"exit_code": 7, "output": "bounded", "truncated": False}
        return {"content": params["path"]}


class Runner:
    def __init__(self, execute, session="session-1"):
        self._context = types.SimpleNamespace(conversation_id=session)
        self.execute = execute

    async def process_tool_calls(self, calls):
        return [await self.execute(calls[0])]


def call(identity="provider-1", name="client_view_file", **args):
    return types.SimpleNamespace(id=identity, name=name, args=args)


def result(identity=None, error=None):
    return types.SimpleNamespace(id=identity, error=error)


class CorrelationTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.module = load_module()
        self.connection = Connection(self.module)
        self.module.bind_connection(self.connection, True)

    async def read(self, native):
        await self.connection.send_request("fs/read_text_file", {
            "sessionId": "session-1", "path": native.args.get("path", "file.txt")})
        return result()

    async def test_native_action_callback_and_result_share_identity(self):
        runner = Runner(self.read)
        native = call(toolInvocationId="model-spoof", providerToolCallId="model-spoof")
        records = await self.module.execute_provider_call(runner, native)
        start, callback, finish = self.connection.messages
        self.assertEqual(records[0].id, native.id)
        self.assertEqual(start["params"]["update"]["toolCallId"], native.id)
        self.assertEqual(start["_meta"], callback["_meta"])
        self.assertEqual(start["_meta"], finish["_meta"])
        self.assertEqual(callback["_meta"]["orbit"]["providerToolCallId"], native.id)
        self.assertTrue(callback["_meta"]["orbit"]["toolInvocationId"].startswith("oti-"))
        self.assertNotIn("model-spoof", str(self.connection.messages))
        self.assertIsNone(self.module._active.get())

    async def test_concurrent_same_tool_actions_do_not_use_fifo(self):
        second_finished = asyncio.Event()

        async def execute(native):
            if native.id == "first":
                await second_finished.wait()
            await self.read(native)
            if native.id == "second":
                second_finished.set()
            return result()

        runner = Runner(execute)
        await asyncio.gather(
            self.module.execute_provider_call(runner, call("first", path="first.txt")),
            self.module.execute_provider_call(runner, call("second", path="second.txt")))
        callbacks = [m for m in self.connection.messages if m["method"] == "fs/read_text_file"]
        self.assertEqual([m["params"]["path"] for m in callbacks], ["second.txt", "first.txt"])
        for message in callbacks:
            self.assertEqual(message["_meta"]["orbit"]["providerToolCallId"] + ".txt",
                             message["params"]["path"])
        self.assertEqual(len({m["_meta"]["orbit"]["toolInvocationId"] for m in callbacks}), 2)

    async def test_missing_unsafe_or_replayed_provider_ids_fail_before_dispatch(self):
        runner = Runner(self.read)
        for identity in (None, "", "space bad", "a" * 257, "秘密"):
            with self.assertRaisesRegex(RuntimeError, "IDENTITY_INVALID"):
                await self.module.execute_provider_call(runner, call(identity))
        self.assertEqual(self.connection.messages, [])
        await self.module.execute_provider_call(runner, call())
        previous = list(self.connection.messages)
        with self.assertRaisesRegex(RuntimeError, "REPLAY_OR_LIMIT"):
            await self.module.execute_provider_call(runner, call())
        self.assertEqual(self.connection.messages, previous)

    async def test_callback_replay_wrong_method_and_wrong_session_fail_closed(self):
        async def execute(native):
            params = {"sessionId": "session-1", "path": "file.txt"}
            if native.id == "wrong-method":
                await self.connection.send_request("fs/write_text_file", params)
            elif native.id == "wrong-session":
                await self.connection.send_request("fs/read_text_file", {**params, "sessionId": "other"})
            else:
                await self.read(native)
                await self.read(native)
            return result()

        for identity in ("wrong-method", "wrong-session", "replay"):
            with self.assertRaises(RuntimeError):
                await self.module.execute_provider_call(Runner(execute), call(identity))
            self.assertIsNone(self.module._active.get())
        callbacks = [m for m in self.connection.messages if "id" in m]
        self.assertEqual(len(callbacks), 1)

    async def test_missing_callback_wrong_result_and_cancel_remain_failed(self):
        async def execute(native):
            if native.id == "cancel":
                raise asyncio.CancelledError()
            if native.id == "wrong-result":
                await self.read(native)
                return result(identity="unrelated")
            return result()

        for identity, error in (("missing", RuntimeError), ("wrong-result", RuntimeError),
                                ("cancel", asyncio.CancelledError)):
            with self.assertRaises(error):
                await self.module.execute_provider_call(Runner(execute), call(identity))
            self.assertEqual(self.connection.messages[-1]["params"]["update"]["status"], "failed")
            self.assertIsNone(self.module._active.get())

    async def test_atomic_terminal_is_one_correlated_callback_with_nonzero_feedback(self):
        async def execute(native):
            output = await self.module.atomic_terminal(self.connection, "session-1", "exit 7", "/workspace")
            self.assertEqual(output["exit_code"], 7)
            return result()

        await self.module.execute_provider_call(Runner(execute), call(name="orbit_terminal"))
        callbacks = [m for m in self.connection.messages if "id" in m]
        self.assertEqual(len(callbacks), 1)
        self.assertEqual(callbacks[0]["method"], "orbit/shell")
        self.assertNotIn("env", callbacks[0]["params"])
        self.assertEqual(self.connection.messages[-1]["params"]["update"]["status"], "completed")

    async def test_child_task_cannot_send_effect_after_dispatch_finishes(self):
        for cancelled in (False, True):
            release = asyncio.Event()
            tasks = []

            async def late_effect():
                await release.wait()
                await self.read(call())

            async def execute(native):
                tasks.append(asyncio.create_task(late_effect()))
                if cancelled:
                    raise asyncio.CancelledError()
                return result(error="native failure before callback")

            if cancelled:
                with self.assertRaises(asyncio.CancelledError):
                    await self.module.execute_provider_call(Runner(execute), call("cancelled"))
            else:
                await self.module.execute_provider_call(Runner(execute), call("failed"))
            release.set()
            with self.assertRaisesRegex(RuntimeError, "CONTEXT_MISSING"):
                await tasks[0]
        self.assertFalse(any("id" in message for message in self.connection.messages))

    async def test_unnegotiated_terminal_and_out_of_context_effects_are_denied(self):
        self.module.bind_connection(self.connection, False)
        with self.assertRaisesRegex(RuntimeError, "CONTEXT_MISSING"):
            await self.connection.send_request("fs/read_text_file", {"sessionId": "session-1"})
        with self.assertRaisesRegex(RuntimeError, "UNCORRELATED_EFFECT_DENIED"):
            await self.connection.send_request("terminal/create", {})

        async def execute(native):
            await self.module.atomic_terminal(self.connection, "session-1", "pwd", "/workspace")
            return result()

        with self.assertRaisesRegex(RuntimeError, "NOT_NEGOTIATED"):
            await self.module.execute_provider_call(Runner(execute), call(name="orbit_terminal"))
        self.assertFalse(any("id" in m for m in self.connection.messages))


if __name__ == "__main__":
    unittest.main()

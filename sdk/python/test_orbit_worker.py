import hashlib
import json
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from orbit_worker import Client, OrbitError, operation, reserve_agent_call, agent_report


class ProtocolTest(unittest.TestCase):
    def test_agent_contract_helpers(self):
        assignment = dict(run_id="run", attempt_id="attempt", generation=2,
                          lease_token="local-fixture", agent_binding_digest="a" * 64)
        request = reserve_agent_call(assignment, call_id="call", tokens=25,
                                     cost_microusd=1, request_id="stable")
        self.assertEqual(request["request_id"], "stable")
        self.assertEqual(request["operation"], "reserve_agent_call")
        self.assertEqual(request["reservation"]["tokens"], 25)
        report = agent_report(assignment, {"answer": True}, ["work"])
        self.assertEqual(report["binding_digest"], "a" * 64)
        self.assertNotIn("lease_token", report)

    def test_transport_and_retransmission(self):
        calls = []

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                calls.append((self.path, self.headers["Authorization"],
                              json.loads(self.rfile.read(int(self.headers["Content-Length"])))))
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'{"status":"accepted"}')

            def do_GET(self):
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"artifact")

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            client = Client(f"http://127.0.0.1:{server.server_port}", "test-token")
            client.register(["repository.code"])
            client.claim("repository.code", request_id="claim-1")
            assignment = dict(run_id="run", attempt_id="attempt", generation=1, lease_token="lease")
            body = operation(assignment, "complete", success=False, outputs=[], failure=None)
            client.send_operation(body)
            client.send_operation(body)
            self.assertEqual(calls[-1], calls[-2])
            self.assertEqual(calls[-1][1], "Bearer test-token")
            artifact = dict(id="artifact-id", size=8, checksum=hashlib.sha256(b"artifact").hexdigest())
            self.assertEqual(client.artifact("run", artifact), b"artifact")
            artifact["size"] = 9
            with self.assertRaises(ValueError):
                client.artifact("run", artifact)
            with self.assertRaises(ValueError):
                operation(assignment, "start", lease_token="replacement")
        finally:
            server.shutdown()
            thread.join()
            server.server_close()


if __name__ == "__main__":
    unittest.main()

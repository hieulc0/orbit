"""Disposable authenticated Git/Responses fixture. Never contacts a model provider."""
import base64
import functools
import http.server
import json
import pathlib
import socket
import sys
import time

root = pathlib.Path(sys.argv[1])
config = json.loads((root / "remote-fixture.json").read_text())


class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        with (root / "git-requests.jsonl").open("a") as log:
            log.write(json.dumps({"path": self.path}) + "\n")
        expected = "Basic " + base64.b64encode(
            ("x-access-token:" + config["git_token"]).encode()
        ).decode()
        if self.headers.get("Authorization") != expected:
            self.send_response(401)
            self.send_header("WWW-Authenticate", 'Basic realm="fixture"')
            self.end_headers()
            return
        super().do_GET()

    def do_POST(self):
        assert self.path == "/v1/responses"
        assert self.headers.get("Authorization") == "Bearer " + config["model_token"]
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert request["store"] is False
        assert request["parallel_tool_calls"] is False
        assert request["include"] == ["reasoning.encrypted_content"]
        for tool in request["tools"]:
            assert tool["strict"] is True
            assert tool["parameters"]["additionalProperties"] is False
        mode = json.loads(request["input"][0]["content"].split("Context: ", 1)[1]).get("mode", "normal")
        results = [item for item in request["input"] if item.get("type") == "function_call_output"]
        turn = len(results)
        with (root / "provider-calls.jsonl").open("a") as log:
            log.write(json.dumps({"mode": mode, "turn": turn}) + "\n")
        if mode == "lost":
            self.connection.shutdown(socket.SHUT_RDWR)
            self.connection.close()
            return
        if mode == "delay":
            (root / "provider-dispatched").write_text("ready")
            time.sleep(60)
        calls = [
            ("read_file", {"path": "calc.sh"}),
            ("write_file", {"path": "calc.sh", "content": "#!/bin/sh\nprintf '%s\\n' 0\n"}),
            ("shell", {"command": "sh test.sh"}),
            ("write_file", {"path": "calc.sh", "content": "#!/bin/sh\nprintf '%s\\n' \"$(( $1 + $2 ))\"\n"}),
            ("shell", {"command": "sh test.sh && test ! -e /run/orbit && test -d .git && test -z \"$ORBIT_TOKEN$ORBIT_GIT_PASSWORD$PROVIDER_API_KEY\" && test ! -e '" + config["host_marker"] + "' && ! touch /orbit-host-write-check && test \"$(ls /sys/class/net)\" = lo && test \"$(cat /sys/fs/cgroup/memory.max)\" = 536870912 && test \"$(cat /sys/fs/cgroup/cpu.max)\" = '100000 100000' && test \"$(cat /sys/fs/cgroup/pids.max)\" = 128"}),
        ]
        if mode == "forbidden":
            name, args = "network", {"url": "https://example.invalid"}
        elif mode == "sleep_tool" and not (root / "tool-sleep-sent").exists():
            (root / "tool-sleep-sent").write_text("sent")
            name, args = "shell", {"command": "echo running > tool-running; sleep 60; echo bad > tool-finished"}
        elif turn < len(calls):
            name, args = calls[turn]
        else:
            name, args = None, None
        if turn == 3 and mode == "normal":
            assert json.loads(results[-1]["output"])["exit_code"] != 0
        if turn == 5 and mode == "normal":
            assert json.loads(results[-1]["output"])["exit_code"] == 0
        output = ([{"type": "function_call", "name": name, "arguments": json.dumps(args),
                    "call_id": f"call_{turn}", "id": f"fc_{turn}", "status": "completed"}]
                  if name else [{"type": "message", "role": "assistant", "id": "msg_fixture", "status": "completed",
                                 "content": [{"type": "output_text", "text": "Fixed addition; tests now pass.", "annotations": []}]}])
        response = json.dumps({"id": f"resp_fixture_{turn}", "status": "completed", "model": request["model"],
                               "usage": {"total_tokens": 100}, "output": output}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        try:
            self.wfile.write(response)
        except BrokenPipeError:
            pass


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=str(root)))
(root / "remote-address").write_text(f"http://127.0.0.1:{server.server_port}")
server.serve_forever()

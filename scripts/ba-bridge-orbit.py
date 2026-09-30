#!/usr/bin/env python3
"""Submit one explicit structured bridge turn to an operator-pinned Orbit role.

The bridge export supplies transport provenance. Orbit validates role authority,
artifact schemas, revision ownership, idempotency and frozen workflow identity.
"""
import argparse
import json
import subprocess
import sys
import threading
from pathlib import Path

MAX_EXPORT_BYTES = 1024 * 1024
MAX_FRAME_BYTES = 1024 * 1024


def selected_artifact(path, message_id):
    with Path(path).open("rb") as source:
        raw = source.read(MAX_EXPORT_BYTES + 1)
    if len(raw) > MAX_EXPORT_BYTES:
        raise ValueError("bridge export exceeds bounds; export a bounded conversation")
    export = json.loads(raw)
    selected = [message for message in export["messages"] if message["id"] == message_id]
    if len(selected) != 1:
        raise ValueError("select exactly one durable bridge message")
    message = selected[0]
    actors = [actor for actor in export["participants"] if actor["id"] == message["actor_id"]]
    if len(actors) != 1 or actors[0]["role"] not in ("business_analyst", "system_architect"):
        raise ValueError("message must belong to a BA or SA participant")
    content = message["content"].strip()
    if content.startswith("```json\n") and content.endswith("\n```"):
        content = content[8:-4]
    artifact = json.loads(content)
    if set(artifact) != {"kind", "payload"}:
        raise ValueError("message must contain exactly one typed reasoning artifact")
    return artifact


class AcpPeer:
    def __init__(self, command):
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL)
        self.sequence = 0

    def request(self, method, params):
        self.sequence += 1
        payload = json.dumps({"jsonrpc": "2.0", "id": self.sequence,
                              "method": method, "params": params}, separators=(",", ":"))
        if len(payload.encode()) > MAX_FRAME_BYTES:
            raise ValueError("ACP request exceeds bounds")
        self.process.stdin.write((payload + "\n").encode())
        self.process.stdin.flush()
        deadline = threading.Timer(30, self.process.kill)
        deadline.daemon = True
        deadline.start()
        observed = 0
        try:
            for _ in range(4096):
                line = self.process.stdout.readline(MAX_FRAME_BYTES + 1)
                observed += len(line)
                if not line or len(line) > MAX_FRAME_BYTES or observed > 20 * 1024 * 1024:
                    raise ValueError("ACP stream closed or exceeded bounds")
                message = json.loads(line)
                if message.get("id") != self.sequence:
                    continue
                if "error" in message:
                    raise ValueError("Orbit rejected the request; inspect durable workflow state")
                return message["result"]
            raise ValueError("ACP message budget exhausted")
        finally:
            deadline.cancel()

    def close(self):
        try:
            self.process.stdin.close()
        except BrokenPipeError:
            pass
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--orbit", default="target/debug/orbit")
    parser.add_argument("--config", required=True)
    parser.add_argument("--database-url-file", required=True)
    parser.add_argument("--session", required=True)
    parser.add_argument("--expected-revision", required=True, type=int)
    parser.add_argument("--bridge-export", required=True)
    parser.add_argument("--message", required=True)
    args = parser.parse_args()
    artifact = selected_artifact(args.bridge_export, args.message)
    config = json.loads(Path(args.config).read_text())
    if config.get("external_role") not in ("business_analyst", "system_architect"):
        raise ValueError("use an operator-pinned external-role configuration")
    peer = AcpPeer([args.orbit, "acp-serve", "--config", args.config,
                    "--database-url-file", args.database_url_file])
    try:
        peer.request("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        peer.request("session/load", {"sessionId": args.session, "cwd": config["repository"],
                                       "mcpServers": []})
        result = peer.request("_orbit/reasoning/submit", {
            "sessionId": args.session, "expectedRevision": args.expected_revision,
            "requestId": args.message, "artifact": artifact})
        print(json.dumps({"sessionId": args.session, "bridgeMessageId": args.message,
                          "revision": result["revision"]}))
    finally:
        peer.close()


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError):
        print("Bridge artifact submission failed; no execution authority was granted.", file=sys.stderr)
        sys.exit(1)

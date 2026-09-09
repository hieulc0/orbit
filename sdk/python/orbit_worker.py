"""Synchronous Orbit worker transport. No automatic execution or mutation retries.

Retain request bodies until acknowledged. Run heartbeats independently of work;
stop work if ownership cannot be confirmed before lease expiry.
"""
import hashlib
import json
import uuid
from urllib.request import Request, build_opener, HTTPRedirectHandler
from urllib.error import HTTPError
from urllib.parse import quote, urlsplit


class OrbitError(Exception):
    def __init__(self, status, body):
        self.status = status
        self.body = body
        super().__init__(f"Orbit request failed ({status})")


class _NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def operation(assignment, action, *, request_id=None, **payload):
    """Build once, persist, and retransmit unchanged after an uncertain response."""
    reserved = {"request_id", "run_id", "attempt_id", "generation", "lease_token", "operation"}
    if reserved.intersection(payload):
        raise ValueError("payload cannot replace operation identity")
    return {
        "request_id": request_id or str(uuid.uuid4()),
        **{k: assignment[k] for k in ("run_id", "attempt_id", "generation", "lease_token")},
        "operation": action, **payload,
    }


class Client:
    def __init__(self, url, token, timeout=30):
        parsed = urlsplit(url)
        if parsed.scheme not in ("http", "https") or not parsed.netloc or parsed.username or parsed.query or parsed.fragment:
            raise ValueError("expected an HTTP(S) server URL without credentials/query/fragment")
        self.url = url.rstrip("/")
        self._token = token
        self.timeout = timeout
        self._opener = build_opener(_NoRedirect())

    def _request(self, path, body=None, binary=False):
        request = Request(self.url + path,
                          data=None if body is None else json.dumps(body).encode(),
                          headers={"Authorization": "Bearer " + self._token,
                                   "Content-Type": "application/json"})
        try:
            with self._opener.open(request, timeout=self.timeout) as response:
                data = response.read()
        except HTTPError as error:
            raise OrbitError(error.code, error.read()) from None
        return data if binary else json.loads(data)

    def register(self, capabilities, recovery_policies=("restart_from_inputs",)):
        return self._request("/worker/register", {
            "protocol_version": "orbit/v0", "capabilities": list(capabilities),
            "recovery_policies": list(recovery_policies)})

    def claim(self, capability, *, request_id):
        return self._request("/worker/claim", {"capability": capability, "request_id": request_id})

    def send_operation(self, body):
        result = self._request("/worker/operate", body)
        if result.get("status") != "accepted":
            raise OrbitError(result.get("status"), result)
        return result

    def get_attempt(self, assignment):
        return self._request("/worker/runs/" + quote(assignment["run_id"], safe="") +
                             "/attempts/" + quote(assignment["attempt_id"], safe=""))

    def upload(self, finalize_operation, data):
        """Use prepare_artifact first; retain this finalize operation for retries."""
        if finalize_operation["operation"] != "finalize_artifact":
            raise ValueError("expected finalize_artifact operation")
        result = self._request("/worker/upload", {
            "operation": finalize_operation, "hex_bytes": data.hex()})
        if result.get("status") != "accepted":
            raise OrbitError(result.get("status"), result)
        return result

    def artifact(self, run_id, artifact):
        data = self._request("/runs/" + quote(run_id, safe="") + "/artifacts/" +
                             quote(artifact["id"], safe=""), binary=True)
        if len(data) != artifact["size"] or hashlib.sha256(data).hexdigest() != artifact["checksum"]:
            raise ValueError("artifact checksum mismatch")
        return data

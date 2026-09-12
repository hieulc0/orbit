"""Private deterministic process fixture; never calls a model/provider."""
import json
import os
from pathlib import Path
import time

payload = json.loads(Path(os.environ["ORBIT_AGENT_INPUT"]).read_text())
context = payload["agent"]["context"]
assert "lease_token" not in payload and "plan" not in payload
assert "ORBIT_TOKEN" not in os.environ and "ORBIT_TOKEN_FILE" not in os.environ
assert "DATABASE_URL" not in os.environ
assert os.environ["PROVIDER_CREDENTIAL"] == "fixture-provider-credential-000000"
Path(context["marker"]).write_text(str(os.getpid()))
time.sleep(context.get("delay", 0))
if context.get("invalid"):
    print("invalid-json-response")
else:
    print(json.dumps({"output":{"task":payload["task"], "credentials_isolated":True}}))

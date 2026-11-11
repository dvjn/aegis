#!/usr/bin/env python3
"""Fake vendor process used only by the AGS black-box tests."""
import json
import os
import pathlib
import re
import sys
import urllib.request

args = sys.argv[1:]
name = pathlib.Path(sys.argv[0]).name
extension = None
if name == "claude":
    endpoint = os.environ["ANTHROPIC_BASE_URL"]
elif name == "agy":
    endpoint = os.environ["CLOUD_CODE_URL"]
    assert "AGY_LLM_GATEWAY_URL" not in os.environ
elif name == "codex":
    config = [args[i+1] for i, a in enumerate(args[:-1]) if a == "--config"]
    endpoint = json.loads(next(c.split("=", 1)[1] for c in config if c.startswith("model_providers.ags.base_url=")))
    assert "model_providers.ags.requires_openai_auth=true" in config
    assert "model_providers.ags.supports_websockets=false" in config
else:
    assert "--transport" not in args
    extension = args[args.index("--extension") + 1]
    source = pathlib.Path(extension).read_text()
    config = json.loads(re.search(r"const config = (.+);", source)[1])
    selected = config["defaults"].get("openai-codex", config["defaults"].get("anthropic"))
    endpoint = next(route["endpoint"] for route in config["routes"] if route["id"] == selected)
    assert 'transport: "sse"' in source
    assert "getProvider" in source
request_path = "/v1internal:streamGenerateContent?alt=sse" if name == "agy" else "/responses?trial=true"
request = urllib.request.Request(endpoint + request_path, b"prompt", {"Authorization": "Bearer vendor-credential"})
if name == "pi" and "--all-providers" in args:
    import concurrent.futures
    def invoke(route):
        req = urllib.request.Request(route["endpoint"] + "/responses?trial=true", b"prompt", {"Authorization": "Bearer vendor-credential"})
        with urllib.request.urlopen(req) as response:
            assert response.read() == b"data: OK\n\n"
    with concurrent.futures.ThreadPoolExecutor() as pool:
        list(pool.map(invoke, config["routes"]))
else:
    with urllib.request.urlopen(request) as response:
        assert response.read() == b"data: OK\n\n"
if "--refresh-during-run" in args:
    state_path=pathlib.Path(os.environ["AGS_HOME"]) / "connection.json"
    state=json.loads(state_path.read_text());state["expires_at"]=0
    state_path.write_text(json.dumps(state))
    with urllib.request.urlopen(request) as response:
        assert response.read() == b"data: OK\n\n"
print(json.dumps({"endpoint": endpoint, "args": args, "extension": extension, "config_dir": os.environ.get("CLAUDE_CONFIG_DIR") or os.environ.get("CODEX_HOME"), "pi_config": config if name == "pi" else None,
                  "aegis_auth_in_environment": any(v.startswith("mock-access") or v == "mock-refresh" for v in os.environ.values())}))
sys.exit(7 if "--exit-7" in args else 0)

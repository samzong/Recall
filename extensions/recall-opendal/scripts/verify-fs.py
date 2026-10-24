#!/usr/bin/env python3
"""Run release provider configure/probe/put/list/get against an isolated fs root."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/recall-opendal").resolve()
with tempfile.TemporaryDirectory(prefix="recall-opendal-fs-") as directory:
    root = Path(directory)
    env = dict(os.environ, RECALL_OPENDAL_CONFIG=str(root / "config.json"))
    backend = root / "backend"
    backend.mkdir()
    config = json.dumps({"scheme": "fs", "root": str(backend)})
    command = [str(binary), "--recall-remote-configure", "--config-json", config]
    print(json.dumps({"command": command}), flush=True)
    configured = subprocess.run(command, env=env, text=True, capture_output=True, check=True)
    print(json.dumps({"exit_code": configured.returncode, "stderr": configured.stderr.strip()}), flush=True)
    source = root / "input"
    content = b"Recall OpenDAL real filesystem round trip.\n"
    source.write_bytes(content)
    def request(operation, **fields):
        payload = dict(transport_version=1, timeout_ms=30000, operation=operation, **fields)
        print(json.dumps({"request": payload}), flush=True)
        response = subprocess.run([str(binary), "--recall-remote-transport"], env=env,
                                  input=json.dumps(payload), text=True, capture_output=True)
        parsed = json.loads(response.stdout)
        print(json.dumps({"exit_code": response.returncode, "response": parsed}), flush=True)
        assert response.returncode == 0, response.stderr
        return parsed["result"]
    assert request("probe") == {"readable": True}
    digest = hashlib.sha256(content).hexdigest()
    assert request("put", key="v1/manual/payload.bin", input_path=str(source), size=len(content), sha256=digest) == {"size": len(content), "sha256": digest}
    assert request("list", prefix="v1/", cursor=None, page_size=2) == {"objects": [{"key": "v1/manual/payload.bin", "size": len(content)}], "next_cursor": None}
    output = root / "download"
    assert request("get", key="v1/manual/payload.bin", output_path=str(output), max_bytes=len(content)) == {"size": len(content)}
    assert output.read_bytes() == content
    print(json.dumps({"round_trip_sha256": hashlib.sha256(output.read_bytes()).hexdigest(), "verified": True}), flush=True)
print(json.dumps({"temporary_backend_removed": not Path(directory).exists()}))

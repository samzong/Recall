import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import unittest


PROBE = Path(__file__).with_name("probe-rx-provider")


def frame(data, event=None):
    prefix = f"event: {event}\n" if event else ""
    return prefix + "data: " + json.dumps(data) + "\n\n"


STREAMS = {
    "chat_completions": frame({
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": "pong"}, "finish_reason": None}],
    }) + frame({
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
    }) + "data: [DONE]\n\n",
    "responses": frame({"type": "response.output_text.delta", "delta": "pong"},
                       "response.output_text.delta")
    + frame({"type": "response.completed", "response": {"status": "completed"}},
            "response.completed"),
    "messages": frame({"type": "message_start", "message": {"role": "assistant"}},
                      "message_start")
    + frame({"type": "content_block_delta", "index": 0,
             "delta": {"type": "text_delta", "text": "pong"}}, "content_block_delta")
    + frame({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}, "message_delta")
    + frame({"type": "message_stop"}, "message_stop"),
}
ERROR = frame({"error": {"message": "unsupported protocol"}}, "error")


class ProbeTests(unittest.TestCase):
    def setUp(self):
        self.home = tempfile.TemporaryDirectory()
        self.addCleanup(self.home.cleanup)
        Path(self.home.name, ".curlrc").write_text("")
        self.streams = dict(STREAMS)
        self.status = 200
        self.content_type = "text/event-stream"
        self.interim_sse = False
        self.delay = 0
        self.prefix = ": heartbeat\n\n"
        self.truncated = False
        self.models = [{"id": "test-model"}]
        case = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                self.send_response(case.status)
                self.end_headers()
                self.wfile.write(json.dumps({"data": case.models}).encode())

            def do_POST(self):
                self.rfile.read(int(self.headers["Content-Length"]))
                protocol = "chat_completions" if self.path.endswith("/chat/completions") else self.path.rsplit("/", 1)[-1]
                body = case.streams[protocol].encode()
                if case.interim_sse:
                    self.wfile.write(b"HTTP/1.1 103 Early Hints\r\nContent-Type: text/event-stream\r\n\r\n")
                self.send_response(case.status)
                if case.content_type is not None:
                    self.send_header("Content-Type", case.content_type)
                if case.truncated:
                    self.send_header("Content-Length", str(len(body) + 100))
                self.end_headers()
                try:
                    if case.delay:
                        self.wfile.write(case.prefix.encode())
                        self.wfile.flush()
                        time.sleep(case.delay)
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)

    def probe(self, admitted, failed=()):
        env = {"PATH": os.defpath, "HOME": self.home.name,
               "CURL_HOME": self.home.name, "RX_PROVIDER_KEY": "synthetic"}
        result = subprocess.run(
            ["sh", str(PROBE), "--base-url", f"http://127.0.0.1:{self.server.server_port}/v1", "--json"],
            env=env, capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0 if admitted else 1, result.stderr + result.stdout)
        document = json.loads(result.stdout)
        self.assertEqual(document["verdict"], "ADMIT" if admitted else "REJECT")
        checks = {check["id"]: check["pass"] for check in document["checks"]}
        for protocol in failed:
            self.assertFalse(checks[protocol], document)

    def test_requires_final_sse_media_type(self):
        for media_type in ("application/json", "text/plain", None):
            with self.subTest(media_type=media_type):
                self.content_type = media_type
                self.probe(False, STREAMS)
        self.interim_sse = True
        self.probe(False, STREAMS)

    def test_accepts_media_type_parameters(self):
        self.content_type = "Text/Event-Stream; charset=utf-8"
        self.probe(True)

    def test_success_after_heartbeat(self):
        self.delay = 1.1
        self.probe(True)

    def test_rejects_delayed_error_after_content(self):
        self.prefix = STREAMS["chat_completions"].split("\n\n")[0] + "\n\n"
        self.delay = 1.1
        self.streams = dict.fromkeys(STREAMS, ERROR)
        self.probe(False, STREAMS)

    def test_crlf_multiline_data_and_unknown_events(self):
        self.streams = {
            protocol: (frame({"type": "future.event"}, "future.event") + body)
            .replace('"delta": "pong"', '"delta":\ndata: "pong"').replace("\n", "\r\n")
            for protocol, body in STREAMS.items()
        }
        self.probe(True)

    def test_rejects_empty_error_heartbeat_json_and_malformed_streams(self):
        for body in ("", ERROR, "event: error\n\n", ": heartbeat\n\n",
                     frame({"type": "ping"}, "ping"), '{"output":"pong"}',
                     "data: {bad json}\n\n", "data: [DONE]\n\n"):
            with self.subTest(body=body):
                self.streams = dict.fromkeys(STREAMS, body)
                self.probe(False, STREAMS)

    def test_rejects_partial_and_failed_streams(self):
        for transform in (
            lambda body: body[:body.rfind("\n\n", 0, len(body) - 2) + 2],
            lambda body: body.rstrip("\n"),
            lambda body: body + ERROR,
            lambda body: body.replace("pong", ""),
        ):
            with self.subTest(transform=transform):
                self.streams = {protocol: transform(body) for protocol, body in STREAMS.items()}
                self.probe(False, STREAMS)

    def test_rejects_wrong_protocol_and_incomplete_response(self):
        for protocol in STREAMS:
            with self.subTest(protocol=protocol):
                self.streams = dict(STREAMS)
                other = "messages" if protocol != "messages" else "responses"
                self.streams[protocol] = STREAMS[other]
                self.probe(False, [protocol])
        self.streams = dict(STREAMS)
        self.streams["responses"] = STREAMS["responses"].replace("completed", "incomplete")
        self.probe(False, ["responses"])

    def test_rejects_transport_failure_after_success_frames(self):
        self.truncated = True
        self.probe(False, STREAMS)

    def test_requires_models_and_successful_http_status(self):
        self.models = []
        self.probe(False, ["openai_models"])
        self.models = [{"id": "test-model"}]
        self.status = 500
        self.probe(False, STREAMS)


if __name__ == "__main__":
    unittest.main()

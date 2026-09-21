"""Opt-in local CLI handshake; no real credentials or remote endpoint.

python tests/claude_native_probe.py ABSOLUTE_OFFICIAL_CLAUDE_EXECUTABLE
Default submits no prompt. --mock-turn uses a fixed prompt and a local SSE
fixture. --record FILE saves normalized mock-turn frames for Rust replay tests.
This is not a live Claude model test.
"""
import json
import http.server
import os
import queue
import subprocess
import sys
import tempfile
import threading


def main():
    executable = os.path.abspath(sys.argv[1])
    server = None
    endpoint = "http://127.0.0.1:1"
    if "--mock-turn" in sys.argv:
        class FixtureServer(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
                if "count_tokens" in self.path:
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(b'{"input_tokens":20}')
                    return
                assert body["model"] == "claude-sonnet-4-6"
                message = {"id":"msg_offline_fixture", "type":"message", "role":"assistant", "model":body["model"],
                           "content":[], "stop_reason":None, "stop_sequence":None,
                           "usage":{"input_tokens":20,"output_tokens":1}}
                frames = [
                    {"type":"message_start", "message":message},
                    {"type":"content_block_start", "index":0, "content_block":{"type":"thinking", "thinking":"", "signature":""}},
                    {"type":"content_block_delta", "index":0, "delta":{"type":"thinking_delta", "thinking":"Offline reasoning"}},
                    {"type":"content_block_delta", "index":0, "delta":{"type":"signature_delta", "signature":"offline-signature"}},
                    {"type":"content_block_stop", "index":0},
                    {"type":"content_block_start", "index":1, "content_block":{"type":"text", "text":""}},
                    {"type":"content_block_delta", "index":1, "delta":{"type":"text_delta", "text":"Offline fixture OK"}},
                    {"type":"content_block_stop", "index":1},
                    {"type":"message_delta", "delta":{"stop_reason":"end_turn", "stop_sequence":None}, "usage":{"output_tokens":4}},
                    {"type":"message_stop"},
                ]
                payload = "".join("event: " + f["type"] + "\ndata: " + json.dumps(f) + "\n\n" for f in frames).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FixtureServer)
        endpoint = "http://127.0.0.1:" + str(server.server_port)
        threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="claude-native-probe-") as directory:
        env = {k: v for k, v in os.environ.items()
               if not k.upper().startswith(("ANTHROPIC_", "CLAUDE_"))}
        env.update(CLAUDE_CONFIG_DIR=directory, ANTHROPIC_API_KEY="offline-fixture-only",
                   ANTHROPIC_BASE_URL=endpoint, CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1",
                   CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK="1", DISABLE_AUTOUPDATER="1")
        args = [executable, "--print", "--input-format", "stream-json", "--output-format",
                "stream-json", "--verbose", "--include-partial-messages", "--safe-mode",
                "--model", "claude-sonnet-4-6", "--tools", "Read,Glob,Grep,AskUserQuestion",
                "--setting-sources", "", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
                "--permission-prompt-tool", "stdio", "--permission-mode", "default",
                "--no-session-persistence", "--no-chrome", "--disable-slash-commands",
                "--fallback-model", "", "--settings", '{"disableAllHooks":true,"enabledPlugins":{},"autoMemoryEnabled":false,"fallbackModel":[],"permissions":{"defaultMode":"default","ask":["Bash","Edit","Write","NotebookEdit"]}}']
        process = subprocess.Popen(args, cwd=directory, env=env, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   text=True, encoding="utf-8", creationflags=0x08000000 if os.name == "nt" else 0)
        frames = queue.Queue()
        def collect():
            for line in process.stdout:
                frames.put(json.loads(line))
        threading.Thread(target=collect, daemon=True).start()
        try:
            for index, subtype in enumerate(["initialize", "get_settings", "mcp_status"]):
                request = {"subtype": subtype}
                if subtype == "initialize":
                    request.update(hooks={}, agents={}, sdkMcpServers=[], promptSuggestions=False)
                process.stdin.write(json.dumps({"type":"control_request", "request_id":str(index), "request":request}) + "\n")
                process.stdin.flush()
                while True:
                    frame = frames.get(timeout=20)
                    if frame.get("type") == "control_response" and frame.get("response", {}).get("request_id") == str(index):
                        response = frame["response"]
                        payload = response.get("response", {})
                        print(json.dumps({"control":subtype, "status":response.get("subtype"),
                                          "keys":sorted(payload), "applied":payload.get("applied"),
                                          "apiProvider":payload.get("account", {}).get("apiProvider"),
                                          "mcpServers":payload.get("mcpServers"),
                                          "effectiveKeys":sorted(payload.get("effective", {})),
                                          "sources":[{"source":s.get("source"), "keys":sorted(s.get("settings", {}))} for s in payload.get("sources", [])],
                                          "error":response.get("error")}), flush=True)
                        break
            if server is not None:
                recorded = []
                process.stdin.write(json.dumps({"type":"user", "message":{"role":"user", "content":"Offline fixture; reply with fixture text."}}) + "\n")
                process.stdin.flush()
                while True:
                    frame = frames.get(timeout=20)
                    # Isolated fake account and fixed fixture content only.
                    print(json.dumps(frame), flush=True)
                    recorded_frame = dict(frame)
                    recorded_frame["session_id"] = "session-fixture"
                    recorded_frame.pop("uuid", None)
                    if "cwd" in recorded_frame:
                        recorded_frame["cwd"] = "OFFLINE_FIXTURE_WORKSPACE"
                    recorded.append(recorded_frame)
                    if frame.get("type") == "result":
                        assert frame["subtype"] == "success" and not frame["is_error"]
                        assert frame["result"] == "Offline fixture OK"
                        break
                if "--record" in sys.argv:
                    destination = sys.argv[sys.argv.index("--record") + 1]
                    with open(destination, "w", encoding="utf-8") as fixture:
                        for frame in recorded:
                            fixture.write(json.dumps(frame) + "\n")
        finally:
            process.kill()
            process.wait(timeout=5)
            # The isolated child has no real account, so startup errors are safe to report.
            stderr = process.stderr.read(2000)
            if stderr:
                print(stderr, file=sys.stderr)
            if server is not None:
                server.shutdown()


if __name__ == "__main__":
    main()

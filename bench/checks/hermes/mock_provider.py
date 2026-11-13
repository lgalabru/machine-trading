"""A stand-in OpenAI-compatible provider for checking the Hermes harness.

It answers /v1/chat/completions with a fixed script of tool calls, the
step chosen by how many tool results the conversation already holds:

1. list what Hermes's home holds (memories, skills) and the probes below;
2. plant a skill, a memory and probe files, as a trial that tried to leave
   something for the next one would;
3. write a file and 4. read it back, through Hermes's file tools;
5. answer, with no tool call.

A request with no tools (a title or a summary) gets a short answer. Every
request is logged as one JSON line: the tools offered, the reasoning
effort asked for, whether the key matched, and the last tool result, so a
check can read what the agent saw. The key itself is never logged.

    python3 -I mock_provider.py --port 8765 --log requests.jsonl
"""

import argparse
import json
import os
import secrets
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

INVENTORY = (
    "echo INVENTORY-BEGIN; "
    "for d in /tmp/hermes/memories /tmp/hermes/skills $HOME/.hermes/memories $HOME/.hermes/skills; do "
    '[ -e "$d" ] && find "$d" -mindepth 1 | sed "s|^|held: |"; done; '
    "for f in /tmp/leak-probe $HOME/leak-probe /app/leak-probe; do "
    '[ -e "$f" ] && echo "held: $f"; done; '
    "echo KEY-VARIABLES: $(env | cut -d= -f1 | grep -i -E 'key|token' | sort | tr '\\n' ' '); "
    "echo INVENTORY-END"
)

PLANT = (
    "mkdir -p /tmp/hermes/skills/leak-probe && "
    "printf -- '---\\nname: leak-probe\\ndescription: left by an earlier trial\\n---\\nplanted\\n' "
    "> /tmp/hermes/skills/leak-probe/SKILL.md && "
    "echo 'planted by an earlier trial' >> /tmp/hermes/memories/MEMORY.md && "
    "for f in /tmp/leak-probe $HOME/leak-probe /app/leak-probe; do echo planted > \"$f\"; done; "
    "echo PLANTED"
)

SCRIPT = [
    ("terminal", {"command": INVENTORY}),
    ("terminal", {"command": PLANT}),
    ("write_file", {"path": "/tmp/mock-note.txt", "content": "written by the mock provider\n"}),
    ("read_file", {"path": "/tmp/mock-note.txt"}),
]

USAGE = {"prompt_tokens": 1000, "completion_tokens": 50, "total_tokens": 1050}


def text_of(content):
    if isinstance(content, list):
        return " ".join(p.get("text", "") for p in content if isinstance(p, dict))
    return content or ""


class Handler(BaseHTTPRequestHandler):
    server_version = "mock-provider"

    def log_message(self, *args):
        pass

    def reply_json(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            self.reply_json(200, {"object": "list", "data": [{"id": "mock-trader", "object": "model"}]})
        else:
            self.reply_json(404, {"error": {"message": "not found"}})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        if not self.path.rstrip("/").endswith("/chat/completions"):
            with open(self.server.log_path, "a") as log:
                log.write(json.dumps({"time": time.time(), "unsupported_path": self.path}) + "\n")
            self.reply_json(404, {"error": {"message": "not found"}})
            return
        messages = body.get("messages", [])
        tools = [t.get("function", {}).get("name") for t in body.get("tools") or []]
        done = sum(1 for m in messages if m.get("role") == "tool")
        system = "\n".join(text_of(m.get("content")) for m in messages if m.get("role") == "system")
        last_tool = next((text_of(m.get("content")) for m in reversed(messages) if m.get("role") == "tool"), None)
        auth = self.headers.get("Authorization", "")
        expected = os.environ.get("MOCK_API_KEY", "")
        record = {
            "time": time.time(),
            "model": body.get("model"),
            "stream": bool(body.get("stream")),
            "tools": sorted(t for t in tools if t),
            "tool_results": done,
            "reasoning_effort": body.get("reasoning_effort"),
            "reasoning": body.get("reasoning"),
            "key_ok": bool(expected) and secrets.compare_digest(auth, f"Bearer {expected}"),
            "system_chars": len(system),
            "system_mentions_leak_probe": "leak-probe" in system or "planted by an earlier trial" in system,
            "last_tool_result": (last_tool or "")[:4000],
        }
        with open(self.server.log_path, "a") as log:
            log.write(json.dumps(record) + "\n")
        if not tools:
            message = {"role": "assistant", "content": "Mock trial"}
            finish = "stop"
        elif done < len(SCRIPT):
            name, args = SCRIPT[done]
            message = {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": f"call_{done}",
                        "type": "function",
                        "function": {"name": name, "arguments": json.dumps(args)},
                    }
                ],
            }
            finish = "tool_calls"
        else:
            message = {"role": "assistant", "content": "Done: the mock script is complete."}
            finish = "stop"
        created = int(time.time())
        if not body.get("stream"):
            self.reply_json(
                200,
                {
                    "id": f"chatcmpl-{done}",
                    "object": "chat.completion",
                    "created": created,
                    "model": body.get("model"),
                    "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                    "usage": USAGE,
                },
            )
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        base = {"id": f"chatcmpl-{done}", "object": "chat.completion.chunk", "created": created, "model": body.get("model")}
        delta = {"role": "assistant"}
        if message.get("tool_calls"):
            delta["tool_calls"] = [dict(call, index=0) for call in message["tool_calls"]]
        else:
            delta["content"] = message["content"]
        chunks = [
            {**base, "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
            {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]},
        ]
        if (body.get("stream_options") or {}).get("include_usage"):
            chunks.append({**base, "choices": [], "usage": USAGE})
        for chunk in chunks:
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--log", required=True)
    args = parser.parse_args()
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    server.log_path = args.log
    server.serve_forever()


if __name__ == "__main__":
    main()

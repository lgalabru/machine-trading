"""A stand-in OpenAI-compatible provider for checking the Hermes harness.

It answers /v1/chat/completions and /v1/responses (OpenAI's own models
run on the latter) with a fixed script of tool calls, the
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
    "echo KEY-VARIABLES: $(compgen -e | grep -i -E 'key|token' | sort | tr '\\n' ' '); "
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
        path = self.path.rstrip("/")
        if path.endswith("/chat/completions"):
            api = "chat"
            items = body.get("messages", [])
            tools = [t.get("function", {}).get("name") for t in body.get("tools") or []]
            results = [text_of(m.get("content")) for m in items if m.get("role") == "tool"]
            system = "\n".join(text_of(m.get("content")) for m in items if m.get("role") == "system")
        elif path.endswith("/responses"):
            api = "responses"
            items = body.get("input") if isinstance(body.get("input"), list) else []
            tools = [t.get("name") for t in body.get("tools") or [] if t.get("type") == "function"]
            results = [text_of(i.get("output")) for i in items if i.get("type") == "function_call_output"]
            system = "\n".join([body.get("instructions") or ""] + [
                text_of(i.get("content")) for i in items if i.get("role") in ("system", "developer")
            ])
        else:
            with open(self.server.log_path, "a") as log:
                log.write(json.dumps({"time": time.time(), "unsupported_path": self.path}) + "\n")
            self.reply_json(404, {"error": {"message": "not found"}})
            return
        done = len(results)
        auth = self.headers.get("Authorization", "")
        expected = os.environ.get("MOCK_API_KEY", "")
        record = {
            "time": time.time(),
            "api": api,
            "model": body.get("model"),
            "stream": bool(body.get("stream")),
            "tools": sorted(t for t in tools if t),
            "tool_results": done,
            "reasoning_effort": body.get("reasoning_effort"),
            "reasoning": body.get("reasoning"),
            "key_ok": bool(expected) and secrets.compare_digest(auth, f"Bearer {expected}"),
            "system_chars": len(system),
            "system_mentions_leak_probe": "leak-probe" in system or "planted by an earlier trial" in system,
            "last_tool_result": (results[-1] if results else "")[:4000],
        }
        with open(self.server.log_path, "a") as log:
            log.write(json.dumps(record) + "\n")
        if not record["key_ok"]:
            self.reply_json(401, {"error": {"message": "invalid api key", "type": "invalid_request_error"}})
            return
        if tools and done < len(SCRIPT):
            call = (f"call_{done}", *SCRIPT[done])
            text = None
        else:
            call = None
            text = "Done: the mock script is complete." if tools else "Mock trial"
        if api == "chat":
            self.answer_chat(body, done, call, text)
        else:
            self.answer_responses(body, done, call, text)

    def sse(self, events):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        for name, event in events:
            prefix = f"event: {name}\n" if name else ""
            self.wfile.write(f"{prefix}data: {json.dumps(event)}\n\n".encode())
        self.wfile.flush()

    def answer_chat(self, body, done, call, text):
        if call:
            call_id, name, args = call
            message = {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {"id": call_id, "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}
                ],
            }
            finish = "tool_calls"
        else:
            message = {"role": "assistant", "content": text}
            finish = "stop"
        base = {"id": f"chatcmpl-{done}", "created": int(time.time()), "model": body.get("model")}
        if not body.get("stream"):
            self.reply_json(
                200,
                {
                    **base,
                    "object": "chat.completion",
                    "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                    "usage": USAGE,
                },
            )
            return
        base["object"] = "chat.completion.chunk"
        delta = {"role": "assistant"}
        if call:
            delta["tool_calls"] = [dict(c, index=0) for c in message["tool_calls"]]
        else:
            delta["content"] = text
        events = [
            (None, {**base, "choices": [{"index": 0, "delta": delta, "finish_reason": None}]}),
            (None, {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}),
        ]
        if (body.get("stream_options") or {}).get("include_usage"):
            events.append((None, {**base, "choices": [], "usage": USAGE}))
        self.sse(events)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def answer_responses(self, body, done, call, text):
        response_id = f"resp_{done}_{secrets.token_hex(4)}"
        if call:
            call_id, name, args = call
            item = {
                "type": "function_call",
                "id": f"fc_{done}",
                "call_id": call_id,
                "name": name,
                "arguments": json.dumps(args),
                "status": "completed",
            }
        else:
            item = {
                "type": "message",
                "id": f"msg_{done}",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": text, "annotations": []}],
            }
        usage = {
            "input_tokens": USAGE["prompt_tokens"],
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": USAGE["completion_tokens"],
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": USAGE["total_tokens"],
        }
        response = {
            "id": response_id,
            "object": "response",
            "created_at": int(time.time()),
            "model": body.get("model"),
            "status": "completed",
            "output": [item],
            "usage": usage,
        }
        if not body.get("stream"):
            self.reply_json(200, response)
            return
        started = {**response, "status": "in_progress", "output": [], "usage": None}
        opened = {**item, "status": "in_progress"}
        if call:
            opened["arguments"] = ""
            middle = [
                ("response.function_call_arguments.delta",
                 {"item_id": item["id"], "output_index": 0, "delta": item["arguments"]}),
                ("response.function_call_arguments.done",
                 {"item_id": item["id"], "output_index": 0, "arguments": item["arguments"]}),
            ]
        else:
            opened["content"] = []
            part = {"type": "output_text", "text": "", "annotations": []}
            middle = [
                ("response.content_part.added", {"item_id": item["id"], "output_index": 0, "content_index": 0, "part": part}),
                ("response.output_text.delta", {"item_id": item["id"], "output_index": 0, "content_index": 0, "delta": text}),
                ("response.output_text.done", {"item_id": item["id"], "output_index": 0, "content_index": 0, "text": text}),
                ("response.content_part.done",
                 {"item_id": item["id"], "output_index": 0, "content_index": 0, "part": dict(part, text=text)}),
            ]
        events = [
            ("response.created", {"response": started}),
            ("response.in_progress", {"response": started}),
            ("response.output_item.added", {"output_index": 0, "item": opened}),
            *middle,
            ("response.output_item.done", {"output_index": 0, "item": item}),
            ("response.completed", {"response": response}),
        ]
        self.sse([(name, dict(event, type=name, sequence_number=n)) for n, (name, event) in enumerate(events)])

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

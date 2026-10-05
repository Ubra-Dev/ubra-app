#!/usr/bin/env python3
"""Scripted stand-in for an OpenAI-compatible Chat Completions API, keyed on
the latest user text.

  SLOW    -> streams a reply over ~5s (exercises the Working state)
  RUNCMD  -> calls the `bash` tool to touch a file (exercises a tool turn)
  other   -> replies with the first ALLCAPS word of the prompt, or OK
Every reply waits ~1s first, like a real API. Requests are logged to argv[2].
Used by tests/pi_real.rs; binds 127.0.0.1 only.
"""
import json, re, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
LOG = open(sys.argv[2], "a", buffering=1)


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return " ".join(p.get("text", "") for p in content if isinstance(p, dict))
    return ""


def last_turn(messages):
    """The text of the last user message, or a marker when the conversation
    ends with a tool result."""
    if messages and messages[-1].get("role") == "tool":
        return "__TOOL_RESULT__"
    for message in reversed(messages):
        if message.get("role") == "user":
            return text_of(message.get("content"))
    return ""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        LOG.write(f"GET {self.path}\n")
        self.reply_json({"object": "list", "data": [{"id": "fake-model", "object": "model"}]})

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        messages = body.get("messages", [])
        text = last_turn(messages)
        users = [text_of(m.get("content")) for m in messages if m.get("role") == "user"]
        LOG.write(
            f"POST {self.path} users={len(users)} last_user={text[:80]!r} "
            f"all_users={[u[:40] for u in users]!r}\n"
        )

        if text == "__TOOL_RESULT__":
            deltas = [{"content": "The command ran. DONECMD"}]
            finish = "stop"
        elif "RUNCMD" in text:
            deltas = [{"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                                       "function": {"name": "bash", "arguments": json.dumps(
                                           {"command": "touch ubra-e2e-file"})}}]}]
            finish = "tool_calls"
        elif "SLOW" in text:
            deltas = [{"content": f"slow part {i} "} for i in range(10)] + [{"content": "SLOWDONE"}]
            finish = "stop"
        else:
            word = next(iter(re.findall(r"\b[A-Z]{4,}\b", text)), "OK")
            deltas = [{"content": word}]
            finish = "stop"

        time.sleep(1.0)
        if not body.get("stream"):
            message = {"role": "assistant", "content": "".join(d.get("content", "") for d in deltas)}
            return self.reply_json({"id": "x", "object": "chat.completion", "model": "fake-model",
                                    "choices": [{"index": 0, "message": message, "finish_reason": finish}]})
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()
        self.event({"role": "assistant", "content": ""}, None)
        for delta in deltas:
            self.event(delta, None)
            if "SLOW" in text:
                time.sleep(0.5)
        self.event({}, finish, usage={"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15})
        self.chunk("data: [DONE]\n\n")
        self.chunk("")

    def event(self, delta, finish, usage=None):
        payload = {"id": "chatcmpl-fake", "object": "chat.completion.chunk", "created": 0,
                   "model": "fake-model",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
        if usage:
            payload["usage"] = usage
        self.chunk(f"data: {json.dumps(payload)}\n\n")

    def chunk(self, data):
        raw = data.encode()
        self.wfile.write(f"{len(raw):x}\r\n".encode() + raw + b"\r\n")
        self.wfile.flush()

    def reply_json(self, value):
        raw = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()

#!/usr/bin/env python3
"""Scripted stand-in for an OpenAI-compatible chat API, keyed on the latest user text.

  SLOW    -> streams a reply over ~5s (exercises the Working state)
  RUNCMD  -> asks to run a bash command (exercises the permission prompt)
  other   -> replies with the first ALLCAPS word of the prompt, or OK
A request without tools (title generation) gets a short title. Every reply
waits ~1s first, like a real API. Requests are logged to argv[2].

The server is also the client's HTTP(S) proxy: it logs and refuses every
request for another host, so the test sees, and blocks, any network the CLI
attempts beyond the model API. Used by tests/copilot_real.rs; binds
127.0.0.1 only.
"""
import json, re, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Server(ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        # The CLI may close an idle keep-alive connection while we read.
        if not isinstance(sys.exc_info()[1], (ConnectionResetError, BrokenPipeError)):
            super().handle_error(request, client_address)

PORT = int(sys.argv[1])
LOG = open(sys.argv[2], "a", buffering=1)


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return " ".join(p.get("text", "") for p in content if isinstance(p, dict))
    return ""


def last_turn(messages):
    """The text of the latest user message, or a marker when a tool answered last."""
    for message in reversed(messages):
        role = message.get("role")
        if role == "tool":
            return "__TOOL_RESULT__"
        if role == "user":
            text = re.sub(r"<current_datetime>.*?</current_datetime>\s*", "", text_of(message.get("content"))).strip()
            if text:
                return text
    return ""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def refuse(self):
        LOG.write(f"PROXY {self.command} {self.path}\n")
        self.send_response(403)
        self.send_header("content-length", "0")
        self.send_header("connection", "close")
        self.end_headers()
        self.close_connection = True

    def do_CONNECT(self):
        self.refuse()

    def proxied(self):
        return self.path.startswith("http://") or self.path.startswith("https://")

    def do_GET(self):
        if self.proxied():
            return self.refuse()
        LOG.write(f"GET {self.path}\n")
        self.reply_json({"object": "list", "data": [{"id": "fake-model", "object": "model"}]})

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        raw = self.rfile.read(length) or b"{}"
        if self.proxied():
            return self.refuse()
        body = json.loads(raw)
        messages = body.get("messages", [])
        text = last_turn(messages)
        tools = bool(body.get("tools"))
        LOG.write(f"POST {self.path} tools={tools} last_user={text!r}\n")
        history = [re.sub(r"<current_datetime>.*?</current_datetime>\s*", "", text_of(m.get("content"))).strip()
                   for m in messages if m.get("role") == "user"]
        LOG.write("HISTORY " + json.dumps(history) + "\n")

        if not tools:
            pieces = [{"content": "Fake title"}]
        elif text == "__TOOL_RESULT__":
            pieces = [{"content": "The command ran. DONECMD"}]
        elif "RUNCMD" in text:
            pieces = [{"tool_calls": [{
                "index": 0, "id": "call_ubra_1", "type": "function",
                "function": {"name": "bash", "arguments": json.dumps(
                    {"command": "touch ubra-e2e-file", "description": "Create the e2e marker file"})},
            }]}]
        elif "SLOW" in text:
            pieces = [{"content": f"slow part {i} "} for i in range(10)] + [{"content": "SLOWDONE"}]
        else:
            word = next(iter(re.findall(r"\b[A-Z]{4,}\b", text)), "OK")
            pieces = [{"content": word}]
        finish = "tool_calls" if any("tool_calls" in p for p in pieces) else "stop"

        time.sleep(1.0)
        if not body.get("stream"):
            message = {"role": "assistant", "content": "".join(p.get("content", "") for p in pieces)}
            calls = [c for p in pieces for c in p.get("tool_calls", [])]
            if calls:
                message["tool_calls"] = [{k: v for k, v in c.items() if k != "index"} for c in calls]
            return self.reply_json({
                "id": "chatcmpl-ubra", "object": "chat.completion", "created": int(time.time()),
                "model": body.get("model", "fake-model"),
                "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
            })

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()
        for i, delta in enumerate(pieces):
            if i == 0:
                delta = {"role": "assistant", **delta}
            self.event(self.chunk_of(body, delta, None))
            if "SLOW" in text and tools:
                time.sleep(0.5)
        last = self.chunk_of(body, {}, finish)
        last["usage"] = {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        self.event(last)
        self.write_chunk("data: [DONE]\n\n")
        self.write_chunk("")

    def chunk_of(self, body, delta, finish):
        return {
            "id": "chatcmpl-ubra", "object": "chat.completion.chunk", "created": int(time.time()),
            "model": body.get("model", "fake-model"),
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        }

    def event(self, value):
        self.write_chunk(f"data: {json.dumps(value)}\n\n")

    def write_chunk(self, data):
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


Server(("127.0.0.1", PORT), Handler).serve_forever()

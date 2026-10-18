"""Home Assistant for the latency bench: reads go to the real one, service calls are never made.

Each service call waits as long as the cluster's log shows the real call taking, then answers
like Home Assistant does. Every call is logged with its time, so the bench can show when the
engine made it relative to the answer.

    python3 -I fake_ha.py <real HA url> <port> <log file>
"""

import http.server
import json
import sys
import time
import urllib.request

REAL, PORT, LOG = sys.argv[1].rstrip("/"), int(sys.argv[2]), sys.argv[3]

# Seconds per service, from the engine's "home assistant service" log lines on 17 Oct.
DELAYS = {
    ("light", "turn_on"): 0.65,
    ("light", "turn_off"): 0.60,
    ("scene", "turn_on"): 0.55,
    ("notify", None): 0.35,
    ("media_player", "media_pause"): 0.12,
    ("media_player", "media_play"): 0.29,
    ("music_assistant", "play_media"): 1.2,
}


# To-do lists kept here, so the shopping list works without changing the real one.
TODO = {}


def todo(service, body):
    """A to-do service call against the kept lists; the response Home Assistant would give."""
    data = json.loads(body or b"{}")
    items = TODO.setdefault(data.get("entity_id", "todo.shopping_list"), [])
    if service == "get_items":
        return {"service_response": {data.get("entity_id", "todo.shopping_list"): {"items": items}}}
    if service == "add_item":
        items.append({"summary": data["item"], "uid": str(len(items) + 1), "status": "needs_action"})
    if service == "update_item":
        for item in items:
            if data["item"] in (item["uid"], item["summary"]):
                item["status"] = data.get("status", item["status"])
    return []


def delay(domain, service):
    return DELAYS.get((domain, service), DELAYS.get((domain, None), 0.03))


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def forward(self, body=None):
        request = urllib.request.Request(REAL + self.path, data=body, method=self.command)
        for header in ("Authorization", "Content-Type"):
            if self.headers.get(header):
                request.add_header(header, self.headers[header])
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                status, data, kind = response.status, response.read(), response.headers.get("Content-Type", "")
        except urllib.error.HTTPError as error:
            status, data, kind = error.code, error.read(), "text/plain"
        self.reply(status, data, kind)

    def reply(self, status, data, kind="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.forward()

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        parts = self.path.split("?")[0].split("/")
        if len(parts) >= 5 and parts[2] == "services":
            domain, service = parts[3], parts[4]
            start = time.time()
            time.sleep(delay(domain, service))
            with open(LOG, "a") as log:
                log.write(f"{start:.3f}\t{time.time():.3f}\t{domain}.{service}\t{body.decode()[:120]}\n")
            if domain == "todo":
                self.reply(200, json.dumps(todo(service, body)).encode())
                return
            self.reply(200, b'{"changed_states": [], "service_response": {}}' if "return_response" in self.path else b"[]")
            return
        self.forward(body)


http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()

"""Count JSON-RPC submission calls to an isolated local validator."""
import json
import threading
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class CountingRpcProxy:
    def __init__(self, upstream):
        self.count = 0
        self.lock = threading.Lock()
        parent = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers['Content-Length']))
                request = json.loads(body)
                calls = request if isinstance(request, list) else [request]
                with parent.lock:
                    parent.count += sum(call.get('method') == 'sendTransaction' for call in calls)
                upstream_request = urllib.request.Request(upstream, body, {'Content-Type': 'application/json'})
                with urllib.request.urlopen(upstream_request, timeout=120) as response:
                    payload = response.read()
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *_args):
                pass

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f'http://127.0.0.1:{self.server.server_port}'

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

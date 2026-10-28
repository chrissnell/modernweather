"""Negative control for the conformance runner: forwards to the Go server but breaks the
contract in four specific, realistic ways. The runner is only trustworthy if it fails here."""
import http.client, http.server, json, re, sys

TARGET = ("127.0.0.1", 18080)

class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        host = self.headers.get("X-Fixture-Host") or self.headers.get("Host", "")
        c = http.client.HTTPConnection(*TARGET, timeout=60)
        c.request("GET", self.path, headers={"Host": host, "Accept-Encoding": "identity"})
        r = c.getresponse(); body = r.read()
        hdrs = [(k, v) for k, v in r.getheaders()
                if k.lower() not in ("content-length", "transfer-encoding", "connection")]
        ct = r.getheader("Content-Type") or ""
        if "application/json" in ct:
            text = body.decode()
            # 1. drop a required key
            text = re.sub(r'"ohum":[^,}]+,', '', text)
            # 2. write a decimal where Go writes an integer
            text = re.sub(r'"windd":(\d+)(?=[,}])', r'"windd":\1.0', text)
            # 3. change a type
            text = re.sub(r'"ts":(\d+)', r'"ts":"\1"', text)
            # 4. drop the trailing newline
            text = text.rstrip("\n")
            body = text.encode()
        self.send_response(r.status)
        for k, v in hdrs:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass

http.server.HTTPServer(("127.0.0.1", 18081), H).serve_forever()

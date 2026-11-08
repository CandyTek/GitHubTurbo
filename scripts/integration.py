"""Black-box transport tests: local sockets only, no GitHub/network dependency."""
import json
import pathlib
import socket
import socketserver
import subprocess
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]

def header(sock):
    data = b""
    while not data.endswith(b"\r\n\r\n"):
        chunk = sock.recv(1)
        if not chunk:
            raise AssertionError("unexpected EOF")
        data += chunk
    return data

class Upstream(socketserver.BaseRequestHandler):
    def handle(self):
        request = header(self.request)
        if b"api.github.com" in request:
            self.request.sendall(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
            return
        assert request.startswith(b"CONNECT github.com:443 HTTP/1.1\r\n")
        # Coalesced bytes after headers must reach the client, unchanged.
        self.request.sendall(b"HTTP/1.1 200 OK\r\n\r\nserver-prefix")
        while True:
            data = self.request.recv(65536)
            if not data:
                break
            self.request.sendall(data)

def request(port, line):
    conn = socket.create_connection(("127.0.0.1", port), timeout=3)
    conn.sendall(line + ("\r\nHost: localhost:%d\r\n\r\n" % port).encode())
    return conn, header(conn)

def main():
    with socketserver.ThreadingTCPServer(("127.0.0.1", 0), Upstream) as upstream, tempfile.TemporaryDirectory() as folder:
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        with socket.socket() as reserve:
            reserve.bind(("127.0.0.1", 0))
            port = reserve.getsockname()[1]
        config = json.loads((ROOT / "config.json").read_text())
        config.update(mode="proxy", host_modes={}, listen="127.0.0.1:%d" % port, upstream="127.0.0.1:%d" % upstream.server_address[1])
        config["transparent"]["enabled"] = False
        config_path = pathlib.Path(folder) / "config.json"
        config_path.write_text(json.dumps(config))
        process = subprocess.Popen([str(ROOT / "dist/github-turbo.exe"), "serve", str(config_path)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            for _ in range(100):
                try:
                    conn, response = request(port, b"GET /status HTTP/1.1")
                    conn.close()
                    assert b"200 OK" in response
                    break
                except OSError:
                    time.sleep(0.05)
            else:
                raise AssertionError("service did not start")
            for authority in [b"evilgithub.com:443", b"github.com.evil.test:443", b"127.0.0.1:443", b"github.com:22"]:
                conn, response = request(port, b"CONNECT " + authority + b" HTTP/1.1")
                conn.close()
                assert b"403 Forbidden" in response
            conn, response = request(port, b"CONNECT api.github.com:443 HTTP/1.1")
            conn.close()
            assert b"502 Bad Gateway" in response
            conn, response = request(port, b"CONNECT github.com:443 HTTP/1.1")
            assert b"200 Connection Established" in response
            payload = bytes(range(256)) * 2048
            def send():
                conn.sendall(payload)
                conn.shutdown(socket.SHUT_WR)
            sender = threading.Thread(target=send)
            sender.start()
            result = b""
            while True:
                chunk = conn.recv(65536)
                if not chunk:
                    break
                result += chunk
            sender.join()
            conn.close()
            assert result == b"server-prefix" + payload
            print("PASS: domain scope, upstream rejection, status, 512 KiB binary relay, preserved early bytes, half-close")
        finally:
            process.terminate()
            process.wait(timeout=5)
            upstream.shutdown()

if __name__ == "__main__":
    main()

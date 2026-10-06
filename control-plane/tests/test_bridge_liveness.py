"""Local control-plane integration tests; never connect to a deployed service.

Usage: python test_bridge_liveness.py /path/to/control-plane-binary
The raw WebSocket peer deliberately keeps TCP open without answering Ping,
reproducing a sleeping/disconnected Bridge behind a reverse proxy.
"""

import base64
import http.cookiejar
import json
import os
from pathlib import Path
import queue
import select
import socket
import struct
import subprocess
import sys
import threading
import time
import unittest
import urllib.error
import urllib.request


class WebSocketPeer:
    def __init__(self, port, path, headers=None):
        self.socket = socket.create_connection(("127.0.0.1", port), timeout=5)
        key = base64.b64encode(os.urandom(16)).decode()
        lines = [f"GET {path} HTTP/1.1", f"Host: 127.0.0.1:{port}",
                 "Upgrade: websocket", "Connection: Upgrade",
                 f"Sec-WebSocket-Key: {key}", "Sec-WebSocket-Version: 13"]
        lines.extend(f"{name}: {value}" for name, value in (headers or {}).items())
        self.socket.sendall(("\r\n".join(lines) + "\r\n\r\n").encode())
        response = b""
        while not response.endswith(b"\r\n\r\n"):
            response += self.read_exact(1)
        if b" 101 " not in response.split(b"\r\n")[0]:
            self.socket.close()
            raise AssertionError(response.decode())
        self.events = queue.Queue()
        self.stop_responder = threading.Event()
        self.thread = None

    def read_exact(self, size):
        data = b""
        while len(data) < size:
            chunk = self.socket.recv(size - len(data))
            if not chunk:
                raise EOFError("WebSocket closed")
            data += chunk
        return data

    def send_frame(self, opcode, payload):
        mask = os.urandom(4)
        size = len(payload)
        header = bytes([0x80 | opcode])
        if size < 126:
            header += bytes([0x80 | size])
        elif size <= 65535:
            header += b"\xfe" + struct.pack("!H", size)
        else:
            header += b"\xff" + struct.pack("!Q", size)
        masked = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
        self.socket.sendall(header + mask + masked)

    def receive(self):
        first, second = self.read_exact(2)
        size = second & 127
        if size == 126:
            size = struct.unpack("!H", self.read_exact(2))[0]
        elif size == 127:
            size = struct.unpack("!Q", self.read_exact(8))[0]
        return first & 15, self.read_exact(size)

    def receive_event(self):
        while True:
            opcode, payload = self.receive()
            if opcode == 9:
                self.send_frame(10, payload)
            elif opcode == 1:
                return json.loads(payload)
            elif opcode == 8:
                raise EOFError("WebSocket closed")

    def respond(self, ready=False, wrong_pong=False):
        def run():
            try:
                while not self.stop_responder.is_set():
                    if not select.select([self.socket], [], [], 0.1)[0]:
                        continue
                    opcode, payload = self.receive()
                    if opcode == 9:
                        self.send_frame(10, b"wrong" if wrong_pong else payload)
                    elif opcode == 1:
                        event = json.loads(payload)
                        self.events.put(event)
                        if ready and event["type"] == "session.start":
                            reply = {"type": "session.ready", "sessionId": event["sessionId"]}
                            self.send_frame(1, json.dumps(reply).encode())
                    elif opcode == 8:
                        return
            except (OSError, EOFError):
                return
        self.thread = threading.Thread(target=run, daemon=True)
        self.thread.start()
        return self

    def pause(self):
        self.stop_responder.set()
        if self.thread:
            self.thread.join(timeout=1)
            assert not self.thread.is_alive()

    def close(self):
        self.pause()
        self.socket.close()


class BridgeLivenessTest(unittest.TestCase):
    def setUp(self):
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        self.base = f"http://127.0.0.1:{self.port}"
        self.cookies = http.cookiejar.CookieJar()
        self.http = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.cookies))
        env = dict(os.environ, BIND_ADDR=f"127.0.0.1:{self.port}", DEMO_MODE="true",
                   BRIDGE_SHARED_SECRET="local-test-bridge-secret", CLIENT_ACCESS_TOKEN="",
                   OCTOS_SERVICE_TOKEN="", ALLOWED_ORIGIN="", SESSION_CAPACITY="1",
                   SESSION_TTL_SECONDS="60", RTC_TOKEN_TTL_SECONDS="1200",
                   BRIDGE_HEARTBEAT_TIMEOUT_SECONDS="2", SESSION_START_TIMEOUT_SECONDS="3")
        self.process = subprocess.Popen([BINARY], env=env, stdout=subprocess.DEVNULL,
                                        stderr=subprocess.DEVNULL)
        self.addCleanup(self.stop_server)
        deadline = time.monotonic() + 5
        while True:
            try:
                self.request("GET", "/api/v1/status")
                break
            except urllib.error.URLError:
                if time.monotonic() >= deadline or self.process.poll() is not None:
                    self.fail("Local control plane did not start")
                time.sleep(0.05)

    def stop_server(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()

    def request(self, method, path):
        req = urllib.request.Request(self.base + path, method=method,
                                     data=b"{}" if method == "POST" else None,
                                     headers={"Content-Type": "application/json"})
        try:
            response = self.http.open(req, timeout=2)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            body = response.read()
            return response.code, json.loads(body) if body.startswith(b"{") else body.decode()

    def wait_status(self, predicate, timeout=5):
        deadline = time.monotonic() + timeout
        while True:
            status = self.request("GET", "/api/v1/status")[1]
            if predicate(status):
                return status
            if time.monotonic() >= deadline:
                self.fail(f"Unexpected control-plane status: {status}")
            time.sleep(0.05)

    def bridge(self, responding=True, **kwargs):
        peer = WebSocketPeer(self.port, "/ws/bridge",
                             {"Authorization": "Bearer local-test-bridge-secret"})
        self.addCleanup(peer.close)
        if responding:
            peer.respond(**kwargs)
        self.wait_status(lambda status: status["bridgeOnline"])
        return peer

    def session(self):
        code, session = self.request("POST", "/api/v1/sessions")
        self.assertEqual(code, 201, session)
        return session

    def client(self, session):
        req = urllib.request.Request(self.base + session["eventsWsPath"])
        self.cookies.add_cookie_header(req)
        peer = WebSocketPeer(self.port, session["eventsWsPath"], {"Cookie": req.get_header("Cookie")})
        self.addCleanup(peer.close)
        return peer

    def test_offline_bridge_rejects_session(self):
        self.assertFalse(self.request("GET", "/api/v1/status")[1]["bridgeOnline"])
        self.assertEqual(self.request("GET", "/readyz")[0], 503)
        code, body = self.request("POST", "/api/v1/sessions")
        self.assertEqual(code, 503)
        self.assertEqual(body["error"]["code"], "bridge_offline")

    def test_open_tcp_without_pong_becomes_offline(self):
        self.bridge(responding=False)
        self.wait_status(lambda status: not status["bridgeOnline"])
        self.assertEqual(self.request("GET", "/readyz")[0], 503)
        self.assertEqual(self.request("POST", "/api/v1/sessions")[0], 503)

    def test_unmatched_pong_does_not_keep_bridge_online(self):
        self.bridge(wrong_pong=True)
        self.wait_status(lambda status: not status["bridgeOnline"])

    def test_live_bridge_stays_online_and_ready_snapshot_is_delivered(self):
        self.bridge(ready=True)
        session = self.session()
        # Wait before subscribing, so session.ready is deliberately missed.
        time.sleep(2.4)
        self.assertTrue(self.request("GET", "/api/v1/status")[1]["bridgeOnline"])
        self.assertEqual(self.request("GET", "/readyz")[0], 200)
        snapshot = self.client(session).receive_event()
        self.assertEqual(snapshot["type"], "session.snapshot")
        self.assertEqual(snapshot["state"], "ready")

    def test_silent_bridge_closes_existing_session_and_releases_capacity(self):
        bridge = self.bridge(ready=True)
        session = self.session()
        client = self.client(session)
        client.receive_event()  # snapshot
        bridge.pause()  # TCP remains open, but the Bridge no longer responds.
        error = client.receive_event()
        if error["type"] == "session.ready":
            error = client.receive_event()
        self.assertEqual(error["code"], "bridge_offline")
        self.assertEqual(client.receive_event()["type"], "session.closed")
        status = self.wait_status(lambda status: not status["bridgeOnline"])
        self.assertEqual(status["activeSessions"], 0)
        self.assertEqual(self.request("POST", "/api/v1/sessions")[0], 503)

    def test_session_start_timeout_stops_bridge_session_and_releases_capacity(self):
        bridge = self.bridge()
        session = self.session()
        client = self.client(session)
        self.assertEqual(client.receive_event()["state"], "starting")
        error = client.receive_event()
        self.assertEqual(error["code"], "session_start_timeout")
        self.assertEqual(client.receive_event()["type"], "session.closed")
        self.assertEqual(bridge.events.get(timeout=2)["type"], "session.start")
        self.assertEqual(bridge.events.get(timeout=2)["type"], "session.stop")
        status = self.request("GET", "/api/v1/status")[1]
        self.assertTrue(status["bridgeOnline"])
        self.assertEqual(status["activeSessions"], 0)
        self.session()  # A new session can use the released capacity.


if __name__ == "__main__":
    BINARY = str(Path(sys.argv.pop(1)).resolve())
    unittest.main(verbosity=2)

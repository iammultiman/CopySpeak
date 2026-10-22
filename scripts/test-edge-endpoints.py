#!/usr/bin/env python3
"""Offline tests for Edge endpoint ownership, captions and private error metadata."""

import asyncio
import importlib.util
import contextlib
import io
import json
import sys
import tempfile
import types
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

WRAPPER = Path(__file__).resolve().parent / "edge" / "copyspeak-edge.py"


def load_wrapper():
    spec = importlib.util.spec_from_file_location("copyspeak_edge", WRAPPER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


wrapper = load_wrapper()


class HandshakeDenied(Exception):
    """Stands in for aiohttp's WSServerHandshakeError."""

    def __init__(self, status=403):
        super().__init__(f"{status}, message='Invalid response status'")
        self.status = status


class FakeCommunicate:
    """Records the configured URL and replays a scripted outcome."""

    #: hosts that refuse the handshake, e.g. {"legacy"} to force the fallback
    refuse = set()
    attempts = []
    chunks = None
    failures = {}
    partial_hosts = set()

    def __init__(self, text, voice, boundary=None):
        self.boundary = boundary

    async def stream(self):
        communicate_module = sys.modules["edge_tts.communicate"]
        url = communicate_module.WSS_URL
        host = "fallback" if "msedgeservices" in url else "legacy"
        FakeCommunicate.attempts.append((host, url, self.boundary))
        if host in FakeCommunicate.partial_hosts:
            yield {"type": "audio", "data": b"discard-me"}
        if host in FakeCommunicate.failures:
            raise FakeCommunicate.failures[host]
        if host in FakeCommunicate.refuse:
            raise HandshakeDenied()
        for chunk in FakeCommunicate.chunks:
            yield chunk


def install_stub():
    """Install a fake client and verify the wrapper leaves its endpoint intact."""
    constants = types.ModuleType("edge_tts.constants")
    constants.TRUSTED_CLIENT_TOKEN = "6A5AA1D4EAFF4E9FB37E23D68491D6F4"
    constants.WSS_URL = (
        "wss://speech.platform.bing.com/consumer/speech/synthesize/readaloud/"
        f"edge/v1?TrustedClientToken={constants.TRUSTED_CLIENT_TOKEN}"
    )
    constants.WSS_HEADERS = {
        "Pragma": "no-cache",
        "Origin": "chrome-extension://jdiccldimpdaibmpdkjnbmckianbfold",
    }
    communicate = types.ModuleType("edge_tts.communicate")
    # Mirrors `from .constants import WSS_URL, WSS_HEADERS` — the bound copies.
    communicate.WSS_URL = constants.WSS_URL
    communicate.WSS_HEADERS = dict(constants.WSS_HEADERS)
    edge_tts = types.ModuleType("edge_tts")
    edge_tts.Communicate = FakeCommunicate
    edge_tts.communicate = communicate
    edge_tts.constants = constants
    sys.modules["edge_tts"] = edge_tts
    sys.modules["edge_tts.communicate"] = communicate
    sys.modules["edge_tts.constants"] = constants
    return edge_tts, communicate


WORDS = [
    {"type": "audio", "data": b"\x01\x02"},
    {"type": "WordBoundary", "offset": 10_000, "duration": 5_000, "text": "Hello"},
    {"type": "audio", "data": b"\x03\x04"},
    {"type": "WordBoundary", "offset": 30_000, "duration": 5_000, "text": "world"},
]


def error_payload(stderr_text):
    """The marker payload, which deliberately starts its own line."""
    marker = [
        line for line in stderr_text.splitlines() if line.startswith("[edge-error] ")
    ]
    assert len(marker) == 1, stderr_text
    return json.loads(marker[0].removeprefix("[edge-error] "))


class EdgeEndpointTests(unittest.TestCase):
    def test_rate_limit_reports_only_safe_metadata(self):
        error = HandshakeDenied(429)
        error.headers = {"Retry-After": "17"}
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            wrapper.report_error(error)
        line = stderr.getvalue().strip()
        self.assertEqual(error_payload(line), {"kind": "http", "status": 429, "retry_after_seconds": 17})
        self.assertNotIn("Invalid response", line)

    def setUp(self):
        self.enterContext(patch.dict(sys.modules))
        self.edge_tts, self.communicate = install_stub()
        FakeCommunicate.refuse = set()
        FakeCommunicate.attempts = []
        FakeCommunicate.chunks = list(WORDS)
        FakeCommunicate.failures = {}
        FakeCommunicate.partial_hosts = set()
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.input_path = str(Path(self.tmp.name) / "in.txt")
        self.output_path = str(Path(self.tmp.name) / "out.mp3")
        Path(self.input_path).write_text("Hello world.", encoding="utf-8")

    def run_synthesize(self):
        asyncio.run(wrapper.synthesize(self.input_path, self.output_path, "en-US-AvaNeural"))

    def test_upstream_endpoint_and_word_boundaries_are_preserved(self):
        endpoint = self.communicate.WSS_URL
        self.run_synthesize()
        self.assertEqual([a[0] for a in FakeCommunicate.attempts], ["legacy"])
        host, url, boundary = FakeCommunicate.attempts[0]
        self.assertEqual(url, endpoint)
        self.assertEqual(boundary, "WordBoundary")
        self.assertNotIn("Sec-WebSocket-Protocol", self.communicate.WSS_HEADERS)

    def test_wrapper_does_not_override_client_selected_endpoint(self):
        self.communicate.WSS_URL = "wss://client-selected.example/speech?version=2"
        self.run_synthesize()
        self.assertEqual(FakeCommunicate.attempts[0][1], self.communicate.WSS_URL)
        self.assertEqual(self.communicate.WSS_HEADERS, self.edge_tts.constants.WSS_HEADERS)

    def test_audio_and_captions_come_from_the_same_attempt(self):
        self.run_synthesize()
        self.assertEqual(Path(self.output_path).read_bytes(), b"\x01\x02\x03\x04")
        captions = json.loads(Path(self.output_path + ".captions.json").read_text(encoding="utf-8"))
        self.assertEqual(captions["text"], "Hello world.")
        self.assertEqual([w["text_start"] for w in captions["words"]][:2], [0, 6])
        # Offsets are ticks/10_000, so 10_000 ticks is 1 ms.
        self.assertEqual(captions["words"][0]["start_ms"], 1.0)
        self.assertEqual(captions["words"][0]["end_ms"], 1.5)

    def test_forbidden_is_not_retried_again_by_the_wrapper(self):
        FakeCommunicate.refuse = {"legacy"}
        stderr = io.StringIO()
        # Drive the production entry point: `synthesize` alone writes no
        # stderr, so asserting on it proved nothing about what Rust captures.
        with contextlib.redirect_stderr(stderr):
            code = wrapper.main([self.input_path, self.output_path, "en-US-AvaNeural"])
        self.assertEqual(code, 1)
        self.assertEqual([a[0] for a in FakeCommunicate.attempts], ["legacy"])
        self.assertFalse(Path(self.output_path).exists())
        line = stderr.getvalue().strip()
        self.assertEqual(error_payload(line), {"kind": "http", "status": 403})
        self.assertNotIn("Invalid response", line)

    def test_non_403_stream_failure_is_not_retried(self):
        for status in (401, 429, 500):
            with self.subTest(status=status):
                FakeCommunicate.attempts = []
                FakeCommunicate.failures = {"legacy": HandshakeDenied(status)}
                with self.assertRaises(HandshakeDenied):
                    self.run_synthesize()
                self.assertEqual([attempt[0] for attempt in FakeCommunicate.attempts], ["legacy"])

    def test_partial_failed_attempt_is_discarded(self):
        FakeCommunicate.partial_hosts = {"legacy"}
        FakeCommunicate.refuse = {"legacy"}
        with self.assertRaises(HandshakeDenied):
            self.run_synthesize()
        self.assertFalse(Path(self.output_path).exists())
        self.assertFalse(Path(self.output_path + ".captions.json").exists())

    def test_exception_context_and_arbitrary_messages_are_not_reported(self):
        error = RuntimeError("private diagnostic and request parameters")
        error.__context__ = HandshakeDenied(403)
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            wrapper.report_error(error)
        self.assertEqual(error_payload(stderr.getvalue()), {"kind": "failure"})
        self.assertNotIn("private diagnostic", stderr.getvalue())

    def test_unrelated_failure_is_not_retried(self):
        class Other(Exception):
            pass

        def explode(self, text, voice, boundary=None):
            raise Other("no audio received")

        original = FakeCommunicate.__init__
        FakeCommunicate.__init__ = explode
        self.addCleanup(lambda: setattr(FakeCommunicate, "__init__", original))
        with self.assertRaises(Other):
            self.run_synthesize()
        self.assertEqual(FakeCommunicate.attempts, [])

    def test_retry_after_dates_and_invalid_values(self):
        now = datetime(2026, 1, 1, tzinfo=timezone.utc)
        self.assertEqual(wrapper.retry_after_seconds("Thu, 01 Jan 2026 00:00:20 GMT", now), 20)
        self.assertEqual(wrapper.retry_after_seconds("Wed, 31 Dec 2025 23:59:59 GMT", now), 0)
        for value in (None, "invalid", "-1", "1.5", "inf"):
            self.assertIsNone(wrapper.retry_after_seconds(value, now))

    def test_transport_failures_are_classified_by_family(self):
        class ClientConnectionError(Exception):
            pass

        class ClientConnectorDNSError(ClientConnectionError):
            pass

        class ServerDisconnectedError(Exception):
            pass

        for error in (ClientConnectionError(), ClientConnectorDNSError(), ServerDisconnectedError()):
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                wrapper.report_error(error)
            self.assertEqual(error_payload(stderr.getvalue()), {"kind": "connection"})

    def test_marker_starts_a_line_even_after_a_partial_line(self):
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            stderr.write("partial")
            wrapper.report_error(HandshakeDenied(403))
        lines = stderr.getvalue().splitlines()
        self.assertEqual(lines[0], "partial")
        self.assertTrue(lines[-1].startswith("[edge-error] "))

    def test_final_status_wins_over_exception_context(self):
        error = HandshakeDenied(429)
        error.__context__ = HandshakeDenied(403)
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            wrapper.report_error(error)
        self.assertEqual(error_payload(stderr.getvalue()), {"kind": "http", "status": 429})


if __name__ == "__main__":
    unittest.main(verbosity=2)

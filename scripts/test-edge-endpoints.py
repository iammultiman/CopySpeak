#!/usr/bin/env python3
"""Endpoint-fallback tests for the Edge Read Aloud wrapper.

The real `edge-tts` cannot be called from CI (it needs Microsoft's service and
a region that still accepts the handshake), so these tests drive
`copyspeak-edge.py` against a stub that records the URL it was asked to open and
can be told to refuse it. That covers what the wrapper is responsible for:

* which host each attempt targets,
* the query parameter each host expects,
* that a refusal on one host retries the other,
* that a non-handshake failure is not retried,
* that captions still come from the successful attempt's word boundaries.

Run: python scripts/test-edge-endpoints.py
"""

import asyncio
import importlib.util
import contextlib
import io
import json
import sys
import tempfile
import types
import unittest
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
    """Install a fake `edge_tts` package whose communicate module carries the
    7.2.8 import-bound globals the wrapper patches."""
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


class EdgeEndpointTests(unittest.TestCase):
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

    def test_legacy_host_is_used_first_and_needs_no_retry(self):
        self.run_synthesize()
        self.assertEqual([a[0] for a in FakeCommunicate.attempts], ["legacy"])
        host, url, boundary = FakeCommunicate.attempts[0]
        # The legacy URL is the pinned one plus the query key it expects.
        self.assertIn("speech.platform.bing.com", url)
        self.assertIn("TrustedClientToken=", url)
        self.assertIn("?", url)
        self.assertEqual(boundary, "WordBoundary")
        # No subprotocol is added to the legacy attempt (7.2.8 sends none).
        self.assertNotIn("Sec-WebSocket-Protocol", self.communicate.WSS_HEADERS)

    def test_refused_legacy_host_falls_back_to_current_host(self):
        FakeCommunicate.refuse = {"legacy"}
        self.run_synthesize()
        self.assertEqual([a[0] for a in FakeCommunicate.attempts], ["legacy", "fallback"])
        _, url, _ = FakeCommunicate.attempts[1]
        self.assertIn("api.msedgeservices.com", url)
        self.assertIn("Ocp-Apim-Subscription-Key=", url)
        # Both hosts carry the same trusted-client token value.
        self.assertIn("6A5AA1D4EAFF4E9FB37E23D68491D6F4", url)
        self.assertEqual(self.communicate.WSS_HEADERS, self.edge_tts.constants.WSS_HEADERS)

    def test_fallback_audio_and_captions_come_from_the_successful_attempt(self):
        FakeCommunicate.refuse = {"legacy"}
        self.run_synthesize()
        self.assertEqual(Path(self.output_path).read_bytes(), b"\x01\x02\x03\x04")
        captions = json.loads(Path(self.output_path + ".captions.json").read_text(encoding="utf-8"))
        self.assertEqual(captions["text"], "Hello world.")
        self.assertEqual([w["text_start"] for w in captions["words"]][:2], [0, 6])
        # Offsets are ticks/10_000, so 10_000 ticks is 1 ms.
        self.assertEqual(captions["words"][0]["start_ms"], 1.0)
        self.assertEqual(captions["words"][0]["end_ms"], 1.5)

    def test_both_hosts_refused_surfaces_the_error(self):
        FakeCommunicate.refuse = {"legacy", "fallback"}
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), self.assertRaises(HandshakeDenied):
            self.run_synthesize()
        self.assertEqual([a[0] for a in FakeCommunicate.attempts], ["legacy", "fallback"])
        self.assertIn("[edge] both-endpoints-denied: HTTP 403", stderr.getvalue())
        self.assertFalse(Path(self.output_path).exists())

    def test_non_403_stream_failure_is_not_retried(self):
        for status in (401, 500):
            with self.subTest(status=status):
                FakeCommunicate.attempts = []
                FakeCommunicate.failures = {"legacy": HandshakeDenied(status)}
                with self.assertRaises(HandshakeDenied):
                    self.run_synthesize()
                self.assertEqual([attempt[0] for attempt in FakeCommunicate.attempts], ["legacy"])

    def test_partial_failed_attempt_is_discarded(self):
        FakeCommunicate.partial_hosts = {"legacy"}
        FakeCommunicate.refuse = {"legacy"}
        self.run_synthesize()
        self.assertEqual(Path(self.output_path).read_bytes(), b"\x01\x02\x03\x04")

    def test_fallback_server_error_is_not_reported_as_two_403s(self):
        FakeCommunicate.refuse = {"legacy"}
        FakeCommunicate.failures = {"fallback": HandshakeDenied(500)}
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), self.assertRaises(HandshakeDenied):
            self.run_synthesize()
        self.assertNotIn("both-endpoints-denied", stderr.getvalue())
        self.assertFalse(Path(self.output_path).exists())

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

    def test_handshake_denied_matches_the_real_error_shapes(self):
        self.assertTrue(wrapper.handshake_denied(HandshakeDenied()))
        self.assertFalse(wrapper.handshake_denied(HandshakeDenied(401)))
        self.assertFalse(wrapper.handshake_denied(HandshakeDenied(500)))
        self.assertFalse(wrapper.handshake_denied(Exception("403, message='Invalid response status'")))
        self.assertFalse(wrapper.handshake_denied(Exception("Invalid response status")))
        self.assertFalse(wrapper.handshake_denied(Exception("No audio was received")))
        self.assertFalse(wrapper.handshake_denied(Exception("timeout")))


if __name__ == "__main__":
    unittest.main(verbosity=2)

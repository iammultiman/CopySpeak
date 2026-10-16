"""Edge audio and native word boundaries from the same request (no SRT rounding)."""
import asyncio
import json
import sys
import re
from pathlib import Path

# Microsoft has been moving Edge Read Aloud to a newer host. The pinned client
# (7.2.8) still targets the legacy one, which regional clusters began refusing
# with a 403 handshake on 2026-10-10. The newer host accepts the same legacy
# token, so it is used as a fallback — never as a replacement, because the
# legacy host still works in regions Microsoft has not migrated, and moving
# those clients off it would be a regression. Release 7.2.2 pointed here with
# `Ocp-Apim-Subscription-Key`; the trusted-client token value is unchanged.
LEGACY_HOST = "speech.platform.bing.com/consumer/speech/synthesize/readaloud/edge/v1"
FALLBACK_HOST = "api.msedgeservices.com/tts/cognitiveservices/websocket/v1"


def configure_endpoint(edge_communicate, host, query_key, token):
    """Point the client at one Read Aloud host.

    `communicate.py` binds `WSS_URL` and `WSS_HEADERS` with
    `from .constants import ...`, so the copies on `edge_tts.communicate` are what
    the handshake reads — patching `edge_tts.constants` would have no effect.
    Both are looked up when the socket is opened, not at import, so assigning
    them here applies to this request. The `Sec-MS-GEC` value is generated from
    the trusted-client token at call time and is not host-specific, so it is left
    alone.

    The URL must carry a query separator: the client appends
    `&ConnectionId=...&Sec-MS-GEC=...&Sec-MS-GEC-Version=...` unconditionally.

    Headers and the token generator are unchanged on both hosts.
    """
    edge_communicate.WSS_URL = f"wss://{host}?{query_key}={token}"


def handshake_denied(error):
    """Retry only an HTTP 403; unrelated handshake failures remain errors."""
    return getattr(error, "status", None) == 403


def preserve_source_text(text, spoken, words):
    """Keep original punctuation when every returned word maps exactly in order.

    If normalization changed the words, display the provider's spoken text;
    never pretend its offsets refer to the original spelling.
    """
    cursor = 0
    mapped = []
    encoded = spoken.encode("utf-16-le")
    for word in words:
        token = encoded[word["text_start"] * 2:word["text_end"] * 2].decode("utf-16-le")
        match = re.search(re.escape(token), text[cursor:], re.IGNORECASE)
        if match is None or any(c.isalnum() for c in text[cursor:cursor + match.start()]):
            return {"text": spoken, "words": words}
        start, end = cursor + match.start(), cursor + match.end()
        mapped.append({**word, "text_start": len(text[:start].encode("utf-16-le")) // 2,
                       "text_end": len(text[:end].encode("utf-16-le")) // 2})
        cursor = end
    if any(c.isalnum() for c in text[cursor:]):
        return {"text": spoken, "words": words}
    return {"text": text, "words": mapped}


async def synthesize(input_path, output_path, voice):
    import edge_tts
    import edge_tts.communicate as communicate_module
    from edge_tts.constants import TRUSTED_CLIENT_TOKEN

    text = Path(input_path).read_text(encoding="utf-8")

    async def speak():
        """One attempt against whichever host is currently configured.

        Returns the audio bytes and word boundaries so a denied handshake can be
        retried on the other host without re-reading the input.
        """
        words = []
        spoken = ""
        audio = bytearray()
        # The CLI defaults to SentenceBoundary; explicitly request individual words.
        communicate = edge_tts.Communicate(text, voice, boundary="WordBoundary")
        async for chunk in communicate.stream():
            if chunk["type"] == "audio":
                audio.extend(chunk["data"])
            elif chunk["type"] == "WordBoundary":
                if spoken:
                    spoken += " "
                start = len(spoken.encode("utf-16-le")) // 2
                spoken += chunk["text"]
                words.append({
                    "text_start": start,
                    "text_end": len(spoken.encode("utf-16-le")) // 2,
                    "start_ms": chunk["offset"] / 10000,
                    "end_ms": (chunk["offset"] + chunk["duration"]) / 10000,
                })
        return bytes(audio), spoken, words

    # Try the host the pinned client ships with first, and fall back only on a
    # refused handshake. With `receive_timeout` set, edge-tts reconnects once on
    # 403 by itself; this adds the host change it cannot know about.
    configure_endpoint(
        communicate_module, LEGACY_HOST, "TrustedClientToken", TRUSTED_CLIENT_TOKEN
    )
    try:
        audio, spoken, words = await speak()
    except Exception as error:
        if not handshake_denied(error):
            raise
        print(
            "[edge] Read Aloud refused the legacy host; retrying on the current one",
            file=sys.stderr,
            flush=True,
        )
        configure_endpoint(
            communicate_module, FALLBACK_HOST, "Ocp-Apim-Subscription-Key", TRUSTED_CLIENT_TOKEN
        )
        try:
            audio, spoken, words = await speak()
        except Exception as fallback_error:
            if handshake_denied(fallback_error):
                print("[edge] both-endpoints-denied: HTTP 403", file=sys.stderr, flush=True)
            raise

    with open(output_path, "wb") as out:
        out.write(audio)
    Path(output_path + ".captions.json").write_text(
        json.dumps(preserve_source_text(text, spoken, words)), encoding="utf-8"
    )


if __name__ == "__main__":
    asyncio.run(synthesize(*sys.argv[1:]))

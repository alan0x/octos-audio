"""Streaming Qwen3-TTS client and PCM resampling helpers.

OminiX streams raw 16-bit mono PCM at 24 kHz from /v1/audio/speech. Agora does
not accept a 24 kHz PCM feed, so frames are upsampled 2x to 48 kHz before being
pushed into the RTC connection.
"""

import array
import logging
import math
import re
from typing import AsyncIterator, List, Optional, Tuple

import httpx

logger = logging.getLogger("bridge.tts")

TTS_SOURCE_RATE = 24_000
TTS_TARGET_RATE = 48_000

# Half-width punctuation mapping
_CJK_TERMINATORS = str.maketrans({"?": "？", "!": "！"})
_CJK_COMMAS_TO_ASCII = str.maketrans({"，": ",", "；": ";", "、": ","})
_ASCII_TO_CJK_FULL = str.maketrans({",": "，", ";": "；", "?": "？", "!": "！"})


def _has_cjk(text: str) -> bool:
    return any("一" <= char <= "鿿" for char in text)


def normalize_tts_text(text: str, avoid_comma_split: bool = True) -> str:
    """Normalize punctuation for Qwen3-TTS.

    If avoid_comma_split is True (default):
    - Multi-line paragraphs are collapsed into a continuous stream to prevent
      OminiX from chopping paragraphs into isolated generations with jarring prosody.
    - Clause separators (，, ；, 、) are mapped to half-width (, / ;) so OminiX
      does not chop clauses into micro-fragments that suffer duration stalling.
    - Sentence terminators (？, ！) are kept as CJK punctuation.

    If avoid_comma_split is False, legacy normalization to full-width is used.
    """
    if not _has_cjk(text):
        return text

    # Collapse multiple lines into a single coherent text flow
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    if lines:
        text = " ".join(lines)

    if avoid_comma_split:
        text = text.translate(_CJK_TERMINATORS)
        return text.translate(_CJK_COMMAS_TO_ASCII)
    return text.translate(_ASCII_TO_CJK_FULL)


# Qwen3-TTS tends to fall into silence and stop early on long segments or after
# pause-like punctuation, dropping the rest of the text. The bridge therefore
# sends short clause-aligned chunks one request at a time, validates each one
# and retries chunks that look truncated.
DEFAULT_MAX_CHUNK_CHARS = 40
DEFAULT_MAX_ATTEMPTS = 3

_SENTENCE_BREAKS = "。！？；：…!?;"
_CLAUSE_BREAKS = "，、,"
_DASHES = re.compile(r"\s*(?:—+|–+|-{2,})\s*")
_PERCENT = re.compile(r"(\d+(?:\.\d+)?)\s*[%％]")

SILENCE_RMS = 200
SILENCE_WINDOW_MS = 20
MAX_TRAILING_SILENCE_MS = 1_000
KEEP_TRAILING_SILENCE_MS = 300

# Each chunk is generated on its own and ends with almost no pause, so the
# pause a single generation would have produced is restored per boundary
# (the next chunk adds ~80 ms of leading silence).
SENTENCE_PAUSE_MS = 400
SEMICOLON_PAUSE_MS = 300
CLAUSE_PAUSE_MS = 150
MAX_UNITS_PER_VOICED_SECOND = 8.5


def _split_after(text: str, breaks: str) -> List[str]:
    """Split text after each run of break characters, keeping the breaks.

    An ASCII comma between digits ("1,000") is not treated as a break.
    """
    parts: List[str] = []
    current = ""
    for index, char in enumerate(text):
        current += char
        nxt = text[index + 1] if index + 1 < len(text) else ""
        if char not in breaks or nxt in breaks:
            continue
        if char == "," and index > 0 and text[index - 1].isdigit() and nxt.isdigit():
            continue
        parts.append(current)
        current = ""
    if current:
        parts.append(current)
    return [part for part in parts if part.strip()]


def _finish_chunk(chunk: str) -> str:
    chunk = chunk.strip()
    # A chunk cut at a clause boundary ends with "," so the model keeps a
    # continuing intonation instead of a sentence-final fall.
    continues = chunk[-1:] in _CLAUSE_BREAKS
    # Inner clause marks become half-width without trailing space so OminiX
    # does not re-split the chunk into micro-fragments.
    chunk = re.sub(r"[，、]", ",", chunk)
    chunk = re.sub(r",\s+", ",", chunk)
    chunk = chunk.rstrip(",;；:：")
    if not chunk:
        return ""
    if continues:
        chunk += ","
    elif chunk[-1] not in "。！？…!?.":
        chunk += "。"
    return chunk.translate(_CJK_TERMINATORS)


def _pause_after_ms(chunk: str) -> int:
    end = chunk.rstrip()[-1:]
    if end and end in _CLAUSE_BREAKS:
        return CLAUSE_PAUSE_MS
    if end and end in "；;：":
        return SEMICOLON_PAUSE_MS
    return SENTENCE_PAUSE_MS


def plan_tts_chunks(
    text: str, max_chars: int = DEFAULT_MAX_CHUNK_CHARS
) -> List[Tuple[str, int]]:
    """Split Chinese text into short, clause-aligned chunks for Qwen3-TTS.

    Sentences end at 。！？；：… (and line breaks); long sentences are packed
    clause by clause up to ``max_chars``. Dashes become clause breaks and
    percentages are spelled out, since OminiX strips ``%``. Non-Chinese text
    is returned as a single chunk. Each chunk comes with the pause in ms that
    should follow it.
    """
    text = text.strip()
    if not text:
        return []
    if not _has_cjk(text):
        return [(text, SENTENCE_PAUSE_MS)]

    text = _PERCENT.sub(r"百分之\1", text)
    text = _DASHES.sub("，", text)

    chunks: List[str] = []
    for line in text.splitlines():
        for sentence in _split_after(line.strip(), _SENTENCE_BREAKS):
            current = ""
            for clause in _split_after(sentence, _CLAUSE_BREAKS):
                if current and len(current) + len(clause) > max_chars:
                    chunks.append(current)
                    current = ""
                current += clause
            if current:
                chunks.append(current)
    planned = [(_finish_chunk(chunk), _pause_after_ms(chunk)) for chunk in chunks]
    return [(chunk, pause) for chunk, pause in planned if chunk]


def split_tts_chunks(text: str, max_chars: int = DEFAULT_MAX_CHUNK_CHARS) -> List[str]:
    return [chunk for chunk, _ in plan_tts_chunks(text, max_chars)]


def expected_speech_units(text: str) -> float:
    """Rough count of spoken syllables: CJK chars, digits and English words."""
    cjk = sum(1 for char in text if _has_cjk(char))
    digits = sum(1 for char in text if char.isdigit())
    words = len(re.findall(r"[A-Za-z]+", text))
    return cjk + digits + 1.5 * words


def voiced_end_ms(pcm: bytes, sample_rate: int = TTS_SOURCE_RATE) -> int:
    """Return the end of the last non-silent 20 ms window of 16-bit mono PCM."""
    samples = array.array("h", pcm[: len(pcm) - len(pcm) % 2])
    window = sample_rate * SILENCE_WINDOW_MS // 1000
    for start in range((len(samples) - 1) // window * window, -1, -window):
        frame = samples[start : start + window]
        if frame and math.sqrt(sum(s * s for s in frame) / len(frame)) > SILENCE_RMS:
            return (start + len(frame)) * 1000 // sample_rate
    return 0


def pcm_duration_ms(pcm: bytes, sample_rate: int = TTS_SOURCE_RATE) -> int:
    return len(pcm) // 2 * 1000 // sample_rate


def truncation_reason(text: str, pcm: bytes, speed: float = 1.0) -> Optional[str]:
    """Return why ``pcm`` looks like an incomplete rendering of ``text``.

    The model's failure mode is to stop speaking mid-text, emit seconds of
    silence and then end, so a long silent tail or an implausibly fast
    speaking rate indicates dropped text.
    """
    voiced = voiced_end_ms(pcm)
    if voiced == 0:
        return "no voiced audio"
    trailing = pcm_duration_ms(pcm) - voiced
    if trailing > MAX_TRAILING_SILENCE_MS:
        return f"trailing silence {trailing} ms"
    rate = expected_speech_units(text) / (voiced / 1000)
    if rate > MAX_UNITS_PER_VOICED_SECOND * max(speed, 1.0):
        return f"speech rate {rate:.1f} units/s"
    return None


def fit_trailing_silence(
    pcm: bytes, pause_ms: int = KEEP_TRAILING_SILENCE_MS, pad: bool = False
) -> bytes:
    """Cut the silent tail to ``pause_ms`` after speech; ``pad`` extends it."""
    target = (voiced_end_ms(pcm) + pause_ms) * TTS_SOURCE_RATE // 1000 * 2
    fitted = pcm[:target]
    if pad:
        fitted += b"\x00" * (target - len(fitted))
    return fitted


class PcmUpsampler2x:
    """16-bit mono PCM 2x linear-interpolation upsampler.

    Keeps a one-sample carry so chunk boundaries stay continuous; an odd
    trailing byte is carried over to the next feed() call.
    """

    def __init__(self) -> None:
        self._pending = bytearray()
        self._last_sample: Optional[int] = None

    def feed(self, pcm: bytes) -> bytes:
        data = bytes(self._pending) + pcm
        usable = len(data) - (len(data) % 2)
        self._pending = bytearray(data[usable:])
        out = bytearray()
        previous = self._last_sample
        for offset in range(0, usable, 2):
            sample = int.from_bytes(data[offset : offset + 2], "little", signed=True)
            if previous is not None:
                midpoint = (previous + sample) // 2
                out += midpoint.to_bytes(2, "little", signed=True)
            out += sample.to_bytes(2, "little", signed=True)
            previous = sample
        self._last_sample = previous
        return bytes(out)


class TtsClient:
    def __init__(
        self,
        url: str,
        voice: str = "serena",
        speed: float = 1.0,
        instruct: str = "",
        language: str = "chinese",
        temperature: float = 0.2,
        top_p: float = 0.8,
        seed: Optional[int] = 42,
        avoid_comma_split: bool = True,
        max_chunk_chars: int = DEFAULT_MAX_CHUNK_CHARS,
        max_attempts: int = DEFAULT_MAX_ATTEMPTS,
        timeout_seconds: float = 120.0,
    ) -> None:
        self.url = url
        self.voice = voice
        self.speed = speed
        self.instruct = instruct
        self.language = language
        self.temperature = temperature
        self.top_p = top_p
        self.seed = seed
        self.avoid_comma_split = avoid_comma_split
        self.max_chunk_chars = max_chunk_chars
        self.max_attempts = max(1, max_attempts)
        self._client = httpx.AsyncClient(timeout=timeout_seconds)

    def _chunks(self, text: str) -> List[Tuple[str, int]]:
        if not self.avoid_comma_split:
            # Legacy path: one request, OminiX splits at every full-width mark.
            return [(normalize_tts_text(text, avoid_comma_split=False), 0)]
        return plan_tts_chunks(text, self.max_chunk_chars)

    async def stream_pcm(
        self,
        text: str,
        voice: Optional[str] = None,
        speed: Optional[float] = None,
        instruct: Optional[str] = None,
    ) -> AsyncIterator[bytes]:
        """Yield 24 kHz PCM, one validated chunk of text at a time."""
        selected_voice = (voice or "").strip() or self.voice
        selected_instruct = (instruct or "").strip() or self.instruct
        selected_speed = speed if speed is not None else self.speed
        chunks = self._chunks(text)
        logger.info(
            "TTS request: url=%s voice=%s chars=%d chunks=%d",
            self.url,
            selected_voice,
            len(text),
            len(chunks),
        )
        for index, (chunk, pause_ms) in enumerate(chunks):
            pcm = await self._synthesize_chunk(
                chunk, selected_voice, selected_speed, selected_instruct, index
            )
            if not pcm:
                continue
            if index < len(chunks) - 1:
                yield fit_trailing_silence(pcm, pause_ms, pad=True)
            else:
                yield fit_trailing_silence(pcm)

    async def _synthesize_chunk(
        self, text: str, voice: str, speed: float, instruct: str, index: int
    ) -> bytes:
        best: Tuple[int, bytes] = (-1, b"")
        for attempt in range(self.max_attempts):
            pcm = await self._request(text, voice, speed, instruct, attempt)
            reason = truncation_reason(text, pcm, speed)
            if reason is None:
                return pcm
            logger.warning(
                "TTS chunk %d attempt %d/%d looks truncated (%s): %r",
                index,
                attempt + 1,
                self.max_attempts,
                reason,
                text,
            )
            voiced = voiced_end_ms(pcm)
            if voiced > best[0]:
                best = (voiced, pcm)
        return best[1]

    async def _request(
        self, text: str, voice: str, speed: float, instruct: str, attempt: int
    ) -> bytes:
        body = {
            "input": text,
            "model": "qwen3-tts",
            "response_format": "pcm",
            "language": self.language,
        }
        if voice:
            body["voice"] = voice
        if instruct:
            body["instruct"] = instruct
        if speed != 1.0:
            body["speed"] = speed
        if self.temperature is not None:
            body["temperature"] = self.temperature
        if self.top_p is not None:
            body["top_p"] = self.top_p
        if self.seed is not None:
            # A retry must not replay the same sample path once the server
            # honours the seed.
            body["seed"] = self.seed + attempt
        logger.info(
            "TTS chunk request: attempt=%d chars=%d text=%r",
            attempt + 1,
            len(text),
            text[:50],
        )
        pcm = bytearray()
        async with self._client.stream("POST", self.url, json=body) as response:
            response.raise_for_status()
            async for data in response.aiter_bytes():
                pcm.extend(data)
        return bytes(pcm)

    async def close(self) -> None:
        await self._client.aclose()

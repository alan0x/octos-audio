"""Outbound LAN bridge process."""

import argparse
import asyncio
import json
import logging
import os
import random
import signal
import time
from typing import Any, Awaitable, Callable, Dict, List, Optional, Tuple

import websockets

from .agora_receiver import AgoraReceiver
from .livekit_receiver import LivekitReceiver
from .segmenter import PcmSegmenter, SegmentEvent, SegmenterConfig
from .sensevoice import SenseVoiceClient
from .tts import TTS_SOURCE_RATE, TTS_TARGET_RATE, PcmUpsampler2x, TtsClient

logger = logging.getLogger(__name__)


def env_float(name: str, default: float) -> float:
    return float(os.getenv(name, str(default)))


def parse_asr_urls(urls_value: str, fallback_url: str = "") -> List[str]:
    """Parse the comma-separated ASR_URLS pool, falling back to a single URL."""
    urls = [url.strip() for url in urls_value.split(",") if url.strip()]
    if not urls and fallback_url.strip():
        urls = [fallback_url.strip()]
    return urls


class RealSession:
    def __init__(
        self,
        session_id: str,
        agora: Dict[str, Any],
        asr_url: str,
        emit: Callable[[Dict[str, Any]], Awaitable[None]],
        tts_url: Optional[str] = None,
        provider: str = "agora",
    ) -> None:
        self.session_id = session_id
        self.emit = emit
        self.queue: asyncio.Queue[Optional[Tuple[bytes, int]]] = asyncio.Queue(maxsize=500)
        self.segmenter = PcmSegmenter(
            SegmenterConfig(threshold_dbfs=env_float("VAD_THRESHOLD_DBFS", -38.0))
        )
        self.asr = SenseVoiceClient(
            url=asr_url,
            model=os.getenv("ASR_MODEL", os.getenv("SENSEVOICE_MODEL", "qwen3-asr")),
            api_key=os.getenv("ASR_API_KEY", os.getenv("SENSEVOICE_API_KEY", "")),
            language=os.getenv(
                "ASR_LANGUAGE", os.getenv("SENSEVOICE_LANGUAGE", "Chinese")
            ),
            protocol=os.getenv(
                "ASR_PROTOCOL", os.getenv("SENSEVOICE_PROTOCOL", "octos-json")
            ),
        )
        if provider == "livekit":
            self.receiver = LivekitReceiver(
                url=agora["url"],
                room=agora["room"],
                token=agora["token"],
                on_pcm=self._on_pcm,
                on_network_stats=self._on_network_stats,
            )
        else:
            self.receiver = AgoraReceiver(
                appid=agora["appId"],
                channel=agora["channel"],
                token=agora["token"],
                uid=int(agora["uid"]),
                on_pcm=self._on_pcm,
                on_network_stats=self._on_network_stats,
            )
        self.worker: Optional[asyncio.Task] = None
        self.partial_task: Optional[asyncio.Task] = None
        self.sequence = 0
        self.network_stats: Dict[str, Any] = {}
        self.tts = (
            TtsClient(
                url=tts_url,
                voice=os.getenv("TTS_VOICE", "serena"),
                speed=float(os.getenv("TTS_SPEED", "1.0")),
                instruct=os.getenv("TTS_INSTRUCT", ""),
                language=os.getenv("TTS_LANGUAGE", "chinese"),
                temperature=float(os.getenv("TTS_TEMPERATURE", "0.2")),
                top_p=float(os.getenv("TTS_TOP_P", "0.8")),
                seed=int(os.getenv("TTS_SEED", "42")) if os.getenv("TTS_SEED", "42").strip() else None,
                avoid_comma_split=os.getenv("TTS_AVOID_COMMA_SPLIT", "true").lower() not in ("0", "false", "no"),
                max_chunk_chars=int(os.getenv("TTS_MAX_CHUNK_CHARS", "40")),
                max_attempts=int(os.getenv("TTS_MAX_ATTEMPTS", "3")),
            )
            if tts_url
            else None
        )
        self.tts_task: Optional[asyncio.Task] = None

    async def start(self) -> None:
        await self.receiver.start()
        self.worker = asyncio.create_task(self._process_audio())

    def _on_pcm(self, pcm: bytes, received_ns: int) -> None:
        try:
            self.queue.put_nowait((pcm, received_ns))
        except asyncio.QueueFull:
            logger.warning("audio queue full; dropping one PCM frame")

    def _on_network_stats(self, stats: Dict[str, Any]) -> None:
        self.network_stats = stats

    async def _process_audio(self) -> None:
        while True:
            item = await self.queue.get()
            if item is None:
                return
            pcm, received_ns = item
            for event in self.segmenter.feed(pcm, received_ns):
                await self._handle_segment(event)

    async def _handle_segment(self, event: SegmentEvent) -> None:
        if event.kind == "partial":
            if self.partial_task is None or self.partial_task.done():
                self.partial_task = asyncio.create_task(
                    self._transcribe(event, final=False)
                )
            return
        if self.partial_task and not self.partial_task.done():
            self.partial_task.cancel()
        await self._transcribe(event, final=True)

    async def _transcribe(self, event: SegmentEvent, final: bool) -> None:
        utterance_id = f"{self.session_id}:{event.utterance_index}"
        asr_started_ns = time.perf_counter_ns()
        try:
            result = await self.asr.transcribe_detailed(event.pcm)
            if not result.text:
                return
            self.sequence += 1
            speech_span_ms = max(
                0.0, (event.last_voice_ns - event.speech_started_ns) / 1_000_000
            )
            audio_duration_ms = max(result.audio_duration_ms, 0.001)
            metrics = {
                "schemaVersion": 1,
                "audio": {
                    "durationMs": round(result.audio_duration_ms, 2),
                    "speechSpanMs": round(speech_span_ms, 2),
                    "boundaryReason": event.boundary_reason,
                },
                "agora": dict(self.network_stats),
                "bridge": {
                    "endpointMs": round(event.endpoint_ms, 2),
                    "audioQueueMs": round(
                        max(0.0, (asr_started_ns - event.emitted_ns) / 1_000_000), 2
                    ),
                    "asrRequestPrepareMs": round(result.request_prepare_ms, 2),
                    "asrHttpRoundTripMs": round(result.http_round_trip_ms, 2),
                    "asrResponseParseMs": round(result.response_parse_ms, 2),
                    "asrTotalMs": round(result.total_ms, 2),
                },
                "asr": {
                    "rtf": round(result.total_ms / audio_duration_ms, 4),
                    "serverTimingMs": result.server_timing_ms,
                },
            }
            payload = {
                "type": "asr.final" if final else "asr.partial",
                "sessionId": self.session_id,
                "utteranceId": utterance_id,
                "seq": self.sequence,
                "text": result.text,
                "metrics": metrics,
                "bridgeResultReadyAtUnixMs": int(time.time() * 1000),
            }
            send_started_ns = time.perf_counter_ns()
            await self.emit(payload)
            send_ms = (time.perf_counter_ns() - send_started_ns) / 1_000_000
            await self.emit(
                {
                    "type": "trace.update",
                    "sessionId": self.session_id,
                    "utteranceId": utterance_id,
                    "seq": self.sequence,
                    "eventType": payload["type"],
                    "metrics": {
                        "bridge": {
                            "resultWebSocketSendMs": round(send_ms, 2),
                        }
                    },
                }
            )
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            logger.exception("ASR request failed")
            await self.emit(
                {
                    "type": "asr.error",
                    "sessionId": self.session_id,
                    "utteranceId": utterance_id,
                    "message": str(exc),
                }
            )

    async def commit(self) -> None:
        for event in self.segmenter.commit():
            await self._handle_segment(event)

    async def speak(
        self,
        text: str,
        voice: Optional[str] = None,
        speed: Optional[float] = None,
        instruct: Optional[str] = None,
    ) -> None:
        if self.tts is None:
            await self.emit(
                {
                    "type": "tts.error",
                    "sessionId": self.session_id,
                    "message": "TTS is not configured on this bridge",
                }
            )
            return
        text = text.strip()
        if not text:
            return
        # Barge-in: a new speak request interrupts the current playback.
        if self.tts_task and not self.tts_task.done():
            self.tts_task.cancel()
            try:
                await self.tts_task
            except asyncio.CancelledError:
                pass
            self.receiver.clear_audio_buffer()
        self.tts_task = asyncio.create_task(
            self._speak(text, voice=voice, speed=speed, instruct=instruct)
        )

    async def _speak(
        self,
        text: str,
        voice: Optional[str] = None,
        speed: Optional[float] = None,
        instruct: Optional[str] = None,
    ) -> None:
        await self.emit(
            {
                "type": "tts.started",
                "sessionId": self.session_id,
                "characters": len(text),
            }
        )
        try:
            upsampler = PcmUpsampler2x()
            pending = bytearray()
            stream_done = False
            stream_error: Optional[Exception] = None

            # Audio framing: 20 ms frames at 48 kHz mono 16-bit
            # 48000 * 2 * 0.02 = 1920 bytes per 20 ms frame (960 samples)
            frame_bytes = int(TTS_TARGET_RATE * 2 * 0.02)
            prebuffer_bytes = frame_bytes * 5  # 100 ms pre-buffering
            bytes_per_ms = TTS_TARGET_RATE * 2 // 1000

            async def produce() -> None:
                nonlocal stream_done, stream_error
                try:
                    try:
                        tts_stream = self.tts.stream_pcm(
                            text, voice=voice, speed=speed, instruct=instruct
                        )
                    except TypeError:
                        tts_stream = self.tts.stream_pcm(text, voice=voice)
                    async for chunk in tts_stream:
                        pending.extend(upsampler.feed(chunk))
                except Exception as exc:
                    stream_error = exc
                finally:
                    stream_done = True

            producer = asyncio.create_task(produce())
            pushed_ms = 0
            pushed_frames = 0
            start_time: Optional[float] = None

            try:
                while True:
                    if stream_error is not None:
                        raise stream_error

                    # Pre-buffer: wait until we have >= 100 ms buffered,
                    # or the stream has completed.
                    if start_time is None:
                        if len(pending) >= prebuffer_bytes or stream_done:
                            if not pending and stream_done:
                                if not stream_error:
                                    raise RuntimeError("TTS returned no audio")
                                break
                            start_time = time.perf_counter()
                            pushed_frames = 0
                        else:
                            await asyncio.sleep(0.01)
                            continue

                    # Buffer underflow check: if pending has less than a frame
                    if len(pending) < frame_bytes:
                        if stream_done:
                            # Push remainder padded to frame_bytes if any audio left
                            if pending:
                                tail = bytearray(pending).ljust(frame_bytes, b"\x00")
                                pending.clear()
                                self.receiver.push_pcm(tail, TTS_TARGET_RATE, 1)
                                pushed_ms += frame_bytes // bytes_per_ms
                            break
                        # Still generating but buffer starved: reset pacing and wait
                        start_time = None
                        await asyncio.sleep(0.01)
                        continue

                    # Precise monotonic pacing: schedule each 20 ms frame
                    target_time = start_time + (pushed_frames * 0.02)
                    now = time.perf_counter()
                    delay = target_time - now
                    if delay > 0:
                        await asyncio.sleep(delay)

                    frame = bytearray(pending[:frame_bytes])
                    del pending[:frame_bytes]
                    if self.receiver.push_pcm(frame, TTS_TARGET_RATE, 1):
                        pushed_frames += 1
                        pushed_ms += 20
                    else:
                        # SDK connection closed or rejected frame
                        break

                # Playout drain wait: allow the last pushed frames to play out
                # (150 ms covers WebRTC jitter buffer drain)
                await asyncio.sleep(0.15)
                logger.info("tts playout complete: pushed=%dms", pushed_ms)
                await self.emit({"type": "tts.finished", "sessionId": self.session_id})
            finally:
                if not producer.done():
                    producer.cancel()
                    try:
                        await producer
                    except asyncio.CancelledError:
                        pass
        except asyncio.CancelledError:
            self.receiver.clear_audio_buffer()
            raise
        except Exception as exc:
            logger.exception("TTS speak failed")
            await self.emit(
                {
                    "type": "tts.error",
                    "sessionId": self.session_id,
                    "message": str(exc),
                }
            )

    async def stop(self) -> None:
        if self.tts_task and not self.tts_task.done():
            self.tts_task.cancel()
            try:
                await self.tts_task
            except asyncio.CancelledError:
                pass
        self.receiver.clear_audio_buffer()
        stop_result = self.receiver.stop()
        if asyncio.iscoroutine(stop_result):
            await stop_result
        if self.worker:
            await self.queue.put(None)
            await self.worker
        if self.partial_task and not self.partial_task.done():
            self.partial_task.cancel()
        await self.asr.close()
        if self.tts is not None:
            await self.tts.close()


class MockSession:
    LINES = [
        "声网负责把浏览器的实时音频安全地送到内网桥接器。",
        "OminiX ASR 仍然运行在公司内网，不需要开放入站端口。",
        "这台 VPS 只负责会话控制和识别文本转发。",
    ]

    def __init__(
        self,
        session_id: str,
        emit: Callable[[Dict[str, Any]], Awaitable[None]],
    ) -> None:
        self.session_id = session_id
        self.emit = emit
        self.task: Optional[asyncio.Task] = None
        self.index = 0
        self.sequence = 0

    async def start(self) -> None:
        self.task = asyncio.create_task(self._play())

    async def _play(self) -> None:
        for line in self.LINES:
            utterance_id = f"{self.session_id}:mock-{self.index + 1}"
            await asyncio.sleep(0.7)
            self.sequence += 1
            await self.emit(
                {
                    "type": "asr.partial",
                    "sessionId": self.session_id,
                    "utteranceId": utterance_id,
                    "seq": self.sequence,
                    "text": line[: max(4, len(line) // 2)],
                }
            )
            await asyncio.sleep(0.65)
            self.sequence += 1
            await self.emit(
                {
                    "type": "asr.final",
                    "sessionId": self.session_id,
                    "utteranceId": utterance_id,
                    "seq": self.sequence,
                    "text": line,
                    "metrics": {
                        "schemaVersion": 1,
                        "audio": {"durationMs": 2400, "boundaryReason": "mock"},
                        "agora": {
                            "networkTransportDelayMs": 48,
                            "jitterBufferDelayMs": 22,
                            "audioLossRatePercent": 0,
                        },
                        "bridge": {
                            "endpointMs": 650,
                            "asrRequestPrepareMs": 3,
                            "asrHttpRoundTripMs": 420,
                            "asrResponseParseMs": 1,
                            "asrTotalMs": 424,
                        },
                        "asr": {"rtf": 0.1767},
                        "mock": True,
                    },
                }
            )
            self.index += 1

    async def commit(self) -> None:
        self.index += 1
        self.sequence += 1
        await self.emit(
            {
                "type": "asr.final",
                "sessionId": self.session_id,
                "utteranceId": f"{self.session_id}:mock-{self.index}",
                "seq": self.sequence,
                "text": "手动断句成功：端到端控制链路工作正常。",
            }
        )

    async def speak(
        self,
        text: str,
        voice: Optional[str] = None,
        speed: Optional[float] = None,
        instruct: Optional[str] = None,
    ) -> None:
        await self.emit(
            {
                "type": "tts.started",
                "sessionId": self.session_id,
                "characters": len(text),
            }
        )
        await asyncio.sleep(0.3)
        await self.emit({"type": "tts.finished", "sessionId": self.session_id})

    async def stop(self) -> None:
        if self.task and not self.task.done():
            self.task.cancel()
            try:
                await self.task
            except asyncio.CancelledError:
                pass


class BridgeApp:
    def __init__(
        self,
        mode: str,
        ws_url: str,
        shared_secret: str,
        asr_urls: Optional[List[str]] = None,
        max_sessions: int = 64,
        tts_url: Optional[str] = None,
    ) -> None:
        self.mode = mode
        self.ws_url = ws_url
        self.shared_secret = shared_secret
        self.asr_urls = asr_urls or []
        self.max_sessions = max_sessions
        self.tts_url = tts_url
        self.websocket: Any = None
        self.send_lock = asyncio.Lock()
        self.sessions: Dict[str, Any] = {}
        self._asr_rr_index = 0
        self.stopping = asyncio.Event()

    def _next_asr_url(self) -> str:
        url = self.asr_urls[self._asr_rr_index % len(self.asr_urls)]
        self._asr_rr_index += 1
        return url

    async def emit(self, payload: Dict[str, Any]) -> None:
        async with self.send_lock:
            if self.websocket is not None:
                await self.websocket.send(json.dumps(payload, ensure_ascii=False))

    async def handle(self, payload: Dict[str, Any]) -> None:
        event_type = payload.get("type")
        session_id = payload.get("sessionId")
        if event_type == "session.start":
            if session_id in self.sessions:
                await self.stop_session(session_id, notify=False)
            if len(self.sessions) >= self.max_sessions:
                logger.warning("bridge at capacity; rejecting session %s", session_id)
                await self.emit(
                    {
                        "type": "asr.error",
                        "sessionId": session_id,
                        "message": "Bridge is at its session capacity",
                    }
                )
                return
            if self.mode == "mock":
                session = MockSession(session_id, self.emit)
            else:
                if not self.asr_urls:
                    raise RuntimeError("ASR_URLS or ASR_URL is required in real mode")
                provider = "livekit" if "livekit" in payload else "agora"
                rtc_config = payload.get("livekit") or payload["agora"]
                session = RealSession(
                    session_id,
                    rtc_config,
                    self._next_asr_url(),
                    self.emit,
                    tts_url=self.tts_url,
                    provider=provider,
                )
            self.sessions[session_id] = session
            try:
                await session.start()
                await self.emit({"type": "session.ready", "sessionId": session_id})
                logger.info(
                    "session ready: %s (active=%d)", session_id, len(self.sessions)
                )
            except Exception as exc:
                logger.exception("session startup failed")
                await self.emit(
                    {
                        "type": "asr.error",
                        "sessionId": session_id,
                        "message": str(exc),
                    }
                )
                await self.stop_session(session_id, notify=False)
        elif event_type == "utterance.commit":
            session = self.sessions.get(session_id)
            if session is not None:
                await session.commit()
        elif event_type == "tts.speak":
            session = self.sessions.get(session_id)
            if session is not None:
                await session.speak(
                    payload.get("text", ""),
                    voice=payload.get("voice"),
                    speed=payload.get("speed"),
                    instruct=payload.get("instruct"),
                )
        elif event_type == "session.stop":
            await self.stop_session(session_id, notify=True)

    async def stop_session(self, session_id: Optional[str], notify: bool) -> None:
        session = self.sessions.pop(session_id, None)
        if session is None:
            return
        try:
            await session.stop()
        finally:
            if notify:
                await self.emit({"type": "session.closed", "sessionId": session_id})

    async def stop_all_sessions(self) -> None:
        for session_id in list(self.sessions):
            await self.stop_session(session_id, notify=False)

    async def run(self) -> None:
        backoff = 1.0
        while not self.stopping.is_set():
            try:
                logger.info("connecting control plane: %s", self.ws_url)
                async with websockets.connect(
                    self.ws_url,
                    additional_headers={
                        "Authorization": "Bearer " + self.shared_secret
                    },
                    # The control plane is directly reachable over public HTTPS.
                    # Ignore shell/system SOCKS proxy variables; websockets 15
                    # otherwise requires the optional python-socks dependency.
                    proxy=None,
                    ping_interval=20,
                    ping_timeout=20,
                    max_size=1_048_576,
                ) as websocket:
                    self.websocket = websocket
                    backoff = 1.0
                    logger.info("bridge connected in %s mode", self.mode)
                    async for message in websocket:
                        try:
                            await self.handle(json.loads(message))
                        except Exception:
                            logger.exception("failed to handle control message")
            except asyncio.CancelledError:
                break
            except Exception as exc:
                logger.warning("bridge connection lost: %s", exc)
            finally:
                self.websocket = None
                await self.stop_all_sessions()
            if not self.stopping.is_set():
                await asyncio.sleep(backoff + random.random() * 0.25)
                backoff = min(backoff * 2, 20.0)

    async def stop(self) -> None:
        self.stopping.set()
        await self.stop_all_sessions()
        if self.websocket is not None:
            await self.websocket.close()
        AgoraReceiver.shutdown_service()


async def async_main(args: argparse.Namespace) -> None:
    secret = os.getenv("BRIDGE_SHARED_SECRET", "")
    if len(secret) < 16:
        raise SystemExit("BRIDGE_SHARED_SECRET must contain at least 16 characters")
    asr_urls = parse_asr_urls(
        os.getenv("ASR_URLS", ""),
        os.getenv("ASR_URL") or os.getenv("SENSEVOICE_URL", ""),
    )
    if args.mode == "real" and not asr_urls:
        raise SystemExit("ASR_URLS or ASR_URL is required in real mode")
    max_sessions = int(os.getenv("BRIDGE_MAX_SESSIONS", "64"))
    tts_url = os.getenv("TTS_URL", "").strip() or None
    app = BridgeApp(
        args.mode, args.control_ws_url, secret, asr_urls, max_sessions, tts_url
    )
    loop = asyncio.get_running_loop()
    for signal_name in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(signal_name, lambda: asyncio.create_task(app.stop()))
    try:
        await app.run()
    finally:
        await app.stop()


def main() -> None:
    parser = argparse.ArgumentParser(description="Agora to private ASR LAN bridge")
    parser.add_argument(
        "--mode", choices=("mock", "real"), default=os.getenv("BRIDGE_MODE", "real")
    )
    parser.add_argument(
        "--control-ws-url",
        default=os.getenv("CONTROL_WS_URL", "ws://localhost:8080/ws/bridge"),
    )
    parser.add_argument("--log-level", default=os.getenv("LOG_LEVEL", "INFO"))
    args = parser.parse_args()
    logging.basicConfig(
        level=getattr(logging, args.log_level.upper()),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    asyncio.run(async_main(args))


if __name__ == "__main__":
    main()

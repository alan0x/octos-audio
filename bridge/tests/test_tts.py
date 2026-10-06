import json
import unittest

import httpx

from bridge.tts import (
    PcmUpsampler2x,
    TtsClient,
    fit_trailing_silence,
    normalize_tts_text,
    plan_tts_chunks,
    split_tts_chunks,
    truncation_reason,
)

LONG_TEXT = (
    "欢迎来到云朵科学馆，当你沿着大厅左侧的蓝色指示线缓步前行，经过展示声音如何穿过空气的互动装置，"
    "再走过一座长约 3.5 米、会随着脚步亮起柔和灯光的小桥之后，就能在二楼体验区听到由 TTS 系统生成的"
    "中文与 English 混合播报；为了让每一位参观者都能听清说明，我们将背景音乐的音量降低了 20%，"
    "并在介绍数字、单位和操作步骤时保留适当的停顿，同时提醒你：先按下绿色按钮，等待三秒，再轻轻转动"
    "右侧旋钮，观察屏幕上的波形是否随声音变化——如果一切准备就绪，你愿意和我们一起，用耳朵发现那些"
    "平时容易被忽略的细节吗？让我们开始今天的声音探索吧！"
)


class NormalizeTtsTextTest(unittest.TestCase):
    def test_chinese_text_avoids_comma_splitting_by_default(self):
        self.assertEqual(
            normalize_tts_text("你好，世界！好吗？"),
            "你好,世界！好吗？",
        )
        self.assertEqual(
            normalize_tts_text("第一、第二；第三，第四。"),
            "第一,第二;第三,第四。",
        )

    def test_chinese_multi_line_paragraphs_are_collapsed(self):
        text = "第一行。\n第二行，继续。\n第三行。"
        self.assertEqual(
            normalize_tts_text(text),
            "第一行。 第二行,继续。 第三行。",
        )

    def test_chinese_text_legacy_full_width_when_disabled(self):
        self.assertEqual(
            normalize_tts_text("你好,世界!好吗?", avoid_comma_split=False),
            "你好，世界！好吗？",
        )

    def test_ascii_text_is_untouched(self):
        self.assertEqual(normalize_tts_text("hello, world!"), "hello, world!")


class SplitTtsChunksTest(unittest.TestCase):
    def test_long_paragraph_is_split_into_short_clause_aligned_chunks(self):
        self.assertEqual(
            split_tts_chunks(LONG_TEXT),
            [
                "欢迎来到云朵科学馆,当你沿着大厅左侧的蓝色指示线缓步前行。",
                "经过展示声音如何穿过空气的互动装置,再走过一座长约 3.5 米。",
                "会随着脚步亮起柔和灯光的小桥之后。",
                "就能在二楼体验区听到由 TTS 系统生成的中文与 English 混合播报。",
                "为了让每一位参观者都能听清说明,我们将背景音乐的音量降低了 百分之20。",
                "并在介绍数字,单位和操作步骤时保留适当的停顿,同时提醒你。",
                "先按下绿色按钮,等待三秒,再轻轻转动右侧旋钮,观察屏幕上的波形是否随声音变化。",
                "如果一切准备就绪,你愿意和我们一起,用耳朵发现那些平时容易被忽略的细节吗？",
                "让我们开始今天的声音探索吧！",
            ],
        )

    def test_chunks_respect_max_chars_and_keep_every_character(self):
        chunks = split_tts_chunks(LONG_TEXT, max_chars=20)
        for chunk in chunks:
            # Clauses are packed up to the limit; only a single clause may exceed it.
            if "," in chunk:
                self.assertLessEqual(len(chunk), 21, chunk)
        spoken = lambda text: [c for c in text if "一" <= c <= "鿿"]
        self.assertEqual(spoken("".join(chunks)), spoken(LONG_TEXT.replace("%", "百分之")))

    def test_inner_clause_marks_do_not_trigger_server_splitting(self):
        for chunk in split_tts_chunks(LONG_TEXT):
            inner = chunk[:-1]
            self.assertFalse(any(mark in inner for mark in "，、；：。！？"), chunk)
            self.assertNotRegex(inner, r"[,;:.!?]\s")

    def test_dash_becomes_clause_break(self):
        self.assertEqual(
            split_tts_chunks("观察波形——如果准备就绪，就开始吧。", max_chars=6),
            ["观察波形。", "如果准备就绪。", "就开始吧。"],
        )

    def test_percent_is_spelled_out(self):
        self.assertEqual(split_tts_chunks("音量降低了20%和3.5％。"), ["音量降低了百分之20和百分之3.5。"])

    def test_digit_grouping_comma_is_not_a_break(self):
        self.assertEqual(split_tts_chunks("共有1,000人，请排队。", max_chars=4), ["共有1,000人。", "请排队。"])

    def test_lines_and_sentences_are_boundaries(self):
        self.assertEqual(
            split_tts_chunks("第一行\n第二行：第三句！？第四句"),
            ["第一行。", "第二行。", "第三句！？", "第四句。"],
        )

    def test_long_clause_is_not_cut_mid_clause(self):
        clause = "这是一个没有任何标点而且长度超过上限的很长很长的分句"
        self.assertEqual(split_tts_chunks(clause, max_chars=10), [clause + "。"])

    def test_pause_follows_the_original_boundary(self):
        self.assertEqual(
            plan_tts_chunks("第一句话。注意：先按下按钮，再等待三秒钟后转动旋钮", max_chars=8),
            [
                ("第一句话。", 400),
                ("注意。", 300),
                ("先按下按钮。", 150),
                ("再等待三秒钟后转动旋钮。", 400),
            ],
        )

    def test_non_chinese_text_is_one_chunk(self):
        self.assertEqual(split_tts_chunks(" hello, world! "), ["hello, world!"])
        self.assertEqual(split_tts_chunks("  "), [])


def tone_ms(ms, amplitude=3000):
    # 24 kHz square wave: RMS == amplitude.
    return pcm([amplitude if i % 2 else -amplitude for i in range(24 * ms)])


def silence_ms(ms):
    return b"\x00\x00" * (24 * ms)


class TruncationDetectionTest(unittest.TestCase):
    TEXT = "先按下绿色按钮,等待三秒,再轻轻转动右侧旋钮。"  # 20 CJK chars

    def test_complete_audio_passes(self):
        self.assertIsNone(truncation_reason(self.TEXT, tone_ms(4000) + silence_ms(100)))

    def test_long_silent_tail_is_truncation(self):
        reason = truncation_reason(self.TEXT, tone_ms(4000) + silence_ms(2500))
        self.assertIn("trailing silence", reason)

    def test_all_silence_is_truncation(self):
        self.assertEqual(truncation_reason(self.TEXT, silence_ms(3000)), "no voiced audio")
        self.assertEqual(truncation_reason(self.TEXT, b""), "no voiced audio")

    def test_implausibly_fast_speech_is_truncation(self):
        self.assertIn("speech rate", truncation_reason(self.TEXT, tone_ms(1000)))

    def test_rate_limit_scales_with_speed(self):
        self.assertIsNone(truncation_reason(self.TEXT, tone_ms(1600), speed=2.0))
        self.assertIsNotNone(truncation_reason(self.TEXT, tone_ms(1600), speed=1.0))

    def test_fit_trims_long_tail(self):
        audio = tone_ms(1000) + silence_ms(2000)
        self.assertEqual(fit_trailing_silence(audio), tone_ms(1000) + silence_ms(300))
        self.assertEqual(fit_trailing_silence(tone_ms(500)), tone_ms(500))

    def test_fit_pads_short_tail(self):
        audio = tone_ms(1000) + silence_ms(60)
        self.assertEqual(fit_trailing_silence(audio, 450, pad=True), tone_ms(1000) + silence_ms(450))


def client_with(responses):
    """TtsClient whose HTTP calls pop PCM bodies from ``responses``."""
    requests = []

    def handler(request):
        requests.append(json.loads(request.content))
        return httpx.Response(200, content=responses.pop(0))

    client = TtsClient("http://tts.local/v1/audio/speech", max_attempts=3)
    client._client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
    return client, requests


async def collect(client, text):
    return [chunk async for chunk in client.stream_pcm(text)]


class TtsClientChunkingTest(unittest.IsolatedAsyncioTestCase):
    async def test_each_chunk_is_a_separate_request(self):
        good = tone_ms(3000)
        client, requests = client_with([good, good])
        out = await collect(client, "第一句话说得很清楚。第二句话也说得很清楚。")
        self.assertEqual(
            [r["input"] for r in requests],
            ["第一句话说得很清楚。", "第二句话也说得很清楚。"],
        )
        # A sentence pause follows every chunk except the last.
        self.assertEqual(out, [good + silence_ms(400), good])
        self.assertEqual(requests[0]["seed"], 42)
        self.assertEqual(requests[0]["voice"], "serena")

    async def test_truncated_chunk_is_retried_with_new_seed(self):
        good = tone_ms(3000) + silence_ms(100)
        client, requests = client_with([tone_ms(800) + silence_ms(3000), good])
        out = await collect(client, "第一句话说得很清楚。")
        self.assertEqual([r["seed"] for r in requests], [42, 43])
        self.assertEqual(out, [good])

    async def test_best_attempt_is_used_when_all_attempts_look_truncated(self):
        client, requests = client_with(
            [tone_ms(300) + silence_ms(3000), tone_ms(900) + silence_ms(3000), silence_ms(3000)]
        )
        out = await collect(client, "第一句话说得很清楚。")
        self.assertEqual(len(requests), 3)
        self.assertEqual(out, [tone_ms(900) + silence_ms(300)])

    async def test_legacy_mode_sends_one_full_width_request(self):
        client, requests = client_with([tone_ms(3000)])
        client.avoid_comma_split = False
        await collect(client, "你好,世界!再见。")
        self.assertEqual([r["input"] for r in requests], ["你好，世界！再见。"])


class TtsClientConfigTest(unittest.TestCase):
    def test_default_config_has_sampling_parameters(self):
        from bridge.tts import TtsClient

        client = TtsClient("http://127.0.0.1:8090/v1/audio/speech")
        self.assertEqual(client.temperature, 0.2)
        self.assertEqual(client.top_p, 0.8)
        self.assertEqual(client.seed, 42)
        self.assertTrue(client.avoid_comma_split)

    def test_custom_sampling_config(self):
        from bridge.tts import TtsClient

        client = TtsClient(
            "http://127.0.0.1:8090/v1/audio/speech",
            temperature=0.0,
            top_p=0.9,
            seed=42,
            avoid_comma_split=False,
        )
        self.assertEqual(client.temperature, 0.0)
        self.assertEqual(client.top_p, 0.9)
        self.assertEqual(client.seed, 42)
        self.assertFalse(client.avoid_comma_split)


def pcm(samples):
    return b"".join(int(s).to_bytes(2, "little", signed=True) for s in samples)


def unpack(data):
    return [
        int.from_bytes(data[i : i + 2], "little", signed=True)
        for i in range(0, len(data), 2)
    ]


class PcmUpsampler2xTest(unittest.TestCase):
    def test_doubles_with_linear_interpolation(self):
        upsampler = PcmUpsampler2x()
        out = unpack(upsampler.feed(pcm([0, 100, 200])))
        # s0, mid(s0,s1), s1, mid(s1,s2), s2
        self.assertEqual(out, [0, 50, 100, 150, 200])

    def test_negative_samples_round_toward_zero(self):
        upsampler = PcmUpsampler2x()
        out = unpack(upsampler.feed(pcm([-100, -200])))
        self.assertEqual(out, [-100, -150, -200])

    def test_chunk_boundaries_stay_continuous(self):
        whole = PcmUpsampler2x().feed(pcm([10, 20, 30, 40]))
        split = PcmUpsampler2x()
        part1 = split.feed(pcm([10, 20]))
        part2 = split.feed(pcm([30, 40]))
        self.assertEqual(part1 + part2, whole)

    def test_odd_byte_is_carried(self):
        upsampler = PcmUpsampler2x()
        data = pcm([1000, 2000])
        out1 = upsampler.feed(data[:3])  # 1.5 samples
        out2 = upsampler.feed(data[3:])
        self.assertEqual(unpack(out1 + out2), [1000, 1500, 2000])

    def test_empty_feed_returns_empty(self):
        self.assertEqual(PcmUpsampler2x().feed(b""), b"")


class FakeTtsClient:
    def __init__(self, chunks: list[bytes]) -> None:
        self.chunks = chunks

    async def stream_pcm(self, text: str, voice: str = None):
        for chunk in self.chunks:
            yield chunk


class FakeReceiver:
    def __init__(self) -> None:
        self.pushed_frames: list[bytes] = []
        self.cleared = False

    def push_pcm(self, pcm: bytes, sample_rate: int = 48000, channels: int = 1) -> bool:
        self.pushed_frames.append(bytes(pcm))
        return True

    def clear_audio_buffer(self) -> None:
        self.cleared = True

    def stop(self) -> None:
        pass


class TtsPlayoutPacerTest(unittest.IsolatedAsyncioTestCase):
    async def test_speak_pushes_20ms_frames_and_emits_finished(self):
        from bridge.main import RealSession, TTS_TARGET_RATE

        emitted = []

        async def emit(payload):
            emitted.append(payload)

        session = RealSession(
            session_id="test-session",
            agora={"appId": "app", "channel": "chan", "token": "tok", "uid": 9001},
            asr_url="http://localhost:8080",
            emit=emit,
            tts_url="http://localhost:8090/v1/audio/speech",
        )
        fake_receiver = FakeReceiver()
        session.receiver = fake_receiver

        # Create 200 ms of 24 kHz audio (24000 * 2 * 0.2 = 9600 bytes)
        # After 2x upsampling, becomes 400 ms of 48 kHz audio (19200 bytes = 10 frames of 20 ms)
        raw_chunk = b"\x01\x00" * 4800
        session.tts = FakeTtsClient([raw_chunk])

        await session.speak("你好")
        await session.tts_task

        # Verify events
        self.assertEqual(emitted[0]["type"], "tts.started")
        self.assertEqual(emitted[0]["characters"], 2)
        self.assertEqual(emitted[1]["type"], "tts.finished")

        # Verify each frame is exactly 20 ms = 1920 bytes
        frame_bytes = int(TTS_TARGET_RATE * 2 * 0.02)
        self.assertEqual(frame_bytes, 1920)
        self.assertGreater(len(fake_receiver.pushed_frames), 0)
        for frame in fake_receiver.pushed_frames:
            self.assertEqual(len(frame), frame_bytes)

    async def test_barge_in_cancels_previous_and_clears_buffer(self):
        import asyncio
        from bridge.main import RealSession

        emitted = []

        async def emit(payload):
            emitted.append(payload)

        session = RealSession(
            session_id="test-session",
            agora={"appId": "app", "channel": "chan", "token": "tok", "uid": 9001},
            asr_url="http://localhost:8080",
            emit=emit,
            tts_url="http://localhost:8090/v1/audio/speech",
        )
        fake_receiver = FakeReceiver()
        session.receiver = fake_receiver

        # A long stream: 10 chunks of 100 ms each
        class SlowTtsClient:
            async def stream_pcm(self, text, voice=None):
                for _ in range(10):
                    yield b"\x01\x00" * 2400
                    await asyncio.sleep(0.05)

        session.tts = SlowTtsClient()

        # Start first speak
        await session.speak("第一句很长的话")
        first_task = session.tts_task
        await asyncio.sleep(0.02)

        # Barge-in with a second speak
        session.tts = FakeTtsClient([b"\x01\x00" * 2400])
        await session.speak("第二句短话")

        self.assertTrue(first_task.done())
        self.assertTrue(fake_receiver.cleared)
        await session.tts_task


if __name__ == "__main__":
    unittest.main()

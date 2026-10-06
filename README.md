# Octos Audio — Private Realtime Speech (LiveKit/Agora × Qwen3-ASR/TTS)

> A self-hosted, production-shaped reference architecture for real-time speech
> recognition and synthesis. Browser audio flows over RTC into a private
> inference host running Qwen3-ASR / Qwen3-TTS on Apple Silicon (via
> [OminiX-API](https://github.com/OminiX-ai/OminiX-API)), and results stream
> back over a lightweight control plane. **No audio ever leaves your own
> infrastructure.**

## What problem does it solve?

- **Privacy & compliance**: audio and all ASR/TTS inference stay inside your
  own infrastructure — nothing passes through a third-party AI API.
- **Cost control**: inference runs on local Apple Silicon (M-series) and
  scales linearly with an instance pool; no per-minute billing.
- **Pluggable RTC transport**: self-hosted LiveKit SFU (zero third-party
  dependency) or Agora Cloud, switchable with one config flag.
- **Production shape, not a toy**: short-lived dynamic credentials, session
  auth, admission control, pooled inference workers, and end-to-end latency
  observability out of the box.

## Architecture

```text
Browser (Web SDK) ──RTC audio──> RTC layer (self-hosted LiveKit SFU / Agora)
                                      │ 16 kHz PCM
                                      ▼
                        Inference host: Bridge (Python) ──HTTP──> OminiX pool
                                      │            (Qwen3-ASR / Qwen3-TTS)
                                      │ outbound WSS
Browser (Web SDK) <──text/events WSS── Control plane (Rust + Salvo)
                                       auth · sessions · credential issuing
```

- The RTC layer only forwards audio. The inference host and models expose **no
  public inbound ports** — the Bridge dials out for everything.
- The control plane never touches audio or inference: it authenticates
  clients, issues short-lived per-session RTC credentials (Agora
  AccessToken2 or LiveKit JWT), and relays recognition events.
- Endpointing (VAD) runs in the Bridge and is replaceable. TTS and ASR run in
  separate instances so long syntheses never block recognition.

## Components

| Path | Contents |
|---|---|
| `control-plane/` | Rust + Salvo: auth, session management, RTC credential issuing, WebSocket relay, static site (home / docs / playground) |
| `bridge/` | Python: RTC receive/publish (Agora Server SDK or livekit-rtc), endpointing, ASR pool routing, TTS synthesis & playout, load-test tools |
| `deploy/` | Docker Compose, environment templates, Nginx reverse-proxy example |
| `docs/PROTOCOL.md` | Control-plane protocol: REST, WebSocket events, metrics conventions |

## Features

- **Concurrent sessions**: `SESSION_CAPACITY` admission control + round-robin
  ASR instance pool; 90+ concurrent sessions measured on a single M-series host.
- **Two-way TTS**: text is pushed through the control plane, synthesized on the
  Bridge and published into the RTC room; preset voice/speed/instruct parameters and
  barge-in are supported. Voice creation and cloning are not exposed by this project.
- **Latency observability**: per-utterance waterfall (network, endpointing,
  inference, relay, render), P50/P95 statistics, JSON export.
- **Mock mode**: develop the control plane and frontend without any RTC
  credentials.

## Quickstart (mock mode)

No credentials required:

```bash
cp deploy/.env.example deploy/.env   # set DEMO_MODE=true
docker compose --env-file deploy/.env -f deploy/docker-compose.yml --profile mock up -d --build
# open http://localhost:18080 — the mock bridge streams simulated transcripts
```

A real deployment needs an Apple Silicon host running
[OminiX-API](https://github.com/OminiX-ai/OminiX-API) (Qwen3-ASR/TTS models),
and an RTC layer: a self-hosted [LiveKit](https://github.com/livekit/livekit)
SFU or an Agora project. Configure `deploy/.env.example` and
`bridge/.env.example`, then start the control plane and the bridge
(`bridge/start-real.sh`).

## Checks

```bash
make check   # cargo test + clippy, Python/JS tests, JS/shell syntax checks
make integration-check  # local Ping/Pong, silent disconnect and startup timeout regressions
```

Requires Rust 1.96+, Python 3.10+, Node.js.

## References

- [OminiX-API](https://github.com/OminiX-ai/OminiX-API) — OpenAI-compatible inference server for Apple Silicon
- [LiveKit](https://github.com/livekit/livekit) — open-source WebRTC SFU
- [Agora Web SDK](https://docs.agora.io/en/voice-calling/get-started/get-started-sdk?platform=web) / [Agora Python Server SDK](https://github.com/AgoraIO-Extensions/Agora-Python-Server-SDK)
- [Salvo](https://docs.rs/salvo/latest/salvo/)

## License

MIT — see [LICENSE](LICENSE).

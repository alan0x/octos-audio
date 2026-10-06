# Production control protocol

The control plane intentionally carries JSON control messages and ASR text only.
Audio never flows through the VPS.

## Browser API

- `GET /healthz`: process liveness.
- `GET /readyz`: readiness; returns `200` only while the LAN Bridge is connected.
- `GET /api/v1/status`: public availability and capacity without credential or session IDs.
- `POST /api/v1/browser-grants`: server-to-server exchange for a short-lived,
  one-time browser grant. Requires `Authorization: Bearer <OCTOS_SERVICE_TOKEN>`.
- `POST /api/v1/sessions`: allocate a session, up to `SESSION_CAPACITY` concurrent sessions.
- `POST /api/v1/sessions/{id}/commit`: force an utterance boundary.
- `POST /api/v1/sessions/{id}/speak`: synthesize `{"text":"...","voice":"serena"}` with Qwen3-TTS and play it into the session's RTC channel. The optional `voice` selects an existing model preset; omitting it uses the Bridge's `TTS_VOICE` setting (default `serena`). Available presets depend on the loaded TTS model. This API does not create voices, accept reference recordings, or perform voice cloning; `instruct` controls expression within the selected preset.
- `DELETE /api/v1/sessions/{id}`: stop the session.
- `GET /ws/client/{id}`: browser text-event WebSocket, authenticated by a path-scoped HttpOnly cookie.

The operator page may continue to create and control sessions with
`Authorization: Bearer <CLIENT_ACCESS_TOKEN>`. An authenticated Octos server
uses `OCTOS_SERVICE_TOKEN` to request a browser grant containing no reusable
service credential. The browser presents that grant exactly once to
`POST /api/v1/sessions`; expired or reused grants are rejected. Successful
session creation sets path-scoped HttpOnly cookies for the client WebSocket and
that session's commit/delete endpoints, so subsequent browser controls do not
reuse the grant.

Raw browser grants are never stored by the control plane: only their SHA-256
digests, owner identifiers, and expiry are retained in memory. The grant is
consumed atomically when the session is allocated. Existing single-session
capacity and bridge-readiness checks run before consumption, so a temporary
busy/offline response does not waste a valid grant.

The session response includes the Agora App ID, a unique channel, numeric UID
and dynamically issued short-lived AccessToken2 credential. The App Certificate
never leaves the VPS. The WebSocket ticket is not included in JSON or URLs.

## Bridge WebSocket

The LAN bridge opens `GET /ws/bridge` with:

```text
Authorization: Bearer <BRIDGE_SHARED_SECRET>
```

Control-plane to bridge:

```json
{"type":"session.start","sessionId":"...","agora":{"appId":"...","channel":"asr-...","uid":9001,"token":"007..."}}
{"type":"utterance.commit","sessionId":"..."}
{"type":"tts.speak","sessionId":"...","text":"要朗读的文字","voice":"vivian"}
{"type":"session.stop","sessionId":"..."}
```

Bridge to control-plane:

```json
{"type":"session.ready","sessionId":"..."}
{"type":"asr.partial","sessionId":"...","utteranceId":"...:1","seq":1,"text":"正在识别","metrics":{}}
{"type":"asr.final","sessionId":"...","utteranceId":"...:1","seq":2,"text":"最终结果","metrics":{}}
{"type":"trace.update","sessionId":"...","utteranceId":"...:1","seq":2,"eventType":"asr.final","metrics":{"bridge":{"resultWebSocketSendMs":1.2}}}
{"type":"asr.error","sessionId":"...","message":"..."}
{"type":"tts.started","sessionId":"...","characters":42}
{"type":"tts.finished","sessionId":"..."}
{"type":"tts.error","sessionId":"...","message":"..."}
{"type":"session.closed","sessionId":"..."}
```

For TTS playback the bridge streams 24 kHz PCM from a dedicated OminiX TTS
instance, upsamples it to 48 kHz, and publishes it into the session's RTC
channel; the browser subscribes and plays the remote audio track.

Browser to control-plane over the authenticated client WebSocket:

```json
{"type":"client.result_ack","sessionId":"...","utteranceId":"...:1","eventType":"asr.final","seq":2}
```

The control plane scopes ACKs to the authenticated session and returns a `trace.update` containing `vpsBrowserAckRttMs` and the explicitly labelled RTT/2 estimate. It also adds `metrics.vps.relayQueueMs` before forwarding Bridge events.

The metrics object uses process-local monotonic durations:

- `metrics.audio`: audio duration, speech span and boundary reason.
- `metrics.agora`: Server SDK receive transport delay, jitter buffer, loss and bitrate.
- `metrics.bridge`: endpointing, audio queue, request preparation, OminiX HTTP, response parsing and result send.
- `metrics.asr`: real-time factor and optional OminiX `Server-Timing` durations.
- `metrics.vps`: control-plane receive/enqueue timestamps and relay queue duration.
- `metrics.delivery`: browser ACK RTT and estimated VPS-to-browser one-way duration.
- `metrics.browser`: speech-start/final, speech-end/final, first partial and DOM update durations.
- `metrics.summary`: same-utterance end-to-end estimate derived by summing the available media, endpointing, ASR, result delivery and render stages. `speechEndToFinalMs` is diagnostic only.

Wall-clock timestamps are diagnostic only and MUST NOT be subtracted across hosts. Raw audio never enters this protocol.

## Lifecycle and current capacity

- Bridge connectivity is verified with control-plane WebSocket Ping/Pong, at
  most every 5 seconds. A missing matching Pong for
  `BRIDGE_HEARTBEAT_TIMEOUT_SECONDS` (default 15 seconds) marks the Bridge offline,
  rejects new sessions, and closes existing sessions. Silent network failures
  may take up to this timeout to appear offline. This verifies the RTC Bridge
  connection, not the health of the separate ASR/TTS inference services.
- Sessions must receive `session.ready` within `SESSION_START_TIMEOUT_SECONDS`
  (default 30 seconds), or they are stopped and their capacity is released.
  Connected clients receive `asr.error` with code `session_start_timeout`, followed
  by `session.closed`. Bridge disconnection errors use code `bridge_offline`.
- The initial `session.snapshot` includes the current session state, so clients
  must also accept a `ready` snapshot when readiness preceded their subscription.
  The browser enables recognition controls once both RTC and Bridge are ready.
- Concurrency is bounded by `SESSION_CAPACITY` on the control plane and by `BRIDGE_MAX_SESSIONS` / the OminiX instance pool on the bridge host.
- Sessions live in memory and expire automatically; a restart requires a new session.
- Bridge or browser event-socket disconnect releases the affected sessions.
- Each session uses an independent channel and independent short-lived RTC tokens.
- ASR results return over the control-plane WebSocket, not Agora RTM.

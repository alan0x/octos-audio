/* ==========================================================================
   Octos Audio API Platform - Application Logic
   ========================================================================== */

const STORAGE_KEY = "octosAudioAccessToken";

const ui = {
  // Navigation & Tabs
  navTabs: document.querySelectorAll(".nav-tab"),
  tabPanes: document.querySelectorAll(".tab-pane"),
  navBridgeDot: document.querySelector("#navBridgeDot"),
  navBridgeLabel: document.querySelector("#navBridgeLabel"),
  tokenConfigBtn: document.querySelector("#tokenConfigBtn"),
  tokenStatusText: document.querySelector("#tokenStatusText"),
  tokenModal: document.querySelector("#tokenModal"),
  accessKey: document.querySelector("#accessKey"),

  // Playground Header & Topbar
  start: document.querySelector("#startBtn"),
  mute: document.querySelector("#muteBtn"),
  commit: document.querySelector("#commitBtn"),
  stop: document.querySelector("#stopBtn"),
  sessionId: document.querySelector("#sessionId"),
  sessionState: document.querySelector("#sessionState"),
  bridgeState: document.querySelector("#bridgeState"),
  modeBadge: document.querySelector("#modeBadge"),

  // Topology Indicators
  browserDot: document.querySelector("#browserDot"),
  agoraDot: document.querySelector("#agoraDot"),
  bridgeDot: document.querySelector("#bridgeDot"),
  asrDot: document.querySelector("#asrDot"),

  // ASR Section
  transcript: document.querySelector("#transcript"),
  partial: document.querySelector("#partial"),
  level: document.querySelector(".audio-level"),
  levelBars: [...document.querySelectorAll(".audio-level i")],

  // TTS Section
  ttsText: document.querySelector("#ttsText"),
  ttsVoiceSelect: document.querySelector("#ttsVoiceSelect"),
  ttsVoiceCustom: document.querySelector("#ttsVoiceCustom"),
  ttsSpeed: document.querySelector("#ttsSpeed"),
  speedValueLabel: document.querySelector("#speedValueLabel"),
  ttsInstruct: document.querySelector("#ttsInstruct"),
  speak: document.querySelector("#speakBtn"),
  ttsStatusText: document.querySelector("#ttsStatusText"),

  // Observability & Metrics
  latestLatency: document.querySelector("#latestLatency"),
  p50Latency: document.querySelector("#p50Latency"),
  p95Latency: document.querySelector("#p95Latency"),
  sampleCount: document.querySelector("#sampleCount"),
  traceText: document.querySelector("#traceText"),
  traceStages: document.querySelector("#traceStages"),
  networkSummary: document.querySelector("#networkSummary"),
  exportMetrics: document.querySelector("#exportMetricsBtn"),
  clearMetrics: document.querySelector("#clearMetricsBtn"),
  eventLog: document.querySelector("#eventLog"),
};

const SPEECH_VOLUME_THRESHOLD = 0.025;
const SPEECH_START_SAMPLES = 2;
const METER_INTERVAL_MS = 100;

const runtime = {
  bridgeOnline: null,
  statusRequest: 0,
  starting: false,
  bridgeReady: false,
  rtcReady: false,
  startupTimer: null,
  startAttempt: 0,
  session: null,
  socket: null,
  rtcClient: null,
  room: null,
  remoteAudioElement: null,
  microphone: null,
  muted: false,
  meterTimer: null,
  accessProtected: true,
  accessToken: localStorage.getItem(STORAGE_KEY) || sessionStorage.getItem("asrAccessToken") || "",
  speech: createSpeechState(),
  observations: [],
  traces: new Map(),
  firstPartialMs: new Map(),
  networkQuality: null,
  manualCommitAt: null,
};

// ==========================================================================
// Tab Switching & Hash Routing
// ==========================================================================

function switchTab(tabId) {
  ui.navTabs.forEach((tab) => {
    const isActive = tab.dataset.tab === tabId;
    tab.classList.toggle("active", isActive);
    tab.setAttribute("aria-selected", isActive);
  });

  ui.tabPanes.forEach((pane) => {
    pane.classList.toggle("active", pane.id === `tab-${tabId}`);
  });

  if (window.location.hash !== `#${tabId}`) {
    window.location.hash = `#${tabId}`;
  }
  window.scrollTo({ top: 0, behavior: "smooth" });
}

function handleHashChange() {
  const hash = window.location.hash.replace("#", "") || "home";
  // 只响应标签级 hash;文档锚点(如 #doc-speak)不触发标签切换
  if (["home", "docs", "playground"].includes(hash)) {
    switchTab(hash);
  }
}

window.addEventListener("hashchange", handleHashChange);
ui.navTabs.forEach((tab) => {
  tab.addEventListener("click", () => switchTab(tab.dataset.tab));
});

// ==========================================================================
// Token Modal Management
// ==========================================================================

function updateTokenUI() {
  if (runtime.accessToken) {
    ui.accessKey.value = runtime.accessToken;
    ui.tokenStatusText.textContent = "Token: ••••••••";
    ui.tokenConfigBtn.style.borderColor = "rgba(16, 185, 129, 0.4)";
    ui.tokenConfigBtn.style.color = "#34d399";
  } else {
    ui.accessKey.value = "";
    ui.tokenStatusText.textContent = "配置密钥";
    ui.tokenConfigBtn.style.borderColor = "var(--border-subtle)";
    ui.tokenConfigBtn.style.color = "var(--text-secondary)";
  }
}

function openTokenModal() {
  ui.accessKey.value = runtime.accessToken;
  ui.tokenModal.classList.remove("hidden");
  setTimeout(() => ui.accessKey.focus(), 50);
}

function closeTokenModal() {
  ui.tokenModal.classList.add("hidden");
}

function saveTokenAndClose() {
  const val = ui.accessKey.value.trim();
  runtime.accessToken = val;
  if (val) {
    localStorage.setItem(STORAGE_KEY, val);
    sessionStorage.setItem("asrAccessToken", val);
  } else {
    localStorage.removeItem(STORAGE_KEY);
    sessionStorage.removeItem("asrAccessToken");
  }
  updateTokenUI();
  closeTokenModal();
  log("访问密钥已更新");
}

ui.tokenConfigBtn.addEventListener("click", openTokenModal);
updateTokenUI();

// ==========================================================================
// Code Snippets Tabs & Copy Functions (Documentation)
// ==========================================================================

document.querySelectorAll(".code-tabs-nav").forEach((nav) => {
  nav.querySelectorAll(".code-tab").forEach((tab) => {
    tab.addEventListener("click", () => {
      const container = tab.closest(".tabs-code-wrapper");
      const lang = tab.dataset.lang;
      container.querySelectorAll(".code-tab").forEach((t) => t.classList.toggle("active", t === tab));
      container.querySelectorAll(".code-tab-pane").forEach((pane) => {
        pane.classList.toggle("active", pane.dataset.lang === lang);
      });
    });
  });
});

document.querySelectorAll(".copy-btn").forEach((btn) => {
  btn.addEventListener("click", async () => {
    const text = btn.dataset.clipboard;
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
      const originalText = btn.textContent;
      btn.textContent = "✓ 已复制";
      btn.classList.add("copied");
      setTimeout(() => {
        btn.textContent = originalText;
        btn.classList.remove("copied");
      }, 2000);
    } catch {
      btn.textContent = "复制失败";
    }
  });
});

// ==========================================================================
// TTS Parameter Helpers (Playground)
// ==========================================================================

ui.ttsVoiceSelect.addEventListener("change", () => {
  const isCustom = ui.ttsVoiceSelect.value === "custom";
  ui.ttsVoiceCustom.classList.toggle("hidden", !isCustom);
  if (isCustom) ui.ttsVoiceCustom.focus();
});

ui.ttsSpeed.addEventListener("input", () => {
  ui.speedValueLabel.textContent = `${Number(ui.ttsSpeed.value).toFixed(1)}x`;
});

function setTtsSample(type) {
  if (ui.ttsText.disabled) return;
  if (type === "日常") {
    ui.ttsText.value = "你好，我是实时语音助手。今天有什么我可以帮你的吗？";
  } else if (type === "技术") {
    ui.ttsText.value = "基于声网 RTC 与本地大模型算力，系统端到端 P50 识别延时已降低至 180 毫秒以内。";
  } else if (type === "比赛") {
    ui.ttsText.value = "形式上说，赛题应该不需要百分之百做正确。比我们以往的比赛，这个比赛当然更难一些，不见得是坏事。";
  }
}

// ==========================================================================
// Core Utilities & State
// ==========================================================================

function log(message, data) {
  const stamp = new Date().toLocaleTimeString();
  const suffix = data === undefined ? "" : ` ${JSON.stringify(data)}`;
  if (ui.eventLog) {
    ui.eventLog.textContent = `[${stamp}] ${message}${suffix}\n${ui.eventLog.textContent}`.slice(0, 10000);
  }
}

function setDot(element, state) {
  if (element) {
    element.className = `status-dot${state ? ` ${state}` : ""}`;
  }
}

function setSessionState(label) {
  if (ui.sessionState) {
    ui.sessionState.textContent = label;
  }
}

function setRunning(running) {
  ui.start.disabled = running || runtime.starting || runtime.bridgeOnline !== true;
  ui.mute.disabled = !running;
  ui.commit.disabled = !running;
  ui.stop.disabled = !running && !runtime.starting;
  ui.ttsText.disabled = !running;
  ui.ttsVoiceSelect.disabled = !running;
  ui.ttsSpeed.disabled = !running;
  ui.ttsInstruct.disabled = !running;
  ui.speak.disabled = !running;
  if (ui.ttsVoiceSelect.value === "custom") {
    ui.ttsVoiceCustom.disabled = !running;
  }
}

function createSpeechState() {
  return {
    active: false,
    consecutiveSamples: 0,
    speechStartAt: null,
    lastVoiceAt: null,
  };
}

function asFiniteNumber(value) {
  if (value === null || value === undefined || value === "") return null;
  const number = Number(value);
  return Number.isFinite(number) ? number : null;
}

function roundMetric(value, digits = 2) {
  const number = asFiniteNumber(value);
  if (number === null) return null;
  const scale = 10 ** digits;
  return Math.round(number * scale) / scale;
}

function formatMs(value) {
  const number = asFiniteNumber(value);
  if (number === null) return "—";
  if (number >= 1000) return `${(number / 1000).toFixed(2)}s`;
  return `${Math.round(number)}ms`;
}

function percentile(values, ratio) {
  const sorted = values
    .map(asFiniteNumber)
    .filter((value) => value !== null)
    .sort((left, right) => left - right);
  if (!sorted.length) return null;
  const position = (sorted.length - 1) * ratio;
  const lower = Math.floor(position);
  const upper = Math.ceil(position);
  if (lower === upper) return sorted[lower];
  const weight = position - lower;
  return sorted[lower] * (1 - weight) + sorted[upper] * weight;
}

function mergeObjects(target, source) {
  if (!source || typeof source !== "object" || Array.isArray(source)) return target;
  Object.entries(source).forEach(([key, value]) => {
    if (value && typeof value === "object" && !Array.isArray(value)) {
      const current = target[key];
      target[key] = mergeObjects(
        current && typeof current === "object" && !Array.isArray(current) ? current : {},
        value,
      );
    } else {
      target[key] = value;
    }
  });
  return target;
}

function observeSpeechLevel(volume, observedAt) {
  if (runtime.muted) return;
  const speech = runtime.speech;
  if (volume >= SPEECH_VOLUME_THRESHOLD) {
    speech.consecutiveSamples += 1;
    if (!speech.active && speech.consecutiveSamples >= SPEECH_START_SAMPLES) {
      speech.active = true;
      speech.speechStartAt = observedAt - (SPEECH_START_SAMPLES - 1) * METER_INTERVAL_MS;
    }
    if (speech.active) speech.lastVoiceAt = observedAt;
  } else if (!speech.active) {
    speech.consecutiveSamples = 0;
  }
}

// ==========================================================================
// HTTP API Client
// ==========================================================================

async function api(path, options = {}) {
  const headers = { ...(options.headers || {}) };
  if (!headers["Content-Type"] && options.body) {
    headers["Content-Type"] = "application/json";
  }
  if (runtime.accessToken) {
    headers.Authorization = `Bearer ${runtime.accessToken}`;
  }
  const response = await fetch(path, {
    credentials: "same-origin", cache: "no-store", signal: AbortSignal.timeout(15000),
    ...options, headers,
  });
  if (response.status === 204) return null;
  const contentType = response.headers.get("content-type") || "";
  const payload = contentType.includes("application/json")
    ? await response.json()
    : await response.text();
  if (!response.ok) {
    const message = payload?.error?.message || payload?.message || response.statusText;
    throw new Error(message);
  }
  return payload;
}

async function refreshStatus() {
  const request = ++runtime.statusRequest;
  try {
    const status = await api("/api/v1/status", { signal: AbortSignal.timeout(5000) });
    if (runtime.statusRequest !== request) return;
    runtime.accessProtected = Boolean(status.accessProtected);
    const bridgeOk = Boolean(status.bridgeOnline);
    runtime.bridgeOnline = bridgeOk;

    if (ui.navBridgeDot && ui.navBridgeLabel) {
      setDot(ui.navBridgeDot, bridgeOk ? "on" : "error");
      ui.navBridgeLabel.textContent = bridgeOk ? "Bridge 在线" : "Bridge 离线";
    }

    if (ui.bridgeState) {
      ui.bridgeState.textContent = bridgeOk ? "在线 (连接正常)" : "离线";
      ui.bridgeState.style.color = bridgeOk ? "#34d399" : "#f87171";
    }

    setDot(ui.bridgeDot, bridgeOk ? "on" : "");
    if (ui.modeBadge) {
      ui.modeBadge.textContent = status.demoMode ? "MOCK 演示模式" : "RTC 真实推流";
    }
    setRunning(Boolean(runtime.session && !runtime.starting && runtime.bridgeReady && runtime.rtcReady));
    if (!bridgeOk && runtime.session) {
      failSession("Bridge 已离线，请等待连接恢复后重试。");
    }
  } catch (error) {
    if (runtime.statusRequest !== request) return;
    runtime.bridgeOnline = null;
    if (ui.navBridgeLabel) ui.navBridgeLabel.textContent = "控制面异常";
    if (ui.bridgeState) ui.bridgeState.textContent = "无法连接";
    setDot(ui.navBridgeDot, "error");
    setDot(ui.bridgeDot, "error");
    setRunning(Boolean(runtime.session && !runtime.starting && runtime.bridgeReady && runtime.rtcReady));
  }
}

// ==========================================================================
// WebSocket & Event Handling
// ==========================================================================

function openEventSocket(wsPath) {
  const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  const socket = new WebSocket(`${protocol}//${window.location.host}${wsPath}`);
  runtime.socket = socket;

  socket.addEventListener("open", () => {
    if (runtime.socket !== socket) return;
    log("已连接控制面 WebSocket 事件流");
    setDot(ui.browserDot, "on");
  });

  socket.addEventListener("message", (event) => {
    if (runtime.socket !== socket) return;
    try {
      const payload = JSON.parse(event.data);
      handleServerEvent(payload);
    } catch {
      log("收到无法解析的消息", event.data);
    }
  });

  socket.addEventListener("close", (e) => {
    if (runtime.socket !== socket) return;
    log("WebSocket 断开", { code: e.code });
    setDot(ui.browserDot, "");
    if (runtime.session) failSession("控制面事件连接已断开，请重新开始会话。");
  });

  socket.addEventListener("error", () => {
    if (runtime.socket !== socket) return;
    setDot(ui.browserDot, "error");
  });
}

function handleServerEvent(event) {
  if (!runtime.session || event.sessionId !== runtime.session.sessionId) return;
  if (event.type === "session.snapshot") {
    if (event.bridgeOnline === false) {
      failSession("Bridge 已离线，请稍后重试。");
    } else if (event.state === "ready") {
      markReady();
    } else if (event.state === "error" || event.state === "closed") {
      failSession("会话启动失败或已结束，请重新开始。");
    }
    return;
  }
  if (event.type === "session.ready") {
    markReady();
    log("会话已就绪");
    return;
  }

  if (event.type === "asr.partial") {
    setDot(ui.asrDot, "busy");
    ui.partial.textContent = event.text || "";
    recordPartialEvent(event);
    return;
  }

  if (event.type === "asr.final") {
    setDot(ui.asrDot, "on");
    ui.partial.textContent = "";
    appendFinal(event.text, event.metrics);
    recordFinalEvent(event);
    return;
  }

  if (event.type === "trace.update") {
    recordTraceUpdate(event);
    return;
  }

  if (event.type === "asr.error") {
    if (runtime.starting || event.code === "bridge_offline" || event.code === "session_start_timeout") {
      failSession(event.message || "会话启动失败，请稍后重试。");
      return;
    }
    setDot(ui.asrDot, "error");
    appendFinal(`⚠ 识别错误: ${event.message || "未知异常"}`);
    log("识别异常", event);
    return;
  }

  if (event.type === "tts.started") {
    setDot(ui.asrDot, "busy");
    if (ui.ttsStatusText) {
      ui.ttsStatusText.textContent = `正在朗读 (${event.characters} 字)...`;
      ui.ttsStatusText.style.color = "#38bdf8";
    }
    return;
  }

  if (event.type === "tts.finished") {
    setDot(ui.asrDot, "on");
    if (ui.ttsStatusText) {
      ui.ttsStatusText.textContent = "朗读完成";
      ui.ttsStatusText.style.color = "#34d399";
    }
    return;
  }

  if (event.type === "tts.error") {
    setDot(ui.asrDot, "");
    if (ui.ttsStatusText) {
      ui.ttsStatusText.textContent = `朗读失败: ${event.message}`;
      ui.ttsStatusText.style.color = "#f87171";
    }
    appendFinal(`⚠ TTS 失败: ${event.message || "未知错误"}`);
    return;
  }

  if (event.type === "session.closed") {
    log("收到 session.closed 事件");
    if (runtime.starting) failSession("会话在就绪前已结束，请稍后重试。");
    else cleanupLocal("会话已结束");
    return;
  }

  if (event.type === "session.expired") {
    appendFinal("⚠ 会话已到期，请重新开始识别。");
    cleanupLocal("会话已到期");
  }
}

function markReady() {
  runtime.bridgeReady = true;
  if (!runtime.rtcReady) return;
  clearTimeout(runtime.startupTimer);
  runtime.startupTimer = null;
  runtime.starting = false;
  setSessionState("识别中");
  setDot(ui.asrDot, "on");
  setRunning(true);
}

function failSession(message) {
  const label = runtime.starting ? "启动失败" : "连接已中断";
  appendFinal(`⚠ ${message}`);
  log(message);
  void stop(label);
}

function armStartupTimeout(milliseconds, attempt) {
  clearTimeout(runtime.startupTimer);
  runtime.startupTimer = setTimeout(() => {
    if (runtime.startAttempt === attempt && runtime.starting) {
      failSession("会话启动超时：未能完成 RTC 连接或收到 Bridge 就绪确认，请稍后重试。");
    }
  }, milliseconds);
}

function appendFinal(text, metrics) {
  if (!text) return;
  ui.transcript.querySelector(".placeholder")?.remove();

  const item = document.createElement("div");
  item.className = "final-utterance";

  const meta = document.createElement("div");
  meta.className = "utterance-meta";
  const stamp = new Date().toLocaleTimeString();
  const latency = metrics?.bridge?.asrTotalMs ? `${Math.round(metrics.bridge.asrTotalMs)}ms` : "";
  meta.innerHTML = `<span>${stamp}</span><span>${latency ? `推理: ${latency}` : ""}</span>`;

  const content = document.createElement("div");
  content.className = "utterance-text";
  content.textContent = text;

  item.appendChild(meta);
  item.appendChild(content);
  ui.transcript.appendChild(item);
  item.scrollIntoView({ behavior: "smooth", block: "nearest" });
}

// ==========================================================================
// Observability & Latency Waterfall
// ==========================================================================

function recordPartialEvent(event) {
  const utteranceId = event.utteranceId;
  if (!utteranceId || runtime.firstPartialMs.has(utteranceId)) return;
  runtime.firstPartialMs.set(utteranceId, performance.now());
}

function recordFinalEvent(event) {
  const utteranceId = event.utteranceId;
  const receivedAt = performance.now();
  const trace = runtime.traces.get(utteranceId) || { utteranceId };
  trace.text = event.text || trace.text;
  trace.metrics = mergeObjects(trace.metrics || {}, event.metrics || {});
  trace.browser = trace.browser || {};
  trace.browser.receivedAtMs = receivedAt;

  if (runtime.manualCommitAt !== null) {
    trace.browser.manualCommitLagMs = roundMetric(Math.max(0, receivedAt - runtime.manualCommitAt));
    runtime.manualCommitAt = null;
  }

  const firstPartialAt = runtime.firstPartialMs.get(utteranceId);
  if (firstPartialAt) {
    trace.browser.finalAfterPartialMs = roundMetric(Math.max(0, receivedAt - firstPartialAt));
  }

  runtime.traces.set(utteranceId, trace);
  compileObservation(trace);
  renderObservability();

  // Send ACK over WebSocket
  if (runtime.socket && runtime.socket.readyState === WebSocket.OPEN) {
    runtime.socket.send(
      JSON.stringify({
        type: "client.result_ack",
        sessionId: runtime.session?.sessionId,
        utteranceId,
        eventType: "asr.final",
        seq: event.seq,
      })
    );
  }
}

function recordTraceUpdate(event) {
  const utteranceId = event.utteranceId;
  if (!utteranceId) return;
  const trace = runtime.traces.get(utteranceId) || { utteranceId };
  trace.metrics = mergeObjects(trace.metrics || {}, event.metrics || {});
  runtime.traces.set(utteranceId, trace);
  compileObservation(trace);
  renderObservability();
}

function compileObservation(trace) {
  const metrics = trace.metrics || {};
  const bridge = metrics.bridge || {};
  const agora = metrics.agora || {};
  const vps = metrics.vps || {};
  const browser = trace.browser || {};

  const asrTotal = asFiniteNumber(bridge.asrTotalMs);
  const endpoint = asFiniteNumber(bridge.endpointMs);
  const network = asFiniteNumber(agora.networkTransportDelayMs) || 0;
  const jitter = asFiniteNumber(agora.jitterBufferDelayMs) || 0;
  const vpsRelay = asFiniteNumber(vps.relayQueueMs) || 0;

  let totalEstimated = null;
  if (asrTotal !== null) {
    totalEstimated = asrTotal + (endpoint || 0) + network + jitter + vpsRelay;
  }

  const observation = {
    utteranceId: trace.utteranceId,
    text: trace.text,
    totalEstimatedMs: roundMetric(totalEstimated),
    stages: {
      agoraNetworkMs: roundMetric(network + jitter),
      endpointMs: roundMetric(endpoint),
      asrInferenceMs: roundMetric(asrTotal),
      vpsRelayMs: roundMetric(vpsRelay),
    },
    raw: trace,
  };

  const existingIdx = runtime.observations.findIndex((o) => o.utteranceId === trace.utteranceId);
  if (existingIdx >= 0) {
    runtime.observations[existingIdx] = observation;
  } else {
    runtime.observations.push(observation);
  }
}

function renderObservability() {
  const count = runtime.observations.length;
  ui.sampleCount.textContent = String(count);
  ui.exportMetrics.disabled = count === 0;
  ui.clearMetrics.disabled = count === 0;

  if (count === 0) {
    ui.latestLatency.textContent = "—";
    ui.p50Latency.textContent = "—";
    ui.p95Latency.textContent = "—";
    ui.traceText.textContent = "尚未收到最终识别结果。";
    ui.traceStages.innerHTML = '<p class="trace-empty">开始识别并产生语音后，此处将呈现各链路阶段的耗时分解。</p>';
    return;
  }

  const latencies = runtime.observations.map((o) => o.totalEstimatedMs).filter((v) => v !== null);
  const latest = runtime.observations[count - 1];

  ui.latestLatency.textContent = formatMs(latest.totalEstimatedMs);
  ui.p50Latency.textContent = formatMs(percentile(latencies, 0.5));
  ui.p95Latency.textContent = formatMs(percentile(latencies, 0.95));
  ui.traceText.textContent = latest.text || "—";

  if (runtime.networkQuality) {
    ui.networkSummary.textContent = `RTC 网络质量: 上行等级 ${runtime.networkQuality.uplinkNetworkQuality || "—"}, 下行等级 ${runtime.networkQuality.downlinkNetworkQuality || "—"}`;
  }

  // Render waterfall
  const stages = latest.stages;
  const stageDefs = [
    { key: "agoraNetworkMs", label: "RTC 网络传输", val: stages.agoraNetworkMs || 0 },
    { key: "endpointMs", label: "Bridge 断句端点", val: stages.endpointMs || 0 },
    { key: "asrInferenceMs", label: "OminiX ASR 推理", val: stages.asrInferenceMs || 0 },
    { key: "vpsRelayMs", label: "VPS 转发及交付", val: stages.vpsRelayMs || 0 },
  ];

  const total = stageDefs.reduce((acc, s) => acc + s.val, 0) || 1;
  ui.traceStages.innerHTML = stageDefs
    .map((s) => {
      const pct = Math.max(3, Math.round((s.val / total) * 100));
      return `
        <div class="stage-row">
          <span style="min-width: 110px;">${s.label}</span>
          <div class="stage-bar-bg"><div class="stage-bar-fill" style="width: ${pct}%;"></div></div>
          <span style="min-width: 50px; text-align: right; font-family: var(--font-mono);">${formatMs(s.val)}</span>
        </div>
      `;
    })
    .join("");
}

function exportObservations() {
  const jsonStr = JSON.stringify(runtime.observations, null, 2);
  const blob = new Blob([jsonStr], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = `octos-asr-trace-${new Date().toISOString().slice(0, 19).replace(/:/g, "-")}.json`;
  a.click();
  URL.revokeObjectURL(url);
}

function clearObservations() {
  runtime.observations = [];
  runtime.traces.clear();
  runtime.firstPartialMs.clear();
  renderObservability();
  log("已清空追踪监控数据");
}

// ==========================================================================
// Agora RTC & Audio Capture
// ==========================================================================

async function joinAgora(config, attempt) {
  if (!window.AgoraRTC) throw new Error("Agora Web SDK 加载失败");
  window.AgoraRTC.setLogLevel(2);
  const client = window.AgoraRTC.createClient({ mode: "live", codec: "vp8" });
  runtime.rtcClient = client;

  client.on("network-quality", (quality) => {
    runtime.networkQuality = quality;
  });

  client.on("user-published", async (user, mediaType) => {
    if (mediaType !== "audio") return;
    await client.subscribe(user, mediaType);
    user.audioTrack?.play();
    log("已订阅远端音频 (TTS 实时收听)", { uid: user.uid });
  });

  await client.setClientRole("host");
  await client.join(config.appId, config.channel, config.token, config.uid);

  const microphone = await window.AgoraRTC.createMicrophoneAudioTrack({
    encoderConfig: "speech_standard",
    AEC: true,
    ANS: true,
    AGC: true,
  });
  if (runtime.startAttempt !== attempt) {
    microphone.close();
    await client.leave().catch(() => {});
    return;
  }
  runtime.microphone = microphone;

  await client.publish([microphone]);
  if (runtime.startAttempt !== attempt) {
    microphone.close();
    await client.leave().catch(() => {});
    return;
  }
  setDot(ui.agoraDot, "on");
  startMeter();
  log("已加入 Agora RTC 并发布麦克风音轨", { channel: config.channel, uid: config.uid });
}

async function joinLivekit(config, attempt) {
  if (!window.LivekitClient) throw new Error("LiveKit SDK 加载失败");
  const LivekitClient = window.LivekitClient;
  const room = new LivekitClient.Room();
  runtime.room = room;

  room.on(LivekitClient.RoomEvent.TrackSubscribed, (track, publication, participant) => {
    if (track.kind !== "audio") return;
    const element = track.attach();
    element.addEventListener("canplay", () => element.play().catch(() => {}));
    document.body.appendChild(element);
    runtime.remoteAudioElement = element;
    log("已订阅远端音频 (TTS 实时收听)", { identity: participant?.identity });
  });

  room.on(LivekitClient.RoomEvent.Disconnected, () => {
    log("LiveKit 连接已断开");
    setDot(ui.agoraDot, "");
  });

  await room.connect(config.url, config.token);
  if (runtime.startAttempt !== attempt) {
    await room.disconnect().catch(() => {});
    return;
  }
  await room.localParticipant.setMicrophoneEnabled(true, {
    echoCancellation: true,
    noiseSuppression: true,
    autoGainControl: true,
  });
  if (runtime.startAttempt !== attempt) {
    await room.disconnect().catch(() => {});
    return;
  }
  setDot(ui.agoraDot, "on");
  startMeter();
  log("已加入 LiveKit 房间并发布麦克风", { room: config.room, identity: config.identity });
}

function startMeter() {
  clearInterval(runtime.meterTimer);
  ui.level.classList.add("active");
  runtime.meterTimer = setInterval(() => {
    const volume =
      runtime.microphone?.getVolumeLevel?.() ??
      runtime.room?.localParticipant?.audioLevel ??
      0;
    observeSpeechLevel(volume, performance.now());
    ui.levelBars.forEach((bar, index) => {
      const threshold = index / ui.levelBars.length;
      bar.style.height = `${4 + (volume > threshold ? 18 * Math.min(1, volume + 0.25) : 0)}px`;
    });
  }, METER_INTERVAL_MS);
}

// ==========================================================================
// Session Actions
// ==========================================================================

async function start() {
  if (runtime.starting || runtime.session) return;
  if (runtime.bridgeOnline !== true) {
    appendFinal("⚠ Bridge 离线或状态未知，请等待服务恢复后重试。");
    void refreshStatus();
    return;
  }
  runtime.accessToken = localStorage.getItem(STORAGE_KEY) || ui.accessKey.value.trim();
  if (runtime.accessProtected && !runtime.accessToken) {
    openTokenModal();
    appendFinal("⚠ 请先在弹窗中配置管理员提供的团队访问密钥。");
    return;
  }

  const attempt = ++runtime.startAttempt;
  runtime.starting = true;
  runtime.bridgeReady = false;
  runtime.rtcReady = false;
  setRunning(false);
  armStartupTimeout(30000, attempt);
  runtime.speech = createSpeechState();
  runtime.manualCommitAt = null;
  runtime.networkQuality = null;
  setSessionState("正在启动...");
  setDot(ui.agoraDot, "busy");

  try {
    const session = await api("/api/v1/sessions", { method: "POST", body: "{}" });
    if (runtime.startAttempt !== attempt) {
      await api(`/api/v1/sessions/${session.sessionId}`, { method: "DELETE" }).catch(() => {});
      return;
    }
    runtime.session = session;
    armStartupTimeout((session.sessionStartTimeoutSeconds || 30) * 1000 + 2000, attempt);
    ui.sessionId.textContent = session.sessionId.slice(0, 8);
    ui.sessionId.title = session.sessionId;

    openEventSocket(session.eventsWsPath);

    if (session.demoMode) {
      setDot(ui.agoraDot, "on");
      log("MOCK 模式：跳过浏览器麦克风与 Agora 入会");
    } else if (session.livekit) {
      await joinLivekit(session.livekit, attempt);
    } else {
      await joinAgora(session.agora, attempt);
    }

    if (runtime.startAttempt !== attempt) return;
    runtime.rtcReady = true;
    if (runtime.bridgeReady) markReady();
  } catch (error) {
    if (runtime.startAttempt !== attempt) return;
    log("启动会话失败", { error: error.message });
    appendFinal(`⚠ 启动失败: ${error.message}`);
    setSessionState("启动失败");
    setDot(ui.agoraDot, "error");
    // 服务端把不认识的 Bearer 当一次性 grant 处理；本地存的密钥过期或
    // 输错时会报 "browser grant invalid"。此时清掉缓存密钥并重新索要。
    if (/browser grant/i.test(error.message)) {
      runtime.accessToken = "";
      localStorage.removeItem(STORAGE_KEY);
      sessionStorage.removeItem("asrAccessToken");
      updateTokenUI();
      appendFinal("⚠ 已保存的访问密钥无效，请重新配置。");
      openTokenModal();
    }
    await stop("启动失败");
  }
}

async function toggleMute() {
  runtime.muted = !runtime.muted;
  if (runtime.room) {
    await runtime.room.localParticipant.setMicrophoneEnabled(!runtime.muted);
  } else if (runtime.microphone) {
    await runtime.microphone.setEnabled(!runtime.muted);
  }
  ui.mute.textContent = runtime.muted ? "取消静音" : "静音";
  ui.level.classList.toggle("active", !runtime.muted);
  log(runtime.muted ? "麦克风已静音" : "麦克风已取消静音");
}

async function commit() {
  if (!runtime.session) return;
  const committedAt = performance.now();
  runtime.manualCommitAt = committedAt;
  try {
    await api(`/api/v1/sessions/${runtime.session.sessionId}/commit`, {
      method: "POST",
      body: "{}",
    });
    log("已触发手动断句");
  } catch (error) {
    if (runtime.manualCommitAt === committedAt) runtime.manualCommitAt = null;
    log("断句请求失败", { error: error.message });
  }
}

async function speak() {
  const text = ui.ttsText.value.trim();
  if (!runtime.session || !text) return;

  let voice = ui.ttsVoiceSelect.value;
  if (voice === "custom") {
    voice = ui.ttsVoiceCustom.value.trim();
  }

  const speed = parseFloat(ui.ttsSpeed.value) || 1.0;
  const instruct = ui.ttsInstruct.value.trim();

  const payload = { text };
  if (voice) payload.voice = voice;
  if (speed !== 1.0) payload.speed = speed;
  if (instruct) payload.instruct = instruct;

  try {
    if (ui.ttsStatusText) {
      ui.ttsStatusText.textContent = "已发送朗读请求...";
      ui.ttsStatusText.style.color = "#38bdf8";
    }
    await api(`/api/v1/sessions/${runtime.session.sessionId}/speak`, {
      method: "POST",
      body: JSON.stringify(payload),
    });
    log("已请求 TTS 朗读", payload);
  } catch (error) {
    log("TTS 朗读失败", { error: error.message });
    if (ui.ttsStatusText) {
      ui.ttsStatusText.textContent = `朗读错误: ${error.message}`;
      ui.ttsStatusText.style.color = "#f87171";
    }
  }
}

async function stop(label = "待机") {
  const session = runtime.session;
  await cleanupLocal(label);
  if (session) {
    try {
      await api(`/api/v1/sessions/${session.sessionId}`, { method: "DELETE" });
    } catch (error) {
      log("结束会话失败", { error: error.message });
    }
  }
}

async function cleanupLocal(label = "待机") {
  ++runtime.startAttempt;
  clearTimeout(runtime.startupTimer);
  runtime.startupTimer = null;
  runtime.starting = false;
  runtime.bridgeReady = false;
  runtime.rtcReady = false;
  clearInterval(runtime.meterTimer);
  runtime.meterTimer = null;
  runtime.microphone?.stop();
  runtime.microphone?.close();
  const rtcClient = runtime.rtcClient;
  const room = runtime.room;
  runtime.remoteAudioElement?.remove();
  const socket = runtime.socket;
  runtime.session = null;
  runtime.socket = null;
  runtime.rtcClient = null;
  runtime.room = null;
  runtime.remoteAudioElement = null;
  runtime.microphone = null;
  runtime.muted = false;
  runtime.speech = createSpeechState();
  runtime.networkQuality = null;
  runtime.manualCommitAt = null;

  ui.mute.textContent = "静音";
  ui.level.classList.remove("active");
  ui.levelBars.forEach((bar) => { bar.style.height = "4px"; });
  ui.partial.textContent = "";
  ui.sessionId.textContent = "—";
  setSessionState(label);
  setDot(ui.browserDot, "");
  setDot(ui.agoraDot, "");
  setDot(ui.asrDot, "");
  setRunning(false);
  socket?.close();
  void refreshStatus();
  if (rtcClient) await rtcClient.leave().catch(() => {});
  if (room) await room.disconnect().catch(() => {});
}

// ==========================================================================
// Event Listeners & Boot
// ==========================================================================

ui.start.addEventListener("click", start);
ui.mute.addEventListener("click", toggleMute);
ui.commit.addEventListener("click", commit);
ui.speak.addEventListener("click", speak);
ui.ttsText.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    speak();
  }
});
ui.stop.addEventListener("click", () => stop());
ui.exportMetrics.addEventListener("click", exportObservations);
ui.clearMetrics.addEventListener("click", clearObservations);

// 内联 onclick 会被 CSP(script-src 无 'unsafe-inline')拦截,全部改为事件绑定
document.querySelector("#ctaPlaygroundBtn")?.addEventListener("click", () => switchTab("playground"));
document.querySelector("#ctaDocsBtn")?.addEventListener("click", () => switchTab("docs"));
document.querySelectorAll(".preset-pill").forEach((pill) => {
  pill.addEventListener("click", () => setTtsSample(pill.dataset.sample));
});
document.querySelector("#tokenModalSave")?.addEventListener("click", saveTokenAndClose);
document.querySelector("#tokenModalCancel")?.addEventListener("click", closeTokenModal);
document.querySelector("#tokenModalClose")?.addEventListener("click", closeTokenModal);
ui.tokenModal?.addEventListener("click", (event) => {
  if (event.target === ui.tokenModal) closeTokenModal();
});

// 文档侧边栏:页内平滑滚动,不占用 hash 路由
document.querySelectorAll(".sidebar-link").forEach((link) => {
  link.addEventListener("click", (event) => {
    event.preventDefault();
    const target = document.querySelector(link.getAttribute("href"));
    target?.scrollIntoView({ behavior: "smooth", block: "start" });
    document
      .querySelectorAll(".sidebar-link")
      .forEach((item) => item.classList.toggle("active", item === link));
  });
});

window.addEventListener("pagehide", () => {
  runtime.microphone?.close();
  runtime.room?.disconnect();
  if (!runtime.session) return;
  const headers = runtime.accessToken ? { Authorization: `Bearer ${runtime.accessToken}` } : {};
  fetch(`/api/v1/sessions/${runtime.session.sessionId}`, {
    method: "DELETE",
    headers,
    keepalive: true,
  }).catch(() => {});
});

// Initial boot
handleHashChange();
setRunning(false);
refreshStatus();
renderObservability();
setInterval(refreshStatus, 5000);

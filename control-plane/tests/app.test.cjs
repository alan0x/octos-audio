const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "../static/app.js"), "utf8");

function harness() {
  const elements = new Map();
  const timers = new Map();
  const requests = [];
  let timerId = 0;
  let status = { bridgeOnline: true, accessProtected: false, demoMode: true };
  const session = { sessionId: "test-session", eventsWsPath: "/ws/client/test-session", demoMode: true };
  function element() {
    return {
      value: "", textContent: "", style: {}, dataset: {}, disabled: false,
      classList: { add() {}, remove() {}, toggle() {} },
      listeners: {}, addEventListener(name, callback) { this.listeners[name] = callback; },
      setAttribute() {}, focus() {}, remove() {}, appendChild() {}, scrollIntoView() {},
      querySelector() { return null; }, querySelectorAll() { return []; },
    };
  }
  class Socket {
    static OPEN = 1;
    constructor() { this.listeners = {}; this.readyState = 1; }
    addEventListener(name, callback) { this.listeners[name] = callback; }
    close() { this.listeners.close?.({ code: 1000 }); }
    send() {}
    emit(event) { this.listeners.message({ data: JSON.stringify(event) }); }
  }
  const storage = { getItem() { return null; }, setItem() {}, removeItem() {} };
  const context = vm.createContext({
    document: {
      querySelector(selector) {
        if (!elements.has(selector)) elements.set(selector, element());
        return elements.get(selector);
      },
      querySelectorAll() { return []; }, createElement: element,
    },
    window: { location: { protocol: "http:", host: "localhost", hash: "" }, addEventListener() {}, scrollTo() {} },
    localStorage: storage, sessionStorage: storage,
    WebSocket: Socket, AbortSignal, performance,
    setTimeout(callback) { timers.set(++timerId, callback); return timerId; },
    clearTimeout(id) { timers.delete(id); }, setInterval() {}, clearInterval() {},
    fetch: async (url, options) => {
      requests.push({ url, options });
      const payload = url === "/api/v1/status" ? status : session;
      return { ok: true, status: options?.method === "DELETE" ? 204 : 200,
        headers: { get() { return "application/json"; } }, json: async () => payload };
    },
  });
  // Leave the real event handlers installed; only skip the page's initial boot.
  vm.runInContext(source.slice(0, source.indexOf("// Initial boot")) + `
    globalThis.messages = [];
    appendFinal = (text) => messages.push(text);
    globalThis.app = { runtime, ui, refreshStatus, start, stop, handleServerEvent,
      markReady, setRunning, armStartupTimeout };
  `, context);
  return { context, app: context.app, timers, requests, session, setStatus(value) { status = value; } };
}

const flush = () => new Promise(setImmediate);

test("unknown or offline Bridge disables start and prevents session allocation", async () => {
  const h = harness();
  h.app.setRunning(false);
  assert.equal(h.app.ui.start.disabled, true);
  h.setStatus({ bridgeOnline: false, accessProtected: false });
  await h.app.refreshStatus();
  await h.app.start();
  assert.equal(h.app.ui.start.disabled, true);
  assert.equal(h.app.ui.navBridgeLabel.textContent, "Bridge 离线");
  assert.equal(h.requests.some((request) => request.options.method === "POST"), false);
});

test("RTC success waits for Bridge readiness and accepts a ready snapshot", async () => {
  const h = harness();
  await h.app.refreshStatus();
  await h.app.start();
  assert.equal(h.app.ui.sessionState.textContent, "正在启动...");
  assert.equal(h.app.ui.mute.disabled, true);
  assert.equal(h.app.ui.stop.disabled, false);
  h.app.runtime.socket.emit({ type: "session.snapshot", sessionId: h.session.sessionId, state: "ready", bridgeOnline: true });
  assert.equal(h.app.ui.sessionState.textContent, "识别中");
  assert.equal(h.app.ui.mute.disabled, false);
  assert.equal(h.app.runtime.startupTimer, null);
});

test("Bridge readiness before RTC completion does not enable recognition early", async () => {
  const h = harness();
  let completeRtc;
  h.session.demoMode = false;
  h.session.livekit = {};
  h.context.connectRtc = () => new Promise((resolve) => { completeRtc = resolve; });
  vm.runInContext("joinLivekit = connectRtc", h.context);
  await h.app.refreshStatus();
  const starting = h.app.start();
  await flush();
  h.app.runtime.socket.emit({ type: "session.ready", sessionId: h.session.sessionId });
  assert.equal(h.app.ui.mute.disabled, true);
  assert.equal(h.app.runtime.starting, true);
  completeRtc();
  await starting;
  assert.equal(h.app.ui.sessionState.textContent, "识别中");
  assert.equal(h.app.ui.mute.disabled, false);
});

test("startup timeout cleans local state, releases server session and shows failure", async () => {
  const h = harness();
  await h.app.refreshStatus();
  await h.app.start();
  h.app.runtime.socket.listeners.open();
  h.timers.get(h.app.runtime.startupTimer)();
  await flush();
  assert.equal(h.app.runtime.session, null);
  assert.equal(h.app.runtime.starting, false);
  assert.equal(h.app.ui.sessionState.textContent, "启动失败");
  assert.equal(h.app.ui.start.disabled, false);
  assert.equal(h.app.ui.mute.disabled, true);
  assert.equal(h.app.ui.browserDot.className, "status-dot");
  assert.ok(h.requests.some((request) => request.options.method === "DELETE"));
  assert.ok(h.context.messages.some((message) => message.includes("会话启动超时")));
});

test("an offline poll clears an active session and both online indicators", async () => {
  const h = harness();
  await h.app.refreshStatus();
  await h.app.start();
  h.app.handleServerEvent({ type: "session.ready", sessionId: h.session.sessionId });
  h.setStatus({ bridgeOnline: false, accessProtected: false });
  await h.app.refreshStatus();
  await flush();
  assert.equal(h.app.runtime.session, null);
  assert.equal(h.app.ui.navBridgeLabel.textContent, "Bridge 离线");
  assert.equal(h.app.ui.bridgeState.textContent, "离线");
  assert.equal(h.app.ui.start.disabled, true);
  assert.equal(h.app.ui.sessionState.textContent, "连接已中断");
});

test("server startup errors stop waiting and preserve the failure label", async () => {
  const h = harness();
  await h.app.refreshStatus();
  await h.app.start();
  h.app.handleServerEvent({ type: "asr.error", sessionId: h.session.sessionId,
    code: "session_start_timeout", message: "Bridge 未就绪" });
  await flush();
  assert.equal(h.app.ui.sessionState.textContent, "启动失败");
  assert.equal(h.app.runtime.session, null);
  assert.equal(h.context.messages.length, 1);
});

test("stale event-socket close and messages cannot affect a replacement session", async () => {
  const h = harness();
  await h.app.refreshStatus();
  await h.app.start();
  const previous = h.app.runtime.socket;
  await h.app.stop();
  await h.app.start();
  const current = h.app.runtime.socket;
  previous.listeners.close({ code: 1006 });
  previous.emit({ type: "session.closed", sessionId: h.session.sessionId });
  assert.equal(h.app.runtime.socket, current);
  assert.equal(h.app.runtime.starting, true);
});

test("stopping during RTC connection prevents late success from restarting UI", async () => {
  const h = harness();
  let completeRtc;
  h.session.demoMode = false;
  h.session.livekit = {};
  h.context.connectRtc = () => new Promise((resolve) => { completeRtc = resolve; });
  vm.runInContext("joinLivekit = connectRtc", h.context);
  await h.app.refreshStatus();
  const starting = h.app.start();
  await flush();
  await h.app.stop();
  completeRtc();
  await starting;
  assert.equal(h.app.ui.sessionState.textContent, "待机");
  assert.equal(h.app.ui.mute.disabled, true);
  assert.equal(h.app.runtime.session, null);
});

test("status fetch failure clears previously green indicators and disables start", async () => {
  const h = harness();
  await h.app.refreshStatus();
  h.context.fetch = async () => { throw new Error("network unavailable"); };
  await h.app.refreshStatus();
  assert.equal(h.app.ui.navBridgeLabel.textContent, "控制面异常");
  assert.equal(h.app.ui.navBridgeDot.className, "status-dot error");
  assert.equal(h.app.ui.start.disabled, true);
  assert.equal(h.app.runtime.bridgeOnline, null);
});

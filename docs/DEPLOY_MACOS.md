# 在新 Mac 上部署推理端（OminiX + Bridge）

本文档描述如何在一台新的 Apple Silicon Mac 上部署本项目的**推理端**：OminiX-API
（Qwen3-ASR / Qwen3-TTS）和 Bridge。控制面（`control-plane/`）运行在 VPS 上，
本文档假定它已部署且保持不变；它的部署见 `deploy/` 下的 Compose 与 Nginx 示例。

文中所有域名、端口、密钥均为占位符（`asr.example.com`、`rtc.example.com` 等），
真实值保存在不入库的私有文档中。

## 0. 拓扑与边界

```text
浏览器 ──RTC──> LiveKit / Agora ──16 kHz PCM──> 本机 Bridge ──HTTP──> 本机 OminiX ASR (8080…)
                                                   │                  本机 OminiX TTS (8090)
                                                   └── 出站 WSS ──> VPS 控制面 (wss://asr.example.com/ws/bridge)
```

- 推理端**不需要任何公网入站端口**。Bridge 只做出站连接：VPS 控制面（443）以及
  RTC 层（LiveKit 的信令与媒体端口，或 Agora）。
- **控制面同一时刻只接受一个 Bridge**。新的 Bridge 连上会顶掉旧的；被顶掉的旧
  Bridge 会按退避策略自动重连，又把新的顶掉，两台 Mac 会来回抢占。**迁移时必须先
  停掉旧 Mac 上的 Bridge**，见第 8 节。

## 1. 参考环境快照

当前生产 Mac 的实际环境如下，可作为对照：

| 项目 | 当前值 |
|---|---|
| 硬件 | Apple M5 Max，128 GB |
| 系统 | macOS 26.6 |
| Xcode | 完整 Xcode（`/Applications/Xcode.app`）+ Metal Toolchain |
| Rust | 1.98（Homebrew） |
| protoc | Homebrew `protobuf` |
| Python | Homebrew `python@3.13`（3.13.15），Bridge 使用 `bridge/.venv` |
| OminiX-API | 源码构建，`v1.2.0+360a073`，路径 `~/Documents/projects/OminiX-API` |
| OminiX-MLX | `4988a3f`，路径 `~/Documents/projects/OminiX-MLX`（OminiX-API 的路径依赖） |
| MLX | 0.32.0，由源码编译（ASR batching 生效） |
| ASR 模型 | `~/.OminiX/models/Qwen3-ASR-1.7B-4bit` |
| TTS 模型 | `~/.OminiX/models/Qwen3-TTS-12Hz-1.7B-CustomVoice-8bit` |
| ASR 实例 | 3 个（8080 / 8081 / 8082），`ASR_MODE=conversational` |
| TTS 实例 | 1 个（8090） |
| RTC | LiveKit（`RTC_PROVIDER=livekit`，在 VPS 端配置） |
| 进程管理 | 手动在终端中运行（未使用 launchd） |
| 休眠 | `pmset sleep 0`（系统不休眠） |

## 2. 选择规格

| 规格 | ASR | TTS | 适用 |
|---|---|---|---|
| **单路（最低配置）** | 1 个实例 | 1 个实例 | 性能较弱的 Mac、内存 ≤ 32 GB |
| 实例池 | N 个实例 | 1 个实例 | 高并发，参考机为 3 个 |

两种规格都**保持 ASR 与 TTS 是两个独立进程**。OminiX 每个进程只有一个推理线程，
如果 ASR 和 TTS 共用一个实例，一次长文本合成会把该实例上所有会话的识别卡住。

单路规格下需要知道的限制：

- **ASR**：所有会话都路由到这一个实例。开启 batching 时（需要 MLX ≥ 0.32.0，见
  3.3 节），多个会话的请求会合并为一次解码；未开启时请求逐个排队。可承载的并发
  会话数取决于芯片，**必须在目标机器上实测**（第 7 节），再据此设置容量上限。
  上游在 M5 Max 上测得单进程约 70 路（batching）/ 约 40 路（无 batching），
  弱一些的芯片会低得多。
- **TTS**：多个会话同时请求朗读时，按请求顺序串行合成。Bridge 会把长文本切成
  ≤ 40 字的分句逐句请求，因此两个会话同时朗读时会互相拖慢。TTS 与 ASR 共享同一块
  GPU，合成期间识别延迟也会上升。
- 内存：ASR 模型约 1.5 GB，TTS 8-bit 模型约 2.9 GB（磁盘大小，运行时还有额外开销）。
  内存紧张时可改用 `Qwen3-TTS-12Hz-1.7B-CustomVoice-4bit`（约 2.2 GB）。注意：
  当前生产验证的是 8-bit 版本，4-bit 版本的音质需自行确认。

## 3. 安装 OminiX-API

### 3.1 前置依赖

```bash
# Xcode：从 App Store 安装完整 Xcode（不是只装 Command Line Tools），然后：
sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
xcodebuild -downloadComponent MetalToolchain   # 较新的 Xcode 需单独下载 Metal 工具链
xcrun -sdk macosx metal --version              # 必须能输出版本号

# Homebrew 依赖
brew install rust protobuf cmake python@3.13
```

> 为什么要完整 Xcode：OminiX-MLX 的构建脚本检测到 Metal 编译器时，会从源码编译
> MLX 0.32.0；检测不到时会退回下载预编译的 MLX 0.30.1。旧版 MLX 有 RoPE 缺陷，
> OminiX 检测到后会**自动关闭 ASR batching**：识别结果仍然正确，但并发能力明显下降。
> 单路规格尤其依赖 batching。

### 3.2 源码构建（与参考环境一致）

两个仓库必须放在同一个父目录下（OminiX-API 通过 `../OminiX-MLX` 引用它）。放在
`~/Documents/projects/` 下时，本项目的启动脚本能自动找到二进制文件，无需设置
`OMINIX_BIN`。

```bash
mkdir -p ~/Documents/projects && cd ~/Documents/projects
git clone https://github.com/OminiX-ai/OminiX-API.git
git clone https://github.com/OminiX-ai/OminiX-MLX.git
git -C OminiX-API checkout 360a073    # 与参考环境一致；升级前请先在测试机验证
git -C OminiX-MLX checkout 4988a3f
cd OminiX-API && cargo build --release
```

产物位于 `~/Documents/projects/OminiX-API/target/release/ominix-api`，
`mlx.metallib` 也在同一目录下，**不要单独拷走二进制文件**。

> 也可以用官方 `install.sh` 安装预编译版本（安装到 `/usr/local/bin`），但其版本和
> MLX 版本与参考环境不同，需要按 3.3 节确认 batching 是否生效。

### 3.3 确认 batching 生效

第 5 节启动 ASR 后，检查日志：

```bash
grep -a "ASR batching" bridge/logs/ominix/ominix-8080.log
```

- 期望看到：`ASR batching: mode=conversational max_batch=8 window=25ms`
- 如果看到 `ASR batching disabled: this MLX build mis-handles RoPE ...`，说明 MLX
  版本过旧，回到 3.1 节检查 Xcode 和 Metal 工具链，然后执行 `cargo clean` 再重新构建。

## 4. 下载模型

```bash
cd ~/Documents/projects/agora-sensevoice-demo/bridge
/opt/homebrew/opt/python@3.13/bin/python3.13 -m venv .venv
.venv/bin/pip install -r requirements.txt huggingface_hub

# 网络受限时可先设置镜像：export HF_ENDPOINT=https://hf-mirror.com
.venv/bin/hf download mlx-community/Qwen3-ASR-1.7B-4bit \
  --local-dir ~/.OminiX/models/Qwen3-ASR-1.7B-4bit
.venv/bin/hf download mlx-community/Qwen3-TTS-12Hz-1.7B-CustomVoice-8bit \
  --local-dir ~/.OminiX/models/Qwen3-TTS-12Hz-1.7B-CustomVoice-8bit
```

`huggingface_hub` 只用于下载模型，不是 Bridge 的运行依赖。

> **注意**：OminiX 会扫描 `~/.OminiX/models/`，并在**每个**实例启动时预加载其中的
> TTS 模型，包括只做 ASR 的实例。参考环境的 3 个 ASR 实例都各自加载了一份 TTS。
> 在 ASR 实例上设置 `TTS_LAZY_LOAD=1` 可以避免这份浪费（第 5 节的命令已包含）。

## 5. 启动 OminiX

以下命令都在 `bridge/` 目录下执行。每个命令占用一个终端，保持运行。

通用环境变量：

- `OMINIX_API_HOST=127.0.0.1`：OminiX 默认监听 `0.0.0.0`，会暴露给局域网。Bridge
  与 OminiX 在同一台机器上，绑定回环地址即可。参考环境未设置这个变量，新机器
  建议加上。
- `TTS_LAZY_LOAD=1`：仅用于 ASR 实例，原因见第 4 节。

**ASR，单路：**

```bash
OMINIX_API_HOST=127.0.0.1 TTS_LAZY_LOAD=1 OMINIX_POOL_SIZE=1 bash start-ominix-pool.sh
```

**ASR，实例池（参考环境为 3 个）：**

```bash
OMINIX_API_HOST=127.0.0.1 TTS_LAZY_LOAD=1 OMINIX_POOL_SIZE=3 bash start-ominix-pool.sh
```

实例池从 8080 开始占用连续端口，日志写入 `bridge/logs/ominix/ominix-<port>.log`。
按 Ctrl+C 会停止池内全部实例。

**TTS（单个实例，端口 8090）：**

```bash
OMINIX_API_HOST=127.0.0.1 bash start-ominix-tts.sh
# 使用 4-bit 模型时：
# TTS_MODEL_DIR=~/.OminiX/models/Qwen3-TTS-12Hz-1.7B-CustomVoice-4bit OMINIX_API_HOST=127.0.0.1 bash start-ominix-tts.sh
```

TTS 日志直接输出到终端。

> `start-ominix-asr.sh`（单实例 `exec`）只在 `PATH` 中查找 `ominix-api`，不会查找
> 源码构建的路径。源码构建时请使用 `start-ominix-pool.sh` 并设置 `OMINIX_POOL_SIZE=1`，
> 或者为前者设置 `OMINIX_BIN`。

## 6. 配置并启动 Bridge

```bash
cd bridge
cp .env.example .env
```

需要修改的配置项：

| 变量 | 说明 |
|---|---|
| `CONTROL_WS_URL` | `wss://<控制面域名>/ws/bridge`，与旧 Mac 相同 |
| `BRIDGE_SHARED_SECRET` | 与 VPS `deploy/.env` 中的值**完全一致**，与旧 Mac 相同 |
| `ASR_URL` / `ASR_URLS` | 单路：只保留 `ASR_URL=http://127.0.0.1:8080/v1/audio/transcriptions`，**删除或注释 `ASR_URLS`**。实例池：`ASR_URLS` 列出全部端口，数量与 `OMINIX_POOL_SIZE` 一致 |
| `ASR_HEALTH_URL` | `http://127.0.0.1:8080/health`（设置了 `ASR_URLS` 时会被忽略，改为逐个检查池内实例） |
| `BRIDGE_MAX_SESSIONS` | 本机能承载的并发会话上限，根据第 7 节的实测结果设置 |
| `TTS_URL` | `http://127.0.0.1:8090/v1/audio/speech`；留空则关闭朗读功能 |
| `TTS_VOICE` | 预设音色，**必须全部小写**（如 `serena`；写成 `Vivian` 会合成失败） |

其余配置项保持 `.env.example` 中的默认值即可。参考环境只显式设置了上表中的变量
以及 `ASR_PROTOCOL`、`ASR_LANGUAGE`、`ASR_MODEL`、`TTS_SPEED`、`TTS_LANGUAGE`、
`LOG_LEVEL`、`VAD_THRESHOLD_DBFS`，其余 TTS 参数使用代码中的默认值，与
`.env.example` 一致。

`.env` 含密钥，已被 gitignore 忽略。请用安全渠道（AirDrop、密码管理器）从旧 Mac
拷贝，或者新建后填入。

启动：

```bash
bash start-real.sh
```

`start-real.sh` 会检查 `.env` 是否仍是示例值，然后等待所有 ASR 实例的 `/health`
就绪（最多等待 `ASR_STARTUP_WAIT_SECONDS`），最后启动 Bridge。日志中出现
`bridge connected in real mode` 即表示已连上控制面。

> Bridge 的 WebSocket 连接会忽略系统代理（`HTTPS_PROXY` 等），直连控制面。

## 7. 验证与容量实测

**生成测试音频**（使用 macOS 自带工具，任选一个中文语音）：

```bash
say -v Tingting "今天天气很好，我们来测试语音识别。" -o /tmp/t.aiff
afconvert -f WAVE -d LEI16@16000 -c 1 /tmp/t.aiff /tmp/test-16k.wav
```

**健康检查与单次识别：**

```bash
curl -s http://127.0.0.1:8080/health     # {"status":"healthy",...}
curl -s http://127.0.0.1:8090/health
.venv/bin/python smoke_sensevoice_live.py /tmp/test-16k.wav   # 输出识别文本
```

**TTS 单次合成：**

```bash
curl -s -o /tmp/tts.wav http://127.0.0.1:8090/v1/audio/speech \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen3-tts","input":"你好，部署测试。","voice":"serena","language":"chinese","response_format":"wav"}'
afplay /tmp/tts.wav
```

**ASR 并发压测**（确定 `BRIDGE_MAX_SESSIONS` 和 VPS 端 `SESSION_CAPACITY` 的依据）：

```bash
# 单路
.venv/bin/python stress_asr_pool.py /tmp/test-16k.wav \
  --urls http://127.0.0.1:8080/v1/audio/transcriptions \
  --levels 2,5,10,20,35 --level-seconds 60
```

取 p50 延迟仍可接受（参考值：约 500 ms 以内）的最高并发档位，再留出余量作为容量
上限。压测时请不要让线上会话连到这台机器。

**端到端验收**：打开控制面页面，开始识别并说话，确认能看到识别文字；然后测试一次
朗读，确认能听到声音。

## 8. 从旧 Mac 迁移（切换步骤）

1. 在新 Mac 上完成第 3–7 节，**暂不启动 Bridge**（OminiX 可以先启动）。
2. 如果新 Mac 的容量低于旧 Mac，先在 VPS 上调低 `deploy/.env` 中的
   `SESSION_CAPACITY`（不超过新 Mac 的 `BRIDGE_MAX_SESSIONS`），然后重建控制面容器：
   ```bash
   docker compose --env-file deploy/.env -f deploy/docker-compose.yml up -d
   ```
3. 在旧 Mac 上停止 Bridge（在运行 `start-real.sh` 的终端按 Ctrl+C）。
4. 在新 Mac 上执行 `bash start-real.sh`。
5. 验证 VPS 就绪状态：`curl https://asr.example.com/readyz` 返回 200，再做一次端到端验收。
6. 确认无误后，再停止旧 Mac 上的 OminiX 进程。

回滚：停止新 Mac 的 Bridge，再在旧 Mac 上启动 Bridge。

## 9. 常驻运行

参考环境是在终端中手动运行三个进程（ASR、TTS、Bridge），并已关闭系统休眠。新机器
至少需要做到：

```bash
sudo pmset -a sleep 0          # 系统不休眠（屏幕可以休眠）
sudo pmset -a autorestart 1    # 断电恢复后自动开机（可选）
```

可以用 `tmux` 保持三个进程在后台运行。如需开机自启，可以为 ASR 实例池、TTS 和
Bridge 各写一个 LaunchAgent（`~/Library/LaunchAgents/*.plist`，`WorkingDirectory`
设为 `bridge/`，`KeepAlive` 设为 true）。参考环境没有使用 launchd，这部分没有经过
验证。

启动顺序没有强依赖：`start-real.sh` 会等待 ASR 就绪；TTS 未就绪时只影响朗读；
Bridge 与控制面断线后会自动重连。

## 10. 常见问题

| 现象 | 原因与处理 |
|---|---|
| `start-real.sh` 报 `Missing bridge/.venv` | 按第 4 节创建 venv |
| `ASR health check failed` | OminiX 未启动，或端口与 `ASR_URL(S)` 不一致；查看 `logs/ominix/` |
| `ominix-api binary not found` | 二进制不在 `~/Documents/projects/OminiX-API/target/release/`，也不在 `PATH` 中；设置 `OMINIX_BIN` |
| `Qwen3-ASR model not found` | 模型目录不对，或者缺少 `config.json`；按第 4 节重新下载 |
| 页面提示"内网 Bridge 已重新连接"，并且反复出现 | 有两个 Bridge 在争抢连接；停掉其中一个 |
| Bridge 日志 `bridge at capacity` | 达到 `BRIDGE_MAX_SESSIONS` 上限 |
| 识别延迟随并发急剧上升 | 超出容量，或 batching 未生效（3.3 节） |
| 朗读无声，OminiX 日志 `Unknown speaker 'Xxx'` | 音色名称大小写错误，改为小写 |
| 朗读时识别变慢 | 正常现象：TTS 与 ASR 共享 GPU；单路规格下更明显 |
| `pip install` 失败 | 确认 venv 使用的是 Homebrew Python 3.13（与参考环境一致） |

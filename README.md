# NRL Pulse

业余无线电 / 应急通信桌面客户端。同时支持 **NRL2**（UDP / G.711、Opus）与 **FMO**（MQTT / APRS 服务器发现、SAS 证书认证、Opus / IMA ADPCM）两套协议，全双工语音调度。支持 Windows、macOS、Linux。

![NRL Pulse](src-tauri/icons/icon-128.png)

---

## 功能

### 语音通信

- **双协议支持**：设置页可在 NRL / FMO 间切换
  - **NRL**：UDP 传输，G.711 A-law（type 1）或 **Opus（type 8）** 语音编码
    - NRL Opus 规格：16kHz / 单声道 / 20ms（320 样本）帧 / 32–40kbps VBR / OPUS_APPLICATION_VOIP / complexity 10
  - **FMO**：MQTT 连接（`FMO/RAW` 语音），**Opus（SILK NB 8k）** 与 **IMA ADPCM** 双编码可选
- **全双工**：接收与发射互不干扰，可同时收听对方语音
- **PTT 发射**：短按切换发射状态，长按（320ms）持续发射，松开停止
- **键盘热键**：可自定义 PTT 触发键（默认 Space）
- **PTT 悬浮窗**：独立小窗口，始终置顶，方便单手操作
- **独立静音按钮**：可单独静音接收语音（FMO 下可关闭解码回放）
- **自动重采样**：优先使用设备原生 8000 Hz，不支持时自动重采样

### FMO 模式

- **APRS-IS 服务器发现**：连接 rotate.aprs2.net 实时解析 FMO-V4 STATION 广播，自动维护服务器表
- **证书导入**：上传 `cert_user.json` / `cert_int.json` / `cert_root.json` / `cert_devicekey.json`，即设当前身份。(证书需要使用备份工具从FMO上备份)。
- **SAS 认证**：基于 Ed25519 / CBOR 证书指纹自动构建 MQTT 凭据
- **服务器列表 / 收藏**：按在线人数排序，☆ 收藏后一键切换
 

### 实时状态

- 当前发言台站呼号实时显示
- 接收 / 发射电平表、频谱可视化（28 频段）
- 网络质量指标：延迟、抖动、丢包率、队列帧数、上下行码率
- 连接状态：连接中 / 已连接 / 重连恢复中 / 离线

### 调度消息

- 收发文本调度消息，支持 Ctrl+Enter 快捷发送
- 消息历史记录（最近 40 条）

### 在线设备

- 显示当前群组在线台站列表
- 支持多群组切换

### 设备与配置

- 自动检测默认音频输入 / 输出设备
- 可调抖动缓冲（Jitter Buffer）
- AGC / 降噪状态显示
- 本地配置持久化（协议、服务器、端口、呼号、SSID、音量、PTT 键、语音编码）

### 平台账号

- 平台账号登录 / 退出
- 自动恢复上次登录会话
- 服务器列表拉取与切换

### 其他

- 中文 / English 双语界面，一键切换
- AT 状态同步（下发本地 AT 状态到远端节点）
- 运行日志面板（设备初始化、连接事件、语音会话记录）
- 录音开关

---

## 下载

在 [Releases](../../releases) 页面下载对应平台的可执行文件：

| 平台 | 文件 |
| --- | --- |
| Windows | `nrl-pulse-windows.exe` |
| Linux | `nrl-pulse-linux` |
| macOS Apple Silicon | `nrl-pulse-mac-arm` |
| macOS Intel | `nrl-pulse-mac-x64` |

---

## 快速上手

### NRL 模式

1. 下载并运行对应平台的文件
2. 点击**登录**，填写平台账号连接服务器
3. 选择群组，点击**连接**建立语音会话
4. 按空格键（或自定义热键）开始 PTT 发射

### FMO 模式

1. 设置页切换到 **FMO** 协议
2. 导入证书 JSON（cert_user / cert_int / cert_devicekey 等）
3. 连接 APRS 发现 FMO 服务器，点选一台服务器（或从收藏中选择）
4. 点击**连接**建立 MQTT 会话，按空格键 PTT 发射

---

## 协议

### NRL2

基于 **NRL2** 语音调度协议，UDP 传输，20ms 帧长。
- type 1：G.711 A-law 语音（8kHz）
- type 8：**Opus** 语音（16kHz / 20ms / VOIP / complexity 10）
- type 2：心跳 · type 5：文本 · type 11：AT · type 12：串口透传

### FMO

基于 **FMO-V4** 协议：
- 发现：APRS-IS（rotate.aprs2.net:10152）FMO-V4 STATION 广播
- 认证：SAS（Ed25519 签名 + CBOR 证书指纹 → MQTT 密码）
- 语音：MQTT `FMO/RAW`，Opus（SILK NB 8kHz）或 IMA ADPCM（80ms 块，320B）
- 遥测：`FMO/TELE` · 服务器信息：`FMO/SERVER_INFO`

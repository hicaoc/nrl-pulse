# NRL Pulse

基于 `Vue 3 + TypeScript + Tauri 2 + Rust` 的跨平台 NRL 桌面通话程序骨架，面向 Windows、macOS、Linux。

## 当前已落地

- 现代化桌面控制台 UI，适合继续扩展实时在线台站、波形、电平、房间状态
- `Pinia` 运行时状态仓库
- `Tauri command + event` 双向通信骨架
- Rust 侧运行时状态中心与定时实时推送
- `NRL2` 协议包模型雏形，便于对照 `nrlnanny` 继续迁移
- Rust 侧 `G.711 A-law` 编解码实现
- Rust 侧 `UDP session / heartbeat / receive loop` 骨架
- 本地运行时配置读写，支持服务器、端口、呼号、SSID

## 目录结构

- `src/`：Vue 3 前端
- `src-tauri/src/runtime.rs`：桌面端运行时状态中心
- `src-tauri/src/nrl.rs`：NRL2 协议结构定义与编解码起点
- `src-tauri/src/g711.rs`：G.711 A-law 编解码
- `src-tauri/src/udp.rs`：UDP 会话、心跳、接收循环
- `src-tauri/src/config.rs`：本地配置持久化
- `src-tauri/src/lib.rs`：Tauri 命令入口

## 建议开发路线

1. 把 `nrlnanny` 中剩余协议字段和 `AT` 指令迁到 `src-tauri/src/nrl.rs`
2. 把 `udp.rs` 的接收分发接到真实 voice/text/control 处理器
3. 补 `audio/` 模块，接入麦克风采集、播放、录音、jitter buffer
4. 用真实设备枚举替换当前默认输入输出设备名
5. 最后把 UI 面板接到真实 runtime 数据，而不是当前模拟状态

## 从 nrlnanny 可直接参考的模块

- `/home/caocheng/ham/nrlnanny/decode.go`
- `/home/caocheng/ham/nrlnanny/udpclient.go`
- `/home/caocheng/ham/nrlnanny/g711.go`
- `/home/caocheng/ham/nrlnanny/monitor.go`
- `/home/caocheng/ham/nrlnanny/micPlay_windows.go`
- `/home/caocheng/ham/nrlnanny/micPlay_linux.go`

## 本地启动

先安装 Node.js、Rust、Tauri 依赖后执行：

```bash
npm install
npm run tauri dev
```

## 说明

当前环境里没有 `cargo`，所以我这次没有办法直接替你做 Rust 编译校验。前端依赖也还未安装，因此这次交付是工程骨架与代码落地，不是已编译产物。
# nrl-desktop

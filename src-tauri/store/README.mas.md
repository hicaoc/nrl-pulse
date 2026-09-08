# Mac App Store 上架指南（NRL Pulse）

商店版并入已发布的「NRL互联」App（App Store Connect 同一记录，iOS + macOS 双平台）：

- Bundle ID：`com.nrlptt.pttapp`（`tauri.mas.json` 覆盖，与 iOS 版一致；普通 DMG 版仍是 `com.nrl.pulse`）
- 显示名：`NRL互联`（`tauri.mas.json` 覆盖 productName）

## 打包产物

```
src-tauri/target/mas/NRL-Pulse_<版本>.pkg
```

用 **Transporter**（Mac App Store 免费下载）上传到 App Store Connect。

## MAS 版与普通 DMG 版的差异

| 项 | 普通版（DMG） | 商店版（MAS） |
| --- | --- | --- |
| 私有 API（PTT 悬浮窗透明） | 启用 | **禁用**（App Store 审核禁止私有 API，悬浮窗退化为不透明无边框窗口） |
| 内置自动更新 | 启用 | **禁用**（由 App Store 托管更新） |
| App Sandbox | 否 | **是**（`entitlements.mas.plist`） |
| 架构 | arm64 / x86_64 分开 | universal（二合一） |

实现方式：

- `tauri.mas.json`：合并配置覆盖（关 `macOSPrivateApi` / updater，换用沙盒 entitlements，`category: Utility`）
- `--no-default-features`：关闭 Cargo 默认 feature `macos-private-api`（= `tauri/macos-private-api`，
  会连带编入 wry 的 `drawsBackground` 等私有 KVC 调用，必须整体排除）
- `src/lib.rs`：`.transparent(true)` 已做条件编译，无 feature 时不调用
- `tauri.conf.json` 中 `macOSPrivateApi` 为 `false` 且 `tauri` 依赖不再直接声明
  `macos-private-api` feature —— 这是 tauri-build allowlist 校验的要求；
  实际开关是 Cargo feature（默认开启），两种构建均能通过校验

## Apple 开发者账号需要准备的东西

1. **Apple Developer 账号**（$99/年）：<https://developer.apple.com/programs/>
2. **App ID**：已存在 —— `com.nrlptt.pttapp`（「NRL互联」，与 iOS 版共用）
3. **App Store Connect 应用记录**：已存在 —— 「NRL互联」（Apple ID 6758810758），
   macOS 平台槽位已添加（macOS 1.0 准备提交）
4. **证书**（钥匙串访问 → 证书助理 → 从证书颁发机构请求证书…，再到开发者网站签发后双击导入，
   再在钥匙串中右键导出为 `.p12`）：
   - `Apple Distribution`（或 `3rd Party Mac Developer Application`）—— 应用签名
   - `3rd Party Mac Developer Installer` —— pkg 安装器签名
5. **描述文件**：Profiles → 新建 → Distribution → **Mac App Store**，
   关联 `com.nrlptt.pttapp` 的 App ID 与 `Apple Distribution` 证书，下载 `.provisionprofile`

## GitHub Actions 所需 Secrets

仓库 **Settings → Secrets and variables → Actions**：

| Secret | 说明 |
| --- | --- |
| `MAS_DISTRIBUTION_CERT_BASE64` | `Apple Distribution` 证书 `.p12` 的 base64（`base64 -i dist.p12 \| pbcopy`） |
| `MAS_DISTRIBUTION_CERT_PASSWORD` | 该 `.p12` 的导出密码 |
| `MAS_INSTALLER_CERT_BASE64` | `3rd Party Mac Developer Installer` 证书 `.p12` 的 base64 |
| `MAS_INSTALLER_CERT_PASSWORD` | 该 `.p12` 的导出密码 |
| `MAS_PROVISIONING_PROFILE_BASE64` | `.provisionprofile` 的 base64 |
| `APPLE_DISTRIBUTION_IDENTITY` | 如 `Apple Distribution: San Zhang (ABCDE12345)` |
| `APPLE_INSTALLER_IDENTITY` | 如 `3rd Party Mac Developer Installer: San Zhang (ABCDE12345)` |

未配置这些 secrets 时，`build-mas` job 只验证 MAS 版能编译，不产出 pkg。

## 本地手动打包

```bash
# 1. 构建 MAS 版 .app（universal，禁用私有 API 与内置更新）
make build-mas

# 2. 签名 + 打 pkg（两张证书需已导入本机钥匙串）
APPLE_DISTRIBUTION_IDENTITY="Apple Distribution: 姓名 (TEAMID)" \
APPLE_INSTALLER_IDENTITY="3rd Party Mac Developer Installer: 姓名 (TEAMID)" \
MAS_PROVISIONING_PROFILE=~/Downloads/NRL_Pulse.provisionprofile \
  bash src-tauri/store/build-mas.sh
```

脚本嵌入描述文件、用沙盒 entitlements 签名并打 pkg。
注意：**不要**在 entitlements 里手写 `com.apple.application-identifier` /
`com.apple.developer.team-identifier`，Apple 校验会报 90287 拒收（这两项由系统从
证书与描述文件自动推导）。

## 上传与提交审核

```bash
# 上传（App Store Connect API 密钥：用户与访问 → 集成 → 团队密钥；
# 私钥放 ~/.appstoreconnect/private_keys/AuthKey_<KeyID>.p8）
xcrun altool --validate-app -f src-tauri/target/mas/NRL-Pulse_<版本>.pkg -t macos \
  --apiKey <KeyID> --apiIssuer <IssuerID>
xcrun altool --upload-app  -f src-tauri/target/mas/NRL-Pulse_<版本>.pkg -t macos \
  --apiKey <KeyID> --apiIssuer <IssuerID>
```

本仓库当前使用的团队密钥名 `mas-upload`（Key ID 见 ASC「集成 → 团队密钥」页）。
也可用 Transporter（Mac App Store 安装）拖拽上传。

上传后在 App Store Connect：

1. macOS 版本页：把「版本」字段改成与构建一致（如 1.0.0），关联构建版本后**点保存**
   （关联构建属于未保存更改，不点保存刷新会丢）
2. 截图（Mac 规格：1280×800 / 1440×900 / 2560×1600 / 2880×1800，至少 1 张）
3. 「添加以供审核」→「App 审核」页草稿提交 →「提交以供审核」
4. 加密合规：`Info.plist` 已设 `ITSAppUsesNonExemptEncryption=false`，无需另传文稿
5. 隐私政策链接沿用 App 级配置（App 隐私页）

## 截图技巧（无需屏幕录制权限）

```bash
./node_modules/.bin/vite preview --port 4173   # 渲染同一套前端
```

用浏览器打开 `http://localhost:4173/`，视口设为 1440×900 直接截图即为合规尺寸；
`#monitor` 路由可截监听窗口。主界面/配置面板/登录面板各一张即可。

## 实战坑位记录（macOS 26 / Xcode 26）

- 审核自动化检查要求**最小 entitlements 集合**：本应用为纯客户端（UDP/MQTT/WebSocket
  均为发起外连），只需 `network.client`；曾带 `network.server` 被 Guideline 退回
  （2026-09 提交 0.2.9 时），移除后重传即可。
- `codesign`（macOS 15/26 均如此）拒绝签名 `Contents/` 根目录带杂散文件的 bundle
  （报 "code object is not signed at all"）。脚本先尝试普通签名，失败则回退 `--deep`：
  描述文件留在根目录、作为已签名嵌套组件纳入封印。App Store 校验 0 错误。
- **90889（TestFlight 资格）目前无解**：服务端要求描述文件以 Xcode 私有签名路径封印，
  公共工具链（含 macOS 15/26、profile 放根目录/Resources/符号链接三种变体，均已实测）
  无法满足；Unity/Solar2D/.NET 等非 Xcode 工具链多年存在同样问题。
  **Mac  beta 测试请走 Developer ID 公证 DMG 渠道（`make build-mac` + notarize），
  或等审核通过后直接用商店版。**
- OpenSSL 3 生成的 p12 需加 `-legacy`（3DES/SHA1）才能被 `security import` 导入钥匙串。
- 专用钥匙串（如 `mas-build.keychain-db`）+ `set-key-partition-list` 可免交互签名。
- MAS 版本号在 `tauri.mas.json` 的 `version` 独立管理（当前 1.0.0），与仓库版本解耦；
  每次提交新版本需同步 ASC 版本字段并递增。

## 沙盒功能差异（需在商店描述/审核中知悉）

| 功能 | 商店版行为 |
| --- | --- |
| 串口透传 | 沙盒禁止访问 `/dev/cu.*`，不可用 |
| FMO 自动获取证书（绑定 MAC） | 系统对沙盒应用返回固定假 MAC，与服务器登记不一致时需手动导入证书 |
| 本地录音 | 仅可写入应用容器目录或用户通过对话框授权的目录 |
| PTT 悬浮窗 | 不透明（透明依赖私有 API） |
| 自动更新 | 由 App Store 托管 |

# Microsoft Store 上架指南（NRL Pulse）

## 打包产物

MSIX 包由 `store/build-msix.ps1` 生成（内部用 Windows SDK 的 `makeappx.exe`，无需额外插件）：

```
src-tauri\target\msix\NRL-Pulse_<版本>_x64.msix
```

特点：
- 用 `tauri.store.json` 覆盖配置编译：**禁用内置自动更新**（商店托管更新），不产生 updater 签名产物
- 包含 `nrl-pulse.exe` + `nrl_pulse_lib.dll` + 商店图标（Assets/）
- Win32 全信任应用（`runFullTrust`），行为与普通 exe 一致

## 打包命令

```powershell
# 在项目根目录
powershell -ExecutionPolicy Bypass -File src-tauri/store/build-msix.ps1

# 常用参数
-IdentityName "12345hicaoc.NRLPulse"   # Partner Center: Package/Identity/Name
-Publisher    "CN=xxxxxxx-xxxx-..."    # Partner Center: Package/Identity/Publisher（须与证书一致）
-IdentityName / -Publisher 也可用环境变量: MSIX_IDENTITY_NAME / MSIX_PUBLISHER
-Cert store\cert.pfx -CertPassword xxx # 本地签名（可选，商店上传时可由商店代签）
-SkipBuild                             # 跳过编译直接打包（调试用）
```

## 上架步骤（需要你本人操作的部分）

1. **注册 Partner Center 开发者账号**
   - <https://partner.microsoft.com/zh-cn/dashboard/account/v3> → 注册（个人约 $19，公司约 $99，一次性）
   - 需要微软账号 + 付款方式 + 身份验证

2. **创建应用、预留名称**
   - 仪表板 → 应用和游戏 → 创建应用 → 输入 `NRL Pulse`
   - 进入应用 → **产品管理 → 产品标识**，记下：
     - `Package/Identity/Name` → 脚本的 `-IdentityName`
     - `Package/Identity/Publisher`（如 `CN=AB12CD34-...`）→ 脚本的 `-Publisher`

3. **签名证书**（商店上传通常要求包已签名，且包内 Publisher 与账号身份一致）
   - 购买代码签名证书（OV 即可，约几百元/年），导出 .pfx 后传 `-Cert`；
   - 或使用 Azure Trusted Signing 云签名；
   - 若你的提交通道允许未签名包（部分渠道/测试），可先传未签名包验证其他环节。

4. **重新打包并上传**
   ```powershell
   powershell -ExecutionPolicy Bypass -File src-tauri/store/build-msix.ps1 `
     -IdentityName "<Package/Identity/Name>" -Publisher "<Publisher>"
   ```
   - Partner Center → 该应用 → 创建新提交 → 上传 `NRL-Pulse_x.x.x.0_x64.msix`

5. **商店列表信息**（提交页面填写）
   - 描述、分类（建议：工具 / 生产力）
   - 截图：1920×1080 或 1366×766（至少 1 张，建议 3-5 张）
   - 隐私政策链接（必填，需公开可访问的 URL）
   - 联系方式

6. **提交认证**
   - 认证一般 1-3 个工作日。常见驳回点：
     - 隐私政策缺失/不可访问
     - 应用需要注册/登录但未在描述中说明（本应用部分功能需登录平台账号）
     - 崩溃/功能不完整

## 注意事项

- **版本号**：MSIX 要求 4 段（脚本自动把 `0.2.6` 补成 `0.2.6.0`）；每次提交必须递增
- **商店版不要启用内置 updater**：已由 `tauri.store.json` 处理；不要改回
- 发布商店版后，GitHub Release 的 NSIS 安装包可继续作为"侧载版"分发，两条渠道互不影响
- MSIX 沙盒虚拟化：应用配置写在用户私有 AppData 中，卸载时会被清理（与普通安装版略有差异，功能不受影响）

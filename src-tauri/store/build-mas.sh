#!/usr/bin/env bash
# NRL Pulse — Mac App Store 签名打包脚本
#
# 前置：先构建 MAS 版 .app（未签名、无私有 API、无内置更新、App Sandbox）：
#   make build-mas
# 等价于：
#   pnpm build && ./node_modules/.bin/tauri build \
#     --config src-tauri/tauri.mas.json \
#     --target universal-apple-darwin --bundles app -- --no-default-features
#
# 必需环境变量：
#   APPLE_DISTRIBUTION_IDENTITY   应用签名身份，如 "3rd Party Mac Developer Application: 名称 (TEAMID)"
#                                 或 "Apple Distribution: 名称 (TEAMID)"
#   APPLE_INSTALLER_IDENTITY      pkg 签名身份，如 "3rd Party Mac Developer Installer: 名称 (TEAMID)"
#   MAS_PROVISIONING_PROFILE      Mac App Store 描述文件路径（.provisionprofile）
# 可选：
#   MAS_APP_PATH                  .app 路径（默认 universal 构建产物）
#   MAS_OUT_DIR                   输出目录（默认 src-tauri/target/mas）
#
# 产物：$MAS_OUT_DIR/NRL-Pulse_<版本>.pkg，用 Transporter 上传到 App Store Connect
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"

fail() { echo "ERROR: $*" >&2; exit 1; }

BUNDLE_DIR="src-tauri/target/universal-apple-darwin/release/bundle/macos"
if [ -z "${MAS_APP_PATH:-}" ]; then
  APP=$(find "$BUNDLE_DIR" -maxdepth 1 -name "*.app" 2>/dev/null | head -1)
  [ -n "$APP" ] || fail "在 $BUNDLE_DIR 找不到 .app，请先执行 make build-mas"
else
  APP="$MAS_APP_PATH"
fi
BASE_ENTITLEMENTS="src-tauri/entitlements.mas.plist"
OUT_DIR="${MAS_OUT_DIR:-src-tauri/target/mas}"

[ -d "$APP" ] || fail "找不到 $APP，请先执行 make build-mas"
[ -f "$BASE_ENTITLEMENTS" ] || fail "找不到 $BASE_ENTITLEMENTS"
[ -n "${APPLE_DISTRIBUTION_IDENTITY:-}" ] || fail "未设置 APPLE_DISTRIBUTION_IDENTITY"
[ -n "${APPLE_INSTALLER_IDENTITY:-}" ] || fail "未设置 APPLE_INSTALLER_IDENTITY"
[ -n "${MAS_PROVISIONING_PROFILE:-}" ] && [ -f "$MAS_PROVISIONING_PROFILE" ] || fail "MAS_PROVISIONING_PROFILE 未指向有效描述文件"

# 版本以构建产物为准（MAS 版通过 tauri.mas.json 覆盖为 1.0.0）
VERSION=$(plutil -extract CFBundleShortVersionString raw "$APP/Contents/Info.plist")
PKG="$OUT_DIR/NRL-Pulse_${VERSION}.pkg"

mkdir -p "$OUT_DIR"

# 注意：签名 entitlements 只含 App Sandbox 及资源权限。
# com.apple.application-identifier / team-identifier 由系统从证书与描述文件自动推导，
# 手动写入会被 App Store 校验拒绝（错误 90287）。

echo "==> 嵌入 provisioning profile"
# 描述文件必须在 Contents/ 根目录（Apple 校验/TestFlight 要求）。
# macOS 26 的 codesign 会把根目录杂散文件当未签名子组件拒签，
# 用 --deep 让它把描述文件作为已签名嵌套组件一并处理（字节不变，仅纳入封印）。
cp "$MAS_PROVISIONING_PROFILE" "$APP/Contents/embedded.provisioningprofile"

if [ -d "$APP/Contents/Frameworks" ]; then
  echo "==> 签名 Frameworks"
  find "$APP/Contents/Frameworks" -maxdepth 1 \( -name "*.dylib" -o -name "*.framework" \) -print0 |
    while IFS= read -r -d '' item; do
      codesign --force --sign "$APPLE_DISTRIBUTION_IDENTITY" \
        --options runtime --timestamp "$item"
    done
fi

echo "==> 签名应用 ($APPLE_DISTRIBUTION_IDENTITY)"
# 优先普通签名：macOS ≤15 的 codesign 会把根目录 profile 封为普通资源（TestFlight 资格完整）。
# macOS 26 的扫描器把根目录杂散文件当未签名嵌套组件而拒签，此时回退 --deep：
# profile 作为已签名嵌套组件纳入封印（App Store 上架不受影响，但 TestFlight 会报 90889 警告）。
if ! codesign --force --sign "$APPLE_DISTRIBUTION_IDENTITY" \
  --entitlements "$BASE_ENTITLEMENTS" --options runtime --timestamp "$APP" 2>/tmp/mas-codesign.err; then
  echo "    普通签名失败（$(head -2 /tmp/mas-codesign.err | tail -1)），回退 --deep"
  codesign --force --deep --sign "$APPLE_DISTRIBUTION_IDENTITY" \
    --entitlements "$BASE_ENTITLEMENTS" --options runtime --timestamp "$APP"
fi

echo "==> 校验签名"
codesign --verify --deep --strict --verbose=2 "$APP"

echo "==> 打包 pkg ($APPLE_INSTALLER_IDENTITY)"
rm -f "$PKG"
productbuild --component "$APP" /Applications \
  --sign "$APPLE_INSTALLER_IDENTITY" --timestamp "$PKG"

echo ""
echo "完成: $PKG"
echo "下一步：用 Transporter（Mac App Store）上传该 pkg，然后在 App Store Connect 提交审核"

#!/usr/bin/env bash
# 用法: ./scripts/bump-version.sh [major|minor|patch]
# 默认 patch
set -euo pipefail

TYPE=${1:-patch}
ROOT=$(cd "$(dirname "$0")/.." && pwd)

# 读取当前版本（从 package.json 为准）
CURRENT=$(node -p "require('./package.json').version" 2>/dev/null || \
          python3 -c "import json; print(json.load(open('package.json'))['version'])")

IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"

case "$TYPE" in
  major) MAJOR=$((MAJOR+1)); MINOR=0; PATCH=0 ;;
  minor) MINOR=$((MINOR+1)); PATCH=0 ;;
  patch) PATCH=$((PATCH+1)) ;;
  *)
    echo "用法: $0 [major|minor|patch]"
    exit 1
    ;;
esac

NEW="$MAJOR.$MINOR.$PATCH"
echo "版本: $CURRENT → $NEW"

cd "$ROOT"

# 更新 package.json
sed -i "s/\"version\": \"$CURRENT\"/\"version\": \"$NEW\"/" package.json

# 更新 src-tauri/Cargo.toml（只改 [package] 段的 version）
sed -i "0,/^version = \"$CURRENT\"/{s/^version = \"$CURRENT\"/version = \"$NEW\"/}" src-tauri/Cargo.toml

# 更新 src-tauri/tauri.conf.json
sed -i "s/\"version\": \"$CURRENT\"/\"version\": \"$NEW\"/" src-tauri/tauri.conf.json

echo "已更新三处版本号为 $NEW"
echo ""
echo "下一步:"
echo "  git add package.json src-tauri/Cargo.toml src-tauri/tauri.conf.json"
echo "  git commit -m \"chore: bump version to $NEW\""
echo "  git tag v$NEW"
echo "  git push && git push --tags"

.PHONY: build build-win build-linux build-mac build-mac-x64 build-all dev-backend bump bump-minor bump-major

CARGO_ENV = . "$(HOME)/.cargo/env" &&
TAURI     = $(CARGO_ENV) vp build && ./node_modules/.bin/tauri build

# 检测当前操作系统
OS := $(shell uname -s)

# 默认目标：根据当前平台构建对应原生版本
build:
ifeq ($(OS),Linux)
	$(MAKE) build-linux
else ifeq ($(OS),Darwin)
	$(MAKE) build-mac
else
	$(MAKE) build-win
endif

# 版本号管理
bump:
	@bash scripts/bump-version.sh patch

bump-minor:
	@bash scripts/bump-version.sh minor

bump-major:
	@bash scripts/bump-version.sh major

# 仅编译 Linux 后端（不构建前端，用于开发调试）
dev-backend:
	$(CARGO_ENV) cargo build --manifest-path src-tauri/Cargo.toml

# Windows（从 Linux/macOS 交叉编译，需要 cargo-xwin）
build-win:
	$(TAURI) --runner cargo-xwin --target x86_64-pc-windows-msvc --no-bundle

# Linux 原生编译
build-linux:
	$(TAURI) --target x86_64-unknown-linux-gnu --no-bundle

# macOS Apple Silicon（必须在 macOS 上运行）
build-mac:
	$(TAURI) --target aarch64-apple-darwin --no-bundle

# macOS Intel x86_64（必须在 macOS 上运行）
build-mac-x64:
	$(TAURI) --target x86_64-apple-darwin --no-bundle

# 同时构建全部平台（仅 macOS 上支持，需要预先安装 cargo-xwin 和 Linux target）
build-all:
	$(MAKE) build-win
	$(MAKE) build-linux
	$(MAKE) build-mac
	$(MAKE) build-mac-x64

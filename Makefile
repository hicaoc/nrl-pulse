.PHONY: build

build:
	. "$(HOME)/.cargo/env" && vp build && ./node_modules/.bin/tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --no-bundle

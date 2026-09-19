# macOS Apple Silicon 的 release 构建（README: Building a release binary for macOS）
TARGET := aarch64-apple-darwin
BIN    := target/$(TARGET)/release/vg-mirror
PREFIX ?= $(HOME)/.local/bin

.PHONY: release install clean

# 编译优化版 arm64 二进制，去掉调试符号，并打印架构和大小确认
release:
	rustup target add $(TARGET)
	cargo build --release --target $(TARGET)
	strip $(BIN)
	@file $(BIN)
	@ls -lh $(BIN) | awk '{print "size: " $$5}'

# 拷贝到 PATH 里（默认 ~/.local/bin，可用 PREFIX=... 覆盖）
install: release
	mkdir -p $(PREFIX)
	cp $(BIN) $(PREFIX)/

clean:
	cargo clean

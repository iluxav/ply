# dev loop for ply

TARGET := x86_64-unknown-linux-musl
BIN    := target/$(TARGET)/release/ply

.PHONY: check fmt build test release install uninstall

# fast feedback: fmt + clippy + tests
check:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace

fmt:
	cargo fmt --all

build:
	cargo build --workspace

test:
	cargo test --workspace

# true static release binary
release:
	cargo build --release --target $(TARGET) -p ply-cli
	@ls -lh $(BIN)

# build the static binary and install it as `ply` (exactly one file lands on the host)
install: release
	sudo install -m 755 $(BIN) /usr/local/bin/ply
	@echo "installed $$(ply --version) to /usr/local/bin/ply"

uninstall:
	sudo rm -f /usr/local/bin/ply
	@echo "removed /usr/local/bin/ply"

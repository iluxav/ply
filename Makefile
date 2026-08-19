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

# Ubuntu >= 24.04: unprivileged user namespaces (rootless `ply run`) need an
# AppArmor profile granting `userns` — same requirement as Docker/Chrome.
install-apparmor:
	printf 'abi <abi/4.0>,\ninclude <tunables/global>\n\nprofile ply /usr/local/bin/ply flags=(unconfined) {\n  userns,\n}\n' | sudo tee /etc/apparmor.d/ply >/dev/null
	sudo apparmor_parser -r /etc/apparmor.d/ply
	@echo "AppArmor profile installed — rootless ply run enabled"

uninstall:
	sudo rm -f /usr/local/bin/ply /etc/apparmor.d/ply
	@echo "removed /usr/local/bin/ply"

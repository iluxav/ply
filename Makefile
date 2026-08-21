# dev loop for ply

TARGET := x86_64-unknown-linux-musl
BIN    := target/$(TARGET)/release/ply

.PHONY: check fmt build test release install uninstall web web-serve registry-catalog registry-push registry-state registry

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

# --- websites + official registry (R2 via wrangler) --------------------------

# build tailwind + render registry page + push both sites
# (plybox.sh ← web/landing, registry.plybox.sh ← web/registry)
web:
	./web/push.sh

# preview both sites locally (landing :8180, registry :8181)
web-serve:
	./web/serve.sh

# refresh the conversion catalog from the latest Alpine APKINDEX
registry-catalog:
	./scripts/apk-catalog.mjs --tier main-core -o scripts/apk2pkg.json

# convert + upload the next batch of packages
# (override: make registry-push LIMIT=500 JOBS=8)
LIMIT ?= 200
JOBS  ?= 6
registry-push:
	./scripts/registry-push.mjs --limit $(LIMIT) --jobs $(JOBS)

# republish state.json + re-render the registry page, no conversions
registry-state:
	./scripts/registry-push.mjs --state-only

# the daily job: catalog refresh + delta push
registry: registry-catalog registry-push

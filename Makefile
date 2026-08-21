# dev loop for ply

TARGET := x86_64-unknown-linux-musl
BIN    := target/$(TARGET)/release/ply

.PHONY: check fmt build test static release install uninstall web web-serve registry-catalog registry-push registry-state registry

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
static:
	cargo build --release --target $(TARGET) -p ply-cli
	@ls -lh $(BIN)

# cut a release: bump version, gate (fmt+clippy+tests), commit, push,
# GitHub release (tag creation triggers .github/workflows/release.yml,
# whose guard re-checks tag == Cargo.toml version).
#   make release            # patch bump (0.1.3 -> 0.1.4)
#   make release V=0.2.0    # explicit version
release:
	@set -eu; \
	test "$$(git rev-parse --abbrev-ref HEAD)" = main || { echo "release: not on main"; exit 1; }; \
	test -z "$$(git status --porcelain)" || { echo "release: working tree not clean"; exit 1; }; \
	git pull --ff-only; \
	CUR=$$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1); \
	V="$(V)"; \
	[ -n "$$V" ] || V=$$(echo "$$CUR" | awk -F. '{print $$1"."$$2"."$$3+1}'); \
	echo "$$V" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$$' || { echo "release: bad version \`$$V\`"; exit 1; }; \
	echo "release: $$CUR -> $$V"; \
	$(MAKE) check; \
	sed -i "s/^version = \".*\"/version = \"$$V\"/" Cargo.toml; \
	cargo update --workspace >/dev/null 2>&1; \
	git add Cargo.toml Cargo.lock; \
	git commit -m "v$$V"; \
	git push; \
	gh release create "v$$V" --title "v$$V" --generate-notes; \
	echo "release: v$$V cut — follow the build with: gh run watch"

# build the static binary and install it as `ply` (exactly one file lands on the host)
install: static
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

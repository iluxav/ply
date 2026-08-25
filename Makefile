# dev loop for ply

TARGET := x86_64-unknown-linux-musl
BIN    := target/$(TARGET)/release/ply

.PHONY: check fmt build test static release install uninstall registry-catalog registry-push registry-state registry registry-all

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
	git tag "v$$V"; \
	git push origin "v$$V"; \
	echo "release: v$$V tagged — release.yml builds both binaries, then creates the"; \
	echo "release: release with them attached (nothing is 'latest' until it is complete)"; \
	echo "release: follow with: gh run watch"

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

# --- website + official registry ---------------------------------------------
# The site (landing + /docs + /registry) is app/ — a Next.js app deployed to
# the web droplet by .github/workflows/deploy-web.yml on push. Local dev:
#   cd app && npm run dev

# One pipeline, parameterized by ARCH (x64 default; arm64 = ARCH=arm64).
# Separate catalog file per arch; shared ledger (per-arch keys) — so each
# package's index.json ends up listing both arches. NEVER run two pushes
# concurrently: the ledger is one file.
#   make registry LIMIT=500              # x64: catalog refresh + delta push
#   make registry ARCH=arm64 LIMIT=500   # arm64: same
#   make registry-all                    # both, sequentially
ARCH  ?= x64
LIMIT ?= 200
JOBS  ?= 6
ALPINE_ARCH = $(if $(filter arm64,$(ARCH)),aarch64,x86_64)
CATALOG     = scripts/apk2pkg$(if $(filter arm64,$(ARCH)),-arm64,).json

# refresh the conversion catalog from the latest Alpine APKINDEX
# (tier cli: main+community packages that ship a command — the set a user
# could actually declare; pure libraries arrive vendored inside consumers)
registry-catalog:
	./scripts/apk-catalog.mjs --tier cli --arch $(ALPINE_ARCH) -o $(CATALOG)

# convert + upload the next batch of packages
registry-push:
	./scripts/registry-push.mjs --catalog $(CATALOG) --limit $(LIMIT) --jobs $(JOBS)

# republish state.json + re-render the registry page, no conversions
registry-state:
	./scripts/registry-push.mjs --state-only

# the daily job for one arch: catalog refresh + delta push
registry: registry-catalog registry-push

# both arches, sequentially (the nightly shape)
registry-all:
	$(MAKE) registry ARCH=x64
	$(MAKE) registry ARCH=arm64

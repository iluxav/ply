# dev loop for ply

TARGET := x86_64-unknown-linux-musl
BIN    := target/$(TARGET)/release/ply

.PHONY: check check-darwin mac-test mac-sign install-mac fmt build test static release release-cli release-web install uninstall registry-catalog registry-push registry registry-all

# fast feedback: fmt + clippy + tests
check:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace

# The macOS seam gate, runnable on Linux. Needs cargo-zigbuild
# (`uv tool install cargo-zigbuild`) and a `zig` on PATH; CI runs the same
# check natively on macos-latest. Clean = 0 errors under -D warnings.
check-darwin:
	rustup target add aarch64-apple-darwin >/dev/null
	cargo-zigbuild check --target aarch64-apple-darwin -p ply-cli
	cargo-zigbuild clippy --target aarch64-apple-darwin -p ply-cli -- -D warnings

fmt:
	cargo fmt --all

# The macOS microVM suite: it boots real VMs, so it runs only on an Apple
# Silicon Mac and only when asked. Needs a kernel — either the published
# `ply/microvm-kernel` keg or PLY_MICROVM_KERNEL pointing at a local build
# (scripts/build-microvm-kernel.sh); every test skips with a message saying so
# if it is unset.
#
# The suite signs its OWN copy of the binary (see `fn ply` in the test file)
# rather than relying on a `codesign` step here: hv_vm_create checks
# com.apple.security.hypervisor at the call, not at load, and cargo re-uplifts
# target/debug/ply from target/debug/deps on its next invocation — which
# silently strips any signature applied between the two.
mac-test:
	@test "$$(uname -s)" = Darwin || { echo "mac-test: Apple Silicon macOS only"; exit 1; }
	cargo test -p ply-cli --test macos_vm -- --test-threads=1 --nocapture

# Sign target/debug/ply in place, for running `./target/debug/ply run` by hand.
# Re-run it after any `cargo build`, which drops the signature.
mac-sign:
	cargo build -p ply-cli
	@printf '<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict><key>com.apple.security.hypervisor</key><true/></dict></plist>\n' > target/hv.entitlements
	codesign --entitlements target/hv.entitlements --force -s - target/debug/ply

build:
	cargo build --workspace

test:
	cargo test --workspace

# true static release binary
static:
	cargo build --release --target $(TARGET) -p ply-cli
	@ls -lh $(BIN)

# cut releases:
#   make release-cli            # CLI: version bump, gate, tag (release.yml builds binaries)
#   make release-cli V=0.2.0    # explicit version
#   make release-web            # site: dispatch deploy-web.yml for current main
#                               # (the ONLY way the site deploys — pushes ship nothing)
#   make release                # both, CLI first
release: release-cli release-web

release-web:
	@$(MAKE) -C app release

release-cli:
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
	echo "release-cli: v$$V tagged — release.yml builds both binaries, then creates the"; \
	echo "release-cli: release with them attached (nothing is 'latest' until it is complete)"; \
	echo "release-cli: follow with: gh run watch"

# Install a release `ply` on this Mac, signed with the hypervisor entitlement.
#
# The entitlement is not optional and its absence is confusing: hv_vm_create
# checks com.apple.security.hypervisor at the CALL, not at load, so an
# unsigned binary installs fine, runs fine, resolves the image fine, and then
# fails the moment it would create a VM.
#
# Signing happens on the INSTALLED copy, never on target/release/ply: cargo
# re-uplifts that path from target/release/deps on its next invocation and
# silently strips the signature, so a `make install-mac; cargo build` would
# leave you with a binary that used to work.
#
#   make install-mac                      -> /usr/local/bin/ply
#   make install-mac MAC_PREFIX=~/.local/bin
MAC_PREFIX ?= /usr/local/bin

install-mac:
	@test "$$(uname -s)" = Darwin || { echo "install-mac: macOS only — use \`make install\` on Linux"; exit 1; }
	@test "$$(uname -m)" = arm64 || { echo "install-mac: Apple Silicon only (M1 or later)"; exit 1; }
	cargo build --release -p ply-cli
	@printf '<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict><key>com.apple.security.hypervisor</key><true/></dict></plist>\n' > target/hv.entitlements
	@mkdir -p $(MAC_PREFIX)
	install -m 755 target/release/ply $(MAC_PREFIX)/ply
	codesign --entitlements target/hv.entitlements --force -s - $(MAC_PREFIX)/ply
	@echo "installed $$($(MAC_PREFIX)/ply --version) to $(MAC_PREFIX)/ply"
	@codesign -d --entitlements - $(MAC_PREFIX)/ply 2>&1 | grep -q hypervisor \
		&& echo "entitlement: com.apple.security.hypervisor ok" \
		|| { echo "entitlement MISSING — ply run will fail at hv_vm_create"; exit 1; }
	@echo "note: until \`ply push\` publishes the kernel keg, set PLY_MICROVM_KERNEL"
	@echo "      to a directory holding microvm-kernel.img + initramfs.cpio"

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

# NOTE: there is no `registry-state` target any more. This lane uploads
# bytes (image + .toml + index.json) and nothing else — state.json is
# derived from the registry's records table, across every namespace, so
# rendering a snapshot from the keg ledger would delete most of the catalog.
# To get keg metadata into the catalog: ./scripts/registry-republish.mjs

# the daily job for one arch: catalog refresh + delta push
registry: registry-catalog registry-push

# both arches, sequentially (the nightly shape)
registry-all:
	$(MAKE) registry ARCH=x64
	$(MAKE) registry ARCH=arm64

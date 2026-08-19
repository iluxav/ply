# dev loop for ply

default: check

# fast feedback: fmt + clippy + tests
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

fmt:
    cargo fmt --all

build:
    cargo build --workspace

# true static release binary
release:
    cargo build --release --target x86_64-unknown-linux-musl -p ply-cli
    @ls -lh target/x86_64-unknown-linux-musl/release/ply

test:
    cargo test --workspace

#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

export HTTP_PROXY="${HTTP_PROXY:-http://127.0.0.1:10808}"
export HTTPS_PROXY="${HTTPS_PROXY:-http://127.0.0.1:10808}"
export http_proxy="${http_proxy:-$HTTP_PROXY}"
export https_proxy="${https_proxy:-$HTTPS_PROXY}"

echo '== host tests =='
cargo fmt --all -- --check
cargo test --workspace

echo '== anydoc Linux x64 fixture run =='
cargo run -p rsou-anydoc-matrix-spike --release

echo '== tokenizer/libsimple oracle =='
scripts/tokenizer-oracle.sh

echo '== synthetic 100k FTS5 benchmark =='
cargo run -p rsou-scale-spike --release

echo '== Linux arm64 compile check =='
cargo check -p rsou-anydoc-matrix-spike --target aarch64-unknown-linux-gnu

echo '== Windows 7 x64 =='
cargo +nightly -Z build-std=std,panic_unwind \
    build -p rsou-anydoc-matrix-spike --release --target x86_64-win7-windows-gnu
scripts/check-win7-imports.sh \
    target/x86_64-win7-windows-gnu/release/rsou-anydoc-matrix-spike.exe \
    x86_64-win7-windows-gnu

echo '== Windows 7 x86 =='
cargo +nightly -Z build-std=std,panic_unwind \
    build -p rsou-anydoc-matrix-spike --release --target i686-win7-windows-gnu
scripts/check-win7-imports.sh \
    target/i686-win7-windows-gnu/release/rsou-anydoc-matrix-spike.exe \
    i686-win7-windows-gnu

echo 'stage 0: PASS WITH ARM64 LINKER TODO (arm64 is compile-checked; a runnable arm64 binary needs an arm64 linker/sysroot)'

#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

LIBSIMPLE="${RSOU_LIBSIMPLE:-/home/ljw/wsou/resources/simple/linux-x64/libsimple.so}"
SQLITE_HEADER="${SQLITE_HEADER:-$HOME/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libsqlite3-sys-0.37.0/sqlite3}"
ORACLE_BIN="${TMPDIR:-/tmp}/rsou-fts5-oracle-$$"
RUST_OUTPUT="${TMPDIR:-/tmp}/rsou-rust-tokens-$$"
ORACLE_OUTPUT="${TMPDIR:-/tmp}/rsou-simple-tokens-$$"
trap 'rm -f "$ORACLE_BIN" "$RUST_OUTPUT" "$ORACLE_OUTPUT"' EXIT

if [[ ! -f "$LIBSIMPLE" ]]; then
    echo "libsimple not found: $LIBSIMPLE" >&2
    exit 2
fi
if [[ ! -f "$SQLITE_HEADER/sqlite3.h" ]]; then
    echo "SQLite development header not found: $SQLITE_HEADER/sqlite3.h" >&2
    exit 2
fi

gcc -std=c11 -I"$SQLITE_HEADER" \
    spikes/tokenizer/oracle.c -Wl,-l:libsqlite3.so.0 -ldl -o "$ORACLE_BIN"

export LD_LIBRARY_PATH="$(dirname "$LIBSIMPLE")${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export HTTP_PROXY="${HTTP_PROXY:-http://127.0.0.1:10808}"
export HTTPS_PROXY="${HTTPS_PROXY:-http://127.0.0.1:10808}"
export http_proxy="${http_proxy:-$HTTP_PROXY}"
export https_proxy="${https_proxy:-$HTTPS_PROXY}"

cargo build -q -p rsou-tokenizer-spike --bin dump-tokens
RUST_BIN="target/debug/dump-tokens"

compare() {
    local input="$1"
    "$RUST_BIN" "$input" >"$RUST_OUTPUT"
    "$ORACLE_BIN" "$LIBSIMPLE" "$input" >"$ORACLE_OUTPUT"
    python3 - "$RUST_OUTPUT" "$ORACLE_OUTPUT" <<'PY'
import pathlib
import sys

rust = pathlib.Path(sys.argv[1]).read_text().splitlines()
oracle = pathlib.Path(sys.argv[2]).read_text().splitlines()

# libsimple's `simple 0` also reports Unicode punctuation/symbol code points;
# rsou deliberately treats those as separators.  Remove only those documented
# known differences before comparing the useful word/Chinese-character stream.
filtered = [line for line in oracle if line.split("\t", 1)[0].isalnum()]
if rust != filtered:
    print("tokenizer mismatch after removing documented punctuation/symbol differences")
    print("rsou:", rust)
    print("libsimple:", oracle)
    print("filtered:", filtered)
    raise SystemExit(1)
PY
}

compare '合同编号 A4 foo中文'
compare '公文号、发票号 文档'
compare 'HTTP://EXAMPLE.COM 32.3'
compare '长英文Word mixed_case'

echo "oracle parity: PASS (ASCII/CJK tokens and source ranges equal)"
echo "known difference: libsimple emits Unicode punctuation/symbol tokens; rsou drops them"

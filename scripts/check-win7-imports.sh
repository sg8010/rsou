#!/usr/bin/env bash
set -euo pipefail

exe="${1:?usage: check-win7-imports.sh EXE TARGET}"
target="${2:?usage: check-win7-imports.sh EXE TARGET}"

if [[ ! -f "$exe" ]]; then
    echo "missing PE: $exe" >&2
    exit 2
fi

case "$target" in
    x86_64-*) objdump_bin="${OBJDUMP:-x86_64-w64-mingw32-objdump}"; expected=("PE32+ executable" "x86-64") ;;
    i686-*) objdump_bin="${OBJDUMP:-i686-w64-mingw32-objdump}"; expected=("PE32 executable" "Intel 80386") ;;
    *) echo "unsupported target: $target" >&2; exit 2 ;;
esac

if ! command -v "$objdump_bin" >/dev/null 2>&1; then
    echo "missing PE inspection tool: $objdump_bin" >&2
    exit 2
fi

imports="$($objdump_bin -p "$exe")"
for forbidden in \
    "combase.dll" \
    "PathCchStripPrefix" \
    "api-ms-win-core-path-l1-1-0.dll" \
    "WaitOnAddress" \
    "api-ms-win-core-synch-l1-2-0.dll" \
    "ProcessPrng"; do
    if grep -Fqi "$forbidden" <<<"$imports"; then
        echo "forbidden Windows 8+ import: $forbidden" >&2
        exit 1
    fi
done

file_type="$(file -b "$exe")"
for marker in "${expected[@]}"; do
    if ! grep -Fq "$marker" <<<"$file_type"; then
        echo "unexpected PE architecture: $file_type" >&2
        exit 1
    fi
done

echo "Win7 import audit: PASS ($target)"

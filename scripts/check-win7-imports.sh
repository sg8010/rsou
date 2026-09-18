#!/usr/bin/env bash
# 检查 Windows 产物是否误引用 Windows 7 没有的 API。
set -euo pipefail

EXE="${1:-target/x86_64-win7-windows-gnu/release/rsou.exe}"
TARGET="${2:-x86_64-win7-windows-gnu}"
if [ -z "${OBJDUMP:-}" ]; then
    case "$TARGET" in
        x86_64-*) OBJDUMP=x86_64-w64-mingw32-objdump ;;
        i686-*) OBJDUMP=i686-w64-mingw32-objdump ;;
        *) OBJDUMP=x86_64-w64-mingw32-objdump ;;
    esac
fi

if [ ! -f "$EXE" ]; then
    echo "❌ 找不到 Windows 产物: $EXE" >&2
    exit 1
fi
if ! command -v "$OBJDUMP" >/dev/null 2>&1; then
    echo "❌ 未找到 PE 导入表检查工具: $OBJDUMP" >&2
    exit 1
fi

IMPORTS=$("$OBJDUMP" -p "$EXE")

# 这些 API/API-set 分别来自 Windows 8 或 Windows 10。尤其是
# PathCchStripPrefix：arboard 的文件列表剪贴板代码会引用它，但本应用
# 不使用该接口，release fat LTO 应将这段未使用代码删除。
for forbidden in \
    "combase.dll" \
    "PathCchStripPrefix" \
    "api-ms-win-core-path-l1-1-0.dll" \
    "WaitOnAddress" \
    "api-ms-win-core-synch-l1-2-0.dll" \
    "ProcessPrng"; do
    if grep -Fq "$forbidden" <<<"$IMPORTS"; then
        echo "❌ 产物引用了 Windows 7 不支持的 API: $forbidden" >&2
        exit 1
    fi
done

FILE_TYPE=$(file -b "$EXE")
case "$TARGET" in
    x86_64-*)
        EXPECTED_FILE_MARKERS=("PE32+ executable" "x86-64")
        ;;
    i686-*)
        EXPECTED_FILE_MARKERS=("PE32 executable" "Intel 80386")
        ;;
    *)
        echo "❌ 不支持的 Windows 目标架构: $TARGET" >&2
        exit 1
        ;;
esac

if ! grep -Fq "${EXPECTED_FILE_MARKERS[0]}" <<<"$FILE_TYPE" || \
   ! grep -Fq "${EXPECTED_FILE_MARKERS[1]}" <<<"$FILE_TYPE"; then
    echo "❌ 产物架构与目标不符: $EXE ($FILE_TYPE)" >&2
    exit 1
fi

echo "✅ Windows 7 导入表检查通过: $TARGET"

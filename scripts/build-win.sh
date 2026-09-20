#!/usr/bin/env bash
# 交叉编译 Windows 7+ x64/x86 版 rsou
# 依赖: mingw-w64、rustup nightly + rust-src
#       sudo apt install -y \
#         gcc-mingw-w64-x86-64 binutils-mingw-w64-x86-64 \
#         gcc-mingw-w64-i686 binutils-mingw-w64-i686
set -euo pipefail
cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$PATH"

TOOLCHAIN="${RUST_TOOLCHAIN:-nightly}"
TARGETS=(x86_64-win7-windows-gnu i686-win7-windows-gnu)

# 发布产物统一放到 dist/，后缀区分架构（x64 / x32）。
DIST_DIR="${DIST_DIR:-dist}"

if ! command -v rustup >/dev/null 2>&1; then
    echo "❌ 未找到 rustup，请先安装 Rust" >&2
    exit 1
fi

# Win7 target 没有可下载的预编译标准库，必须从 rust-src 构建。
if ! rustup run "$TOOLCHAIN" rustc --version >/dev/null 2>&1; then
    rustup toolchain install "$TOOLCHAIN" --profile minimal
fi
rustup component add rust-src --toolchain "$TOOLCHAIN"

# 不执行 rustup target add：这些 Tier 3 target 没有独立的 std 组件。
for TARGET in "${TARGETS[@]}"; do
    case "$TARGET" in
        x86_64-*)
            LINKER=x86_64-w64-mingw32-gcc
            DEFAULT_WINDRES=x86_64-w64-mingw32-windres
            ARCH_SUFFIX=x64
            ;;
        i686-*)
            LINKER=i686-w64-mingw32-gcc
            DEFAULT_WINDRES=i686-w64-mingw32-windres
            ARCH_SUFFIX=x32
            ;;
        *)
            echo "❌ 不支持的 Windows 目标: $TARGET" >&2
            exit 1
            ;;
    esac
    if ! command -v "$LINKER" >/dev/null 2>&1; then
        echo "❌ 未找到 $LINKER，请安装对应的 mingw-w64 工具链" >&2
        exit 1
    fi

    # windres 资源对象按架构分别生成；调用者显式设置 WINDRES 时尊重其选择。
    WINDRES_BIN="${WINDRES:-$DEFAULT_WINDRES}"
    if ! command -v "$WINDRES_BIN" >/dev/null 2>&1; then
        echo "❌ 未找到 $WINDRES_BIN，请安装对应的 mingw-w64 工具链" >&2
        exit 1
    fi

    echo "=== 构建 $TARGET ==="
    WINDRES="$WINDRES_BIN" cargo +"$TOOLCHAIN" -Z build-std=std,panic_unwind \
        build --release --locked --target "$TARGET"

    # 保留 Cargo 原始产物，另行复制带架构后缀的发布文件。
    EXE="target/$TARGET/release/rsou.exe"
    scripts/check-win7-imports.sh "$EXE" "$TARGET"

    mkdir -p "$DIST_DIR"
    OUT="$DIST_DIR/rsou-win-$ARCH_SUFFIX.exe"
    cp -f "$EXE" "$OUT"
    echo "📦 发布产物: $OUT ($(du -h "$OUT" | cut -f1))"
done

echo ""
for TARGET in "${TARGETS[@]}"; do
    case "$TARGET" in
        x86_64-*) ARCH_SUFFIX=x64 ;;
        i686-*) ARCH_SUFFIX=x32 ;;
    esac
    EXE="target/$TARGET/release/rsou.exe"
    echo "✅ 构建完成: $EXE ($(du -h "$EXE" | cut -f1))"
    echo "   发布产物: $DIST_DIR/rsou-win-$ARCH_SUFFIX.exe"
done
echo "   目标系统: Windows 7 及以上；架构: x64、x32(i686)"
echo "   运行验证: 复制到 Windows 后双击"

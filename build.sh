#!/usr/bin/env bash
set -euo pipefail

# ─── 配置 ─────────────────────────────────
PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY_NAME="agentkernel-mcp-framework"
VERSION="$(grep '^version' "$PROJECT_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)"/\1/')"

usage() {
  echo "用法: ./build.sh [选项]"
  echo ""
  echo "选项:"
  echo "  (无)          编译本机版本"
  echo "  --linux       Linux x86_64 静态二进制 (musl)"
  echo "  --linux-arm   Linux aarch64 静态二进制 (musl)"
  echo "  --windows     Windows x86_64 exe (mingw)"
  echo "  --macos-arm   macOS aarch64 (Apple Silicon)"
  echo "  --all         全平台编译"
  echo "  -o <dir>      自定义输出目录 (默认 dist/)"
  echo "  -h            显示帮助"
  exit 0
}

# ─── 参数解析 ─────────────────────────────
BUILD_NATIVE=true
BUILD_LINUX=false
BUILD_LINUX_ARM=false
BUILD_WINDOWS=false
BUILD_MACOS_ARM=false
OUTPUT_DIR="$PROJECT_DIR/dist"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --linux)      BUILD_NATIVE=false; BUILD_LINUX=true; shift ;;
    --linux-arm)  BUILD_NATIVE=false; BUILD_LINUX_ARM=true; shift ;;
    --windows)    BUILD_NATIVE=false; BUILD_WINDOWS=true; shift ;;
    --macos-arm)  BUILD_NATIVE=false; BUILD_MACOS_ARM=true; shift ;;
    --all)        BUILD_LINUX=true; BUILD_LINUX_ARM=true; BUILD_WINDOWS=true; BUILD_MACOS_ARM=true; shift ;;
    -o)           OUTPUT_DIR="$2"; shift 2 ;;
    -h|--help)    usage ;;
    *)            echo "未知参数: $1"; usage ;;
  esac
done

cd "$PROJECT_DIR"
mkdir -p "$OUTPUT_DIR"

build_ok=0

# ─── 本机编译 ─────────────────────────────
if [ "$BUILD_NATIVE" = true ]; then
  echo "🔨 编译本机版本 (v${VERSION})"
  cargo build --release

  BIN="target/release/$BINARY_NAME"
  if [ ! -f "$BIN" ]; then echo "❌ 编译失败"; exit 1; fi
  cp "$BIN" "$OUTPUT_DIR/${BINARY_NAME}-v${VERSION}"
  chmod +x "$OUTPUT_DIR/${BINARY_NAME}-v${VERSION}"
  SIZE=$(du -h "$OUTPUT_DIR/${BINARY_NAME}-v${VERSION}" | cut -f1)
  echo "✅ 本机版本: $OUTPUT_DIR/${BINARY_NAME}-v${VERSION} ($SIZE)"
  echo ""
  build_ok=$((build_ok+1))
fi

# ─── Linux x86_64 ────────────────────────
if [ "$BUILD_LINUX" = true ]; then
  TARGET="x86_64-unknown-linux-musl"
  OUT="$OUTPUT_DIR/${BINARY_NAME}-v${VERSION}-linux-amd64"

  echo "🔨 交叉编译 Linux amd64 ($TARGET) v${VERSION}"
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc \
  CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc \
  cargo build --release --target "$TARGET"

  BIN="target/$TARGET/release/$BINARY_NAME"
  if [ ! -f "$BIN" ]; then echo "❌ Linux 编译失败"; exit 1; fi
  cp "$BIN" "$OUT"
  chmod +x "$OUT"
  SIZE=$(du -h "$OUT" | cut -f1)
  echo "✅ Linux amd64: $OUT ($SIZE)"
  echo ""
  build_ok=$((build_ok+1))
fi

# ─── Linux aarch64 ────────────────────────
if [ "$BUILD_LINUX_ARM" = true ]; then
  TARGET="aarch64-unknown-linux-musl"
  OUT="$OUTPUT_DIR/${BINARY_NAME}-v${VERSION}-linux-arm64"

  echo "🔨 交叉编译 Linux arm64 ($TARGET) v${VERSION}"
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc \
  CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc \
  cargo build --release --target "$TARGET"

  BIN="target/$TARGET/release/$BINARY_NAME"
  if [ ! -f "$BIN" ]; then echo "❌ Linux arm64 编译失败"; exit 1; fi
  cp "$BIN" "$OUT"
  chmod +x "$OUT"
  SIZE=$(du -h "$OUT" | cut -f1)
  echo "✅ Linux arm64: $OUT ($SIZE)"
  echo ""
  build_ok=$((build_ok+1))
fi

# ─── macOS aarch64 ────────────────────────
if [ "$BUILD_MACOS_ARM" = true ]; then
  TARGET="aarch64-apple-darwin"
  OUT="$OUTPUT_DIR/${BINARY_NAME}-v${VERSION}-macos-arm64"

  echo "🔨 交叉编译 macOS arm64 ($TARGET) v${VERSION}"
  cargo build --release --target "$TARGET"

  BIN="target/$TARGET/release/$BINARY_NAME"
  if [ ! -f "$BIN" ]; then echo "❌ macOS arm64 编译失败"; exit 1; fi
  cp "$BIN" "$OUT"
  chmod +x "$OUT"
  SIZE=$(du -h "$OUT" | cut -f1)
  echo "✅ macOS arm64: $OUT ($SIZE)"
  echo ""
  build_ok=$((build_ok+1))
fi

# ─── Windows x86_64 ──────────────────────
if [ "$BUILD_WINDOWS" = true ]; then
  TARGET="x86_64-pc-windows-gnu"
  OUT="$OUTPUT_DIR/${BINARY_NAME}-v${VERSION}-windows-amd64.exe"

  echo "🔨 交叉编译 Windows amd64 ($TARGET) v${VERSION}"
  CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
  cargo build --release --target "$TARGET"

  BIN="target/$TARGET/release/$BINARY_NAME.exe"
  if [ ! -f "$BIN" ]; then echo "❌ Windows 编译失败"; exit 1; fi
  cp "$BIN" "$OUT"
  SIZE=$(du -h "$OUT" | cut -f1)
  echo "✅ Windows amd64: $OUT ($SIZE)"
  echo ""
  build_ok=$((build_ok+1))
fi

# ─── 完成 ─────────────────────────────────
echo "📦 输出目录: $OUTPUT_DIR  ($build_ok 个构建)"
echo "   版本: v${VERSION}"
ls -lh "$OUTPUT_DIR"/${BINARY_NAME}* 2>/dev/null

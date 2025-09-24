#!/usr/bin/env bash
set -euo pipefail

TARGET="x86_64-unknown-linux-musl"

if ! rustup target list --installed | grep -q "^${TARGET}$"; then
  echo "Installing ${TARGET} target via rustup..." >&2
  rustup target add "${TARGET}"
fi

if ! command -v musl-gcc >/dev/null 2>&1; then
  cat >&2 <<'MSG'
error: musl-gcc is required to build a portable Linux binary.

Install the musl toolchain from your distribution (e.g. `apt install musl-tools`)
and ensure `musl-gcc` is available on the PATH.
MSG
  exit 1
fi

export PKG_CONFIG_ALLOW_CROSS=1

cargo build --release --target "${TARGET}" "$@"

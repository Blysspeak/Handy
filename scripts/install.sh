#!/usr/bin/env bash
# Builds diktor and installs it side by side with earlier installs:
#   ~/.local/opt/diktor/<yyyymmdd>-<commit>/{bin/diktor, lib/diktor/*.so*, share/diktor/}
# then points ~/.local/bin/diktor at it. Installs the systemd user unit and the
# example config only when they are missing; restarts nothing.
# Build environment that works on this machine (OpenBLAS + Vulkan):
#   TRANSCRIBE_CMAKE_ARGS=-DBLA_VENDOR=OpenBLAS CARGO_BUILD_JOBS=2 CMAKE_BUILD_PARALLEL_LEVEL=2
set -euo pipefail
crate=$(cd "$(dirname "$0")/.." && pwd)
cd "$crate"
git diff --quiet HEAD -- . || { echo "install.sh: commit your changes first ($crate)" >&2; exit 1; }
commit=$(git rev-parse --short=12 HEAD)
export TRANSCRIBE_CMAKE_ARGS=${TRANSCRIBE_CMAKE_ARGS:--DBLA_VENDOR=OpenBLAS}
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2} CMAKE_BUILD_PARALLEL_LEVEL=${CMAKE_BUILD_PARALLEL_LEVEL:-2}
cargo build --release --locked

binary=target/release/diktor
libdir=$(dirname "$(ldd "$binary" | awk '/libtranscribe/ {print $3}')")
[ -f "$libdir/libggml-vulkan.so" ] || { echo "install.sh: ggml modules not found in $libdir" >&2; exit 1; }

prefix=$HOME/.local/opt/diktor/$(date +%Y%m%d)-$commit
rm -rf "$prefix.tmp"
mkdir -p "$prefix.tmp/bin" "$prefix.tmp/lib/diktor" "$prefix.tmp/share/diktor"
install -m 0755 "$binary" "$prefix.tmp/bin/diktor"
cp -a "$libdir"/lib*.so* "$prefix.tmp/lib/diktor/"
install -m 0644 resources/silero_vad_v4.onnx "$prefix.tmp/share/diktor/"
install -m 0644 contrib/config.example.toml contrib/systemd/diktor.service "$prefix.tmp/share/diktor/"
rm -rf "$prefix"
mv "$prefix.tmp" "$prefix"

# The installed binary must resolve its libraries from its own prefix.
if ldd "$prefix/bin/diktor" | grep -q "not found"; then
  echo "install.sh: unresolved libraries" >&2; ldd "$prefix/bin/diktor" >&2; exit 1
fi
resolved=$(readlink -f "$(ldd "$prefix/bin/diktor" | awk '/libtranscribe/ {print $3}')")
[ "$(dirname "$resolved")" = "$prefix/lib/diktor" ] \
  || { echo "install.sh: libtranscribe taken from $resolved, not $prefix" >&2; exit 1; }

mkdir -p "$HOME/.local/bin"
ln -sfn "$prefix/bin/diktor" "$HOME/.local/bin/diktor"

config=${XDG_CONFIG_HOME:-$HOME/.config}/diktor/config.toml
[ -e "$config" ] || install -D -m 0644 contrib/config.example.toml "$config"
unit=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/diktor.service
[ -e "$unit" ] || install -D -m 0644 contrib/systemd/diktor.service "$unit"

echo "installed $prefix"
"$HOME/.local/bin/diktor" --version

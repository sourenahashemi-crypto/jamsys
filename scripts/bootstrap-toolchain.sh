#!/usr/bin/env bash
# Re-stage the local Rust + C toolchain into a DURABLE location so it survives
# temp-directory cleanup between sessions.
set -euo pipefail
TC="$HOME/.cache/jamsys-toolchain"
mkdir -p "$TC"/{debs,prefix,bin}
cd "$TC/debs"
if [ ! -x "$TC/prefix/usr/bin/gcc-15" ]; then
  apt-get download gcc-15 cpp-15 libgcc-15-dev gcc-15-x86-64-linux-gnu \
      cpp-15-x86-64-linux-gnu binutils binutils-x86-64-linux-gnu binutils-common 2>&1 | tail -2
  for d in *.deb; do dpkg-deb -x "$d" "$TC/prefix/"; done
fi
ln -sf "$TC/prefix/usr/bin/gcc-15" "$TC/bin/gcc"
ln -sf "$TC/prefix/usr/bin/gcc-15" "$TC/bin/cc"
export PATH="$TC/bin:$TC/prefix/usr/bin:$PATH"
echo 'int main(void){return 0;}' > /tmp/hw2.c && cc /tmp/hw2.c -o /tmp/hw2 && echo "C toolchain OK"
export RUSTUP_HOME="$TC/rustup" CARGO_HOME="$TC/cargo"
if [ ! -x "$TC/cargo/bin/cargo" ]; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path 2>&1 | tail -3
fi
"$TC/cargo/bin/rustc" --version
"$TC/cargo/bin/cargo" --version
echo "STAGED OK at $TC"

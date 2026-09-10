#!/usr/bin/env bash
# Source this to get the locally-staged Rust + C toolchain used on a machine that has
# neither installed and no passwordless sudo. On a normal machine just
# `apt install build-essential` and `rustup`, and you will not need this file.
#
# The prefix lives under ~/.cache so it survives temp-directory cleanup.
TC="${JAMSYS_TOOLCHAIN:-$HOME/.cache/jamsys-toolchain}"
if [ -x "$TC/cargo/bin/cargo" ]; then
    export RUSTUP_HOME="$TC/rustup"
    export CARGO_HOME="$TC/cargo"
    export PATH="$TC/bin:$TC/prefix/usr/bin:$CARGO_HOME/bin:$PATH"
fi
# Keep build artefacts out of the source tree but in a durable place.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/jamsys-target}"
command -v cargo >/dev/null || echo "devenv: cargo not found; run scripts/bootstrap-toolchain.sh" >&2

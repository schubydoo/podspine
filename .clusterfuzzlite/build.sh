#!/bin/bash -eu
# Build every cargo-fuzz harness in fuzz/ and copy the libFuzzer binaries to
# $OUT for ClusterFuzzLite. Runs inside the base-builder-rust image (which sets
# the sanitizer/coverage flags via $RUSTFLAGS / $SANITIZER).

cd "$SRC/podspine"

# `cargo fuzz build` respects OSS-Fuzz's sanitizer env; -O for release opt.
cargo fuzz build -O

FUZZ_TARGET_OUTPUT_DIR="fuzz/target/x86_64-unknown-linux-gnu/release"
for target in parse_cue parse_ffmeta parse_probe_json; do
    cp "${FUZZ_TARGET_OUTPUT_DIR}/${target}" "${OUT}/"
done

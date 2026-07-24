# ClusterFuzzLite build image. The OSS-Fuzz Rust base ships a nightly toolchain
# + cargo-fuzz + the sanitizer runtimes; we only add the source and the build
# script. https://google.github.io/clusterfuzzlite/build-integration/rust/
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:8a5b2b370c7778ce0190986e487dde952aa1d80496721f5647a3f11838a00a1c

# ffmpeg/ffprobe are NOT needed here: the harnesses fuzz pure parsers
# (crates/chapters, crates/prober::parse_probe_json), no subprocess.
COPY . $SRC/podspine
WORKDIR $SRC/podspine
COPY .clusterfuzzlite/build.sh $SRC/build.sh

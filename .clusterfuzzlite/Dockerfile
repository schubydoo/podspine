# ClusterFuzzLite build image. The OSS-Fuzz Rust base ships a nightly
# toolchain + cargo-fuzz + the sanitizer runtimes; this file only adds the
# source and the build script.
# https://google.github.io/clusterfuzzlite/build-integration/rust/
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:9178c8abf83e7a82d02c1a9bf7b2f564320f669f7a1cec512ae9a8c4ea078427

# ffmpeg/ffprobe are NOT needed here: the harnesses fuzz pure parsers
# (crates/chapters, crates/prober::parse_probe_json), with no subprocess.
COPY . $SRC/podspine
WORKDIR $SRC/podspine
COPY .clusterfuzzlite/build.sh $SRC/build.sh

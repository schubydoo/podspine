# ClusterFuzzLite build image. The OSS-Fuzz Rust base ships a nightly
# toolchain + cargo-fuzz + the sanitizer runtimes; this file only adds the
# source and the build script.
# https://google.github.io/clusterfuzzlite/build-integration/rust/
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:4de23732b37444c5bfa324ae9f3f7e51d45a975d609edddaae01a87b10d5f615

# ffmpeg/ffprobe are NOT needed here: the harnesses fuzz pure parsers
# (crates/chapters, crates/prober::parse_probe_json), with no subprocess.
COPY . $SRC/podspine
WORKDIR $SRC/podspine
COPY .clusterfuzzlite/build.sh $SRC/build.sh

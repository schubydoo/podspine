# ClusterFuzzLite build image. The OSS-Fuzz Rust base ships a nightly toolchain
# + cargo-fuzz + the sanitizer runtimes; we only add the source and the build
# script. https://google.github.io/clusterfuzzlite/build-integration/rust/
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:8822b56a21478614ed181d183683451e4f7b959d0a34043d95fc8b9f7fcc85f9

# ffmpeg/ffprobe are NOT needed here: the harnesses fuzz pure parsers
# (crates/chapters, crates/prober::parse_probe_json), no subprocess.
COPY . $SRC/podspine
WORKDIR $SRC/podspine
COPY .clusterfuzzlite/build.sh $SRC/build.sh

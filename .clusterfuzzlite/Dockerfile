# ClusterFuzzLite build image. The OSS-Fuzz Rust base ships a nightly toolchain
# + cargo-fuzz + the sanitizer runtimes; we only add the source and the build
# script. https://google.github.io/clusterfuzzlite/build-integration/rust/
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:8d1f850f48563880d58178389ac6e1bc2de8074eca136981216ac26e8159b9af

# ffmpeg/ffprobe are NOT needed here: the harnesses fuzz pure parsers
# (crates/chapters, crates/prober::parse_probe_json), no subprocess.
COPY . $SRC/podspine
WORKDIR $SRC/podspine
COPY .clusterfuzzlite/build.sh $SRC/build.sh

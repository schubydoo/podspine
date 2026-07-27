# ClusterFuzzLite build image. The OSS-Fuzz Rust base ships a nightly toolchain
# + cargo-fuzz + the sanitizer runtimes; we only add the source and the build
# script. https://google.github.io/clusterfuzzlite/build-integration/rust/
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:832ab4da13b86f85a765241c93337a6f2f65196349b26acec9234a4eb8a26e26

# ffmpeg/ffprobe are NOT needed here: the harnesses fuzz pure parsers
# (crates/chapters, crates/prober::parse_probe_json), no subprocess.
COPY . $SRC/podspine
WORKDIR $SRC/podspine
COPY .clusterfuzzlite/build.sh $SRC/build.sh

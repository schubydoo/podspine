# ClusterFuzzLite build image. The OSS-Fuzz Rust base ships a nightly
# toolchain + cargo-fuzz + the sanitizer runtimes; this file only adds the
# source and the build script.
# https://google.github.io/clusterfuzzlite/build-integration/rust/
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:9643b583ef5aa3d956b3c170e93ef8283878c70c6df8c99d8a70d73c5019677f

# ffmpeg/ffprobe are NOT needed here: the harnesses fuzz pure parsers
# (crates/chapters, crates/prober::parse_probe_json), with no subprocess.
COPY . $SRC/podspine
WORKDIR $SRC/podspine
COPY .clusterfuzzlite/build.sh $SRC/build.sh

# ClusterFuzzLite build image. The OSS-Fuzz Rust base ships a nightly toolchain
# + cargo-fuzz + the sanitizer runtimes; we only add the source and the build
# script. https://google.github.io/clusterfuzzlite/build-integration/rust/
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:7ab3ee834d724879ced8c4617e2b3f884d0248e3f10888e2f120a95f5fbfe4cd

# ffmpeg/ffprobe are NOT needed here: the harnesses fuzz pure parsers
# (crates/chapters, crates/prober::parse_probe_json), no subprocess.
COPY . $SRC/podspine
WORKDIR $SRC/podspine
COPY .clusterfuzzlite/build.sh $SRC/build.sh

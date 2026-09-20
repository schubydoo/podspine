# ClusterFuzzLite build image. The OSS-Fuzz Rust base ships a nightly
# toolchain + cargo-fuzz + the sanitizer runtimes; this file only adds the
# source and the build script.
# https://google.github.io/clusterfuzzlite/build-integration/rust/
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:923f529e6aacda622ac6470456b9ba97448209ab7cb7f38b96a338851eee7acf

# ffmpeg/ffprobe are NOT needed here: the harnesses fuzz pure parsers
# (crates/chapters, crates/prober::parse_probe_json), with no subprocess.
COPY . $SRC/podspine
WORKDIR $SRC/podspine
COPY .clusterfuzzlite/build.sh $SRC/build.sh

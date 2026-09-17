# Homebrew formula for the signed standalone podspine binary.
#
#   brew install schubydoo/podspine/podspine
#
# Covers the published macOS (arm64 + Intel) and Linux (amd64 + arm64) binaries.
# Windows installs via the Scoop bucket. Version + checksums are auto-bumped per
# release by packaging-bump.yml from the release checksums.txt.
class Podspine < Formula
  desc "Self-hosted server that turns audiobooks into per-chapter podcast feeds"
  homepage "https://github.com/schubydoo/podspine"
  version "1.8.0"
  license "AGPL-3.0-only"

  # Podspine shells out to ffmpeg/ffprobe at runtime.
  depends_on "ffmpeg"

  on_macos do
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.8.0/podspine-v1.8.0-darwin-arm64"
      sha256 "be49ae9a50bf1f0beb5bb9caa64e6b444133707a48a9f5e86627b109cf4eeba4"
    end
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.8.0/podspine-v1.8.0-darwin-amd64"
      sha256 "59eadb712c466e709e6797daa9f040712b2d3ed51cb2fe3e98dad4b64ef3f578"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.8.0/podspine-v1.8.0-linux-amd64"
      sha256 "17a691b1edf7f189d733cdaa99f2c88c037c8e3e98dfeaa57421e1e2ed327830"
    end
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.8.0/podspine-v1.8.0-linux-arm64"
      sha256 "5a97d127db6d7ea65188ab6e25f85f3d5aa246681f410ff68b9157b9fe3218ec"
    end
  end

  def install
    # The release asset downloads under its versioned name; install it as `podspine`.
    bin.install Dir["podspine-*"].first => "podspine"
  end

  test do
    # `--help` (not `--version`): the pinned release may predate the --version flag,
    # so assert the binary runs and identifies itself rather than a version string.
    assert_match "podspine", shell_output("#{bin}/podspine --help")
  end
end

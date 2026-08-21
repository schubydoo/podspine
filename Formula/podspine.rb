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
  version "1.7.1"
  license "AGPL-3.0-only"

  # Podspine shells out to ffmpeg/ffprobe at runtime.
  depends_on "ffmpeg"

  on_macos do
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.7.1/podspine-v1.7.1-darwin-arm64"
      sha256 "d718ee11a565072fc8fade36c3cc0f177224df95f9417d2e45de8b0e72be9206"
    end
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.7.1/podspine-v1.7.1-darwin-amd64"
      sha256 "3a8472da80898fcaa91ad9ff60e81aafb0def4ea82f597642e6061c4cac6e013"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.7.1/podspine-v1.7.1-linux-amd64"
      sha256 "c4dc58b765a4172e102554e14cd19fe79d1d4a73102f5de30e4f668ec184ba62"
    end
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.7.1/podspine-v1.7.1-linux-arm64"
      sha256 "78f2d63b06687f1ae6b7e4c78d60b133fb347ed1c2ce11bf373d3783668b1eb8"
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

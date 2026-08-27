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
  version "1.7.2"
  license "AGPL-3.0-only"

  # Podspine shells out to ffmpeg/ffprobe at runtime.
  depends_on "ffmpeg"

  on_macos do
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.7.2/podspine-v1.7.2-darwin-arm64"
      sha256 "9375d2499ee5ebf73344fcdbf18e337ef99dbe5a284430dcbc8395da4e0b44c9"
    end
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.7.2/podspine-v1.7.2-darwin-amd64"
      sha256 "d7f438fc9db46528152fa6d2adb5cb6c69785fe742ac7d28b3acc098cbbd0dd5"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.7.2/podspine-v1.7.2-linux-amd64"
      sha256 "f695c7556ded9d327f6e005d0a043af016cf50575ea4d915e69b126c0fcc03a5"
    end
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.7.2/podspine-v1.7.2-linux-arm64"
      sha256 "611952683f9bd7df6671017d644f1cf2ea1501c70be1b203bed9e9213b2ce076"
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

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
  version "1.7.0"
  license "AGPL-3.0-only"

  # Podspine shells out to ffmpeg/ffprobe at runtime.
  depends_on "ffmpeg"

  on_macos do
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.7.0/podspine-v1.7.0-darwin-arm64"
      sha256 "8f4dbac9946a11b24c476f3362fafd60dbecd23eeb95f820dc4b2f76c8243e62"
    end
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.7.0/podspine-v1.7.0-darwin-amd64"
      sha256 "fa7a7c02a81ae2c4f473460113066028bef7402cf8c9c2868739cd8885d5f40a"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.7.0/podspine-v1.7.0-linux-amd64"
      sha256 "94f30c209222dfb132f8d41e767e0b26cfb56d41bd15a620af38e11c96a05dfe"
    end
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.7.0/podspine-v1.7.0-linux-arm64"
      sha256 "290071af509c0ff345424107a212d8498bfe6056c716c71c98d8274be6d0fef4"
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

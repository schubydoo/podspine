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
  version "1.2.0"
  license "AGPL-3.0-only"

  # Podspine shells out to ffmpeg/ffprobe at runtime.
  depends_on "ffmpeg"

  on_macos do
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.2.0/podspine-v1.2.0-darwin-arm64"
      sha256 "7d98b149a1671120493cfa46d5325022244e92fca98ff859db92d77a4cc48351"
    end
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.2.0/podspine-v1.2.0-darwin-amd64"
      sha256 "8e71b1b6385a2698267dbec949e08429b4ab1711dd017df81492e1bbb46b375f"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.2.0/podspine-v1.2.0-linux-amd64"
      sha256 "bd49a60890260b0e731451ba8d4461b6d242960e1033c60bf5ac9c5b5dc9d7f1"
    end
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.2.0/podspine-v1.2.0-linux-arm64"
      sha256 "d10992680d26361f8626a1fa11b065630d241a90686327edf7150500d6ce5206"
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

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
  version "1.6.0"
  license "AGPL-3.0-only"

  # Podspine shells out to ffmpeg/ffprobe at runtime.
  depends_on "ffmpeg"

  on_macos do
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.6.0/podspine-v1.6.0-darwin-arm64"
      sha256 "ccb84ae583cf9d0beb4b8b926e07c9413b05ff32ed0672bf70a380e7237187d2"
    end
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.6.0/podspine-v1.6.0-darwin-amd64"
      sha256 "9da453034b3eae2659f47dfc07c0a7378ceebcc0c161c73857d369b049639ba3"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.6.0/podspine-v1.6.0-linux-amd64"
      sha256 "9f12a9a11309c174f3bfb0f408cad539247b5e17f2d6b16269c50ec838a0cdc1"
    end
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.6.0/podspine-v1.6.0-linux-arm64"
      sha256 "0e03861ac4e4d74126a62c82671b9706d2dcb34c41291235d64a8b866dfb9a92"
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

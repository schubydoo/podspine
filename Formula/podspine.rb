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
  version "1.4.0"
  license "AGPL-3.0-only"

  # Podspine shells out to ffmpeg/ffprobe at runtime.
  depends_on "ffmpeg"

  on_macos do
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.4.0/podspine-v1.4.0-darwin-arm64"
      sha256 "b9b19f38daebd129d164a9acf1be1e2974a612f594a963d1f5603b4f668a0a3a"
    end
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.4.0/podspine-v1.4.0-darwin-amd64"
      sha256 "a7cf2880ab32287b1271a2982fa0bbc6a177b4b0eb0e1b824014e60dc2cf7a16"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.4.0/podspine-v1.4.0-linux-amd64"
      sha256 "1acd66df595d1e6a7c87aaa0ff480cf66caf28bf4c992f1fd71c65be7a7b8979"
    end
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.4.0/podspine-v1.4.0-linux-arm64"
      sha256 "75ed68491211709684b97296f64cd1ec3083361439aa7dc02bcc8c77767a55e3"
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

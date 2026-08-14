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
  version "1.5.0"
  license "AGPL-3.0-only"

  # Podspine shells out to ffmpeg/ffprobe at runtime.
  depends_on "ffmpeg"

  on_macos do
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.5.0/podspine-v1.5.0-darwin-arm64"
      sha256 "462a290282ac1921a104a118e62f750558b696aa50a2639b5507ff0496e2d00b"
    end
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.5.0/podspine-v1.5.0-darwin-amd64"
      sha256 "710ebdff9615338c02882e856eee60f78414eb1a813c59a27ae7560cbfc79d57"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.5.0/podspine-v1.5.0-linux-amd64"
      sha256 "98f1929035ff065002cae723a23d726f7dbfb5f4fa2ab2138345064f54adef7a"
    end
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.5.0/podspine-v1.5.0-linux-arm64"
      sha256 "fbd8abe7da3f3858c498877f6ebf4208d5f2c3d227b70e8f2d7d21663d045ee9"
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

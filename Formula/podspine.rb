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
  version "1.9.0"
  license "AGPL-3.0-only"

  # Podspine shells out to ffmpeg/ffprobe at runtime.
  depends_on "ffmpeg"

  on_macos do
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.9.0/podspine-v1.9.0-darwin-arm64"
      sha256 "914c942baf941e26c0398ab82b84669d09bf4e26f7d83b1a450b489a18569bb6"
    end
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.9.0/podspine-v1.9.0-darwin-amd64"
      sha256 "396ef1e22c98ef16b69d5fdedef1e4ae3e7ca6bebec7a45673c3064d3b68f4fa"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.9.0/podspine-v1.9.0-linux-amd64"
      sha256 "604e7a33283184c8db2a6328e26d0d9fb4d48b477215cc06d477477022bf65f9"
    end
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.9.0/podspine-v1.9.0-linux-arm64"
      sha256 "ba90f0b4e9d7e0f99f304c85a5634150f2b3bbd68fa2d13b671ec3f3642121bd"
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

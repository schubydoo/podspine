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
  version "1.3.0"
  license "AGPL-3.0-only"

  # Podspine shells out to ffmpeg/ffprobe at runtime.
  depends_on "ffmpeg"

  on_macos do
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.3.0/podspine-v1.3.0-darwin-arm64"
      sha256 "24029b8be687b43e648d7603c4cb1afcc419e752de57f7d54fc11034d16657a6"
    end
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.3.0/podspine-v1.3.0-darwin-amd64"
      sha256 "66d6a33a0e4d21aad4be7e1a9a5aa98baf8dc744ae0c737151cfe0bf948b9198"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.3.0/podspine-v1.3.0-linux-amd64"
      sha256 "0b7a9776ca45f7e6ac6c05a485463e85a248758b85123815cc4b343e40419326"
    end
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.3.0/podspine-v1.3.0-linux-arm64"
      sha256 "f5d93c61f290833f3520235b18a05db654a4780acbdd8dfb25d04e949dd701fe"
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

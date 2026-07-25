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
  version "1.4.1"
  license "AGPL-3.0-only"

  # Podspine shells out to ffmpeg/ffprobe at runtime.
  depends_on "ffmpeg"

  on_macos do
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.4.1/podspine-v1.4.1-darwin-arm64"
      sha256 "59b7971fb959e18e8879ee66557d92b59b027051c45c680200fe554018c47db5"
    end
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.4.1/podspine-v1.4.1-darwin-amd64"
      sha256 "cd3dcb939f752eda396a1cd2260ccbc5eeeeb7b38af7dff1db77042843e1dd9c"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.4.1/podspine-v1.4.1-linux-amd64"
      sha256 "f994107d71d1674fdf86572f7a10a20e77381e4225b9895d89235dd1198ce2d8"
    end
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.4.1/podspine-v1.4.1-linux-arm64"
      sha256 "63da02aae09dbdfce37784e9e3907829e0c47adce4eca5986473ef3da5576478"
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

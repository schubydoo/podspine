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
  version "1.1.0"
  license "AGPL-3.0-only"

  # Podspine shells out to ffmpeg/ffprobe at runtime.
  depends_on "ffmpeg"

  on_macos do
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.1.0/podspine-v1.1.0-darwin-arm64"
      sha256 "00e10ce18fd3e009df9a50cf78614b20d90c1b342e93de8ccc1b212bcd94072b"
    end
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.1.0/podspine-v1.1.0-darwin-amd64"
      sha256 "ebae811e7a5649470eb6022c8399cf9869f11d6a8a6f5ebbf334986e91b519bf"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/schubydoo/podspine/releases/download/v1.1.0/podspine-v1.1.0-linux-amd64"
      sha256 "fc1b8afa24c35010ac7cbfb49d62cfa659cfb9c69af0b65e110dbb3375a64f63"
    end
    on_arm do
      url "https://github.com/schubydoo/podspine/releases/download/v1.1.0/podspine-v1.1.0-linux-arm64"
      sha256 "d129ae3c3355eb8a0c1dfc17156e1c0870d625a4ec8d94f1124cc76fabf16005"
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

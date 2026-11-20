class Lazykatok < Formula
  desc "Local KakaoTalk keyword, BM25, and vector search CLI for Apple Silicon macOS"
  homepage "https://github.com/changeroa/lazykatok"
  url "https://github.com/changeroa/lazykatok.git",
    tag:      "v0.3.4",
    revision: "06bff71adc6f652baf1f30d4aaf57eee8efc385d"
  license "MIT"

  depends_on "rust" => :build
  depends_on arch: :arm64

  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      For native KakaoTalk sync, grant your terminal Full Disk Access:
        System Settings > Privacy & Security > Full Disk Access

      Then run:
        lazykatok doctor --json
    EOS
  end

  test do
    assert_match "lazykatok", shell_output("#{bin}/lazykatok --help")
  end
end

class Macdedup < Formula
  desc "Reclaim disk space on macOS using APFS space-saving clones"
  homepage "https://github.com/nateProjects/MacDeDup"
  url "https://github.com/nateProjects/MacDeDup/releases/download/v1.1.2/MacDeDup-v1.1.2-macos-universal.tar.gz"
  sha256 "bad758e33bc6a0ff90d2dc4b239fb7076ebd799d02ff208e53a04204e2e56e00"

  depends_on :macos

  def install
    bin.install "MacDeDup"
  end

  test do
    assert_match "APFS", shell_output("#{bin}/MacDeDup --help 2>&1")
  end
end

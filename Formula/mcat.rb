# mcat 的 Homebrew formula 模板。
# release workflow（.github/workflows/release.yml）把 __*__ 占位符填成真实值后
# 推到 argsno/homebrew-tap 的 Formula/mcat.rb，不要直接用这份文件安装——
# 占位符不是真实的 tag 和校验和。
class Mcat < Formula
  desc "A cat that renders Markdown in the terminal"
  homepage "https://github.com/argsno/mcat"
  license "MIT"
  version "__VERSION__"

  # 二进制分发，按架构/系统给四个象限各一份 tar.gz（都由 release workflow 构建）
  on_arm do
    on_macos do
      url "https://github.com/argsno/mcat/releases/download/__TAG__/mcat-__TAG__-aarch64-apple-darwin.tar.gz"
      sha256 "__SHA_ARM_MAC__"
    end
    on_linux do
      url "https://github.com/argsno/mcat/releases/download/__TAG__/mcat-__TAG__-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "__SHA_ARM_LINUX__"
    end
  end
  on_intel do
    on_macos do
      url "https://github.com/argsno/mcat/releases/download/__TAG__/mcat-__TAG__-x86_64-apple-darwin.tar.gz"
      sha256 "__SHA_INTEL_MAC__"
    end
    on_linux do
      url "https://github.com/argsno/mcat/releases/download/__TAG__/mcat-__TAG__-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "__SHA_INTEL_LINUX__"
    end
  end

  def install
    bin.install "mcat"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/mcat --version")
  end
end

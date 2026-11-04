class StrutStackSql < Formula
  desc "Token-efficient MCP server for MySQL, PostgreSQL, and SQLite"
  homepage "https://github.com/rzlco666/strut-stack-sql"
  version "2.0.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/rzlco666/strut-stack-sql/releases/download/v2.0.0/strut-stack-sql-macos-arm64.tar.gz"
      sha256 "UPDATE_AFTER_RELEASE"
    else
      url "https://github.com/rzlco666/strut-stack-sql/releases/download/v2.0.0/strut-stack-sql-macos-x64.tar.gz"
      sha256 "UPDATE_AFTER_RELEASE"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/rzlco666/strut-stack-sql/releases/download/v2.0.0/strut-stack-sql-linux-arm64.tar.gz"
      sha256 "UPDATE_AFTER_RELEASE"
    else
      url "https://github.com/rzlco666/strut-stack-sql/releases/download/v2.0.0/strut-stack-sql-linux-x64.tar.gz"
      sha256 "UPDATE_AFTER_RELEASE"
    end
  end

  def install
    bin.install "strut-stack-sql"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/strut-stack-sql --version")
  end
end

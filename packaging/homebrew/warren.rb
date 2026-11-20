# Homebrew formula for Warren, published through the tap
# veerareddyvishal144/homebrew-warren (Formula/warren.rb in that repository):
#
#   brew tap veerareddyvishal144/warren
#   brew install warren
#
# The formula installs the prebuilt release tarballs, so there is no Rust
# toolchain involved. The url and sha256 lines are rewritten for each release by
# scripts/release/update-formula.sh from the release's SHA256SUMS; the values
# below are placeholders until the first release is cut. How the tap is created
# and updated is in docs/install.md.
class Warren < Formula
  desc "Self-hosted agent sandboxes: persistent Linux and macOS computers behind one API"
  homepage "https://github.com/veerareddyvishal144/warren"
  version "0.1.0"
  license "Apache-2.0"

  on_macos do
    # Apple silicon only: the macOS build hosts `mac` instances through
    # Virtualization.framework, and no Intel binaries are published.
    depends_on arch: :arm64
    url "https://github.com/veerareddyvishal144/warren/releases/download/v0.1.0/warren-0.1.0-darwin-arm64.tar.gz"
    sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  end

  on_linux do
    on_intel do
      url "https://github.com/veerareddyvishal144/warren/releases/download/v0.1.0/warren-0.1.0-linux-x86_64.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_arm do
      url "https://github.com/veerareddyvishal144/warren/releases/download/v0.1.0/warren-0.1.0-linux-arm64.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    # The tarball unpacks to one directory, which Homebrew strips, leaving the
    # binaries and LICENSE in the build directory.
    bin.install "warrend", "warren", "warren-mcp", "warren-gateway"
  end

  # `brew services start warren` runs the daemon with its defaults: data under
  # #{var}/warren, the API at http://api.warren.localhost:7337. On a Mac
  # without Docker the daemon starts without the container types; add
  # WARREN_MAC_BASE_VM and WARREN_RUNTIME=none for macOS instances through a
  # launchd plist of your own (packaging/launchd/dev.warren.daemon.plist) or
  # `warren mac init`, since the service block cannot take per-user settings.
  service do
    run [opt_bin/"warrend", "--data-dir", var/"warren"]
    keep_alive true
    working_dir var/"warren"
    log_path var/"log/warrend.log"
    error_log_path var/"log/warrend.log"
    environment_variables PATH: std_service_path_env, WARREN_LOG: "info"
  end

  def caveats
    <<~EOS
      Next steps:
        warren up                 start the daemon on this machine
        warren mac init           (macOS) prepare this Mac for macOS instances
        warren connect claude     add the Warren MCP server to Claude Code

      Or run the daemon as a service: brew services start warren
      Documentation: https://github.com/veerareddyvishal144/warren/blob/main/docs/install.md
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/warren --version")
    assert_match version.to_s, shell_output("#{bin}/warrend --version")
    assert_match version.to_s, shell_output("#{bin}/warren-gateway --version")
  end
end

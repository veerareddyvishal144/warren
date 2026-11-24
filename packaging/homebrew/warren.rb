# Homebrew formula for Warren, published through the tap
# veerareddyvishal144/homebrew-warren (Formula/warren.rb in that repository):
#
#   brew tap veerareddyvishal144/warren
#   brew install warren
#
# The formula installs the prebuilt release tarballs, so there is no Rust
# toolchain involved. The url and sha256 lines are rewritten for each release by
# scripts/release/update-formula.sh from the release's SHA256SUMS. How the tap
# is created and updated is in docs/install.md. Homebrew 5 and newer ask users
# to trust a third-party tap once: `brew trust veerareddyvishal144/warren`.
class Warren < Formula
  desc "Self-hosted sandboxes for coding agents: Linux and macOS VMs, one API"
  homepage "https://github.com/veerareddyvishal144/warren"
  license "Apache-2.0"

  livecheck do
    url :stable
    strategy :github_latest
  end

  on_macos do
    # Apple silicon only: the macOS build hosts `mac` instances through
    # Virtualization.framework, and no Intel binaries are published.
    depends_on arch: :arm64
    on_arm do
      url "https://github.com/veerareddyvishal144/warren/releases/download/v0.2.0/warren-0.2.0-darwin-arm64.tar.gz"
      sha256 "3c9cbe2432f4c8db3ec009b6eff261dcaef409d741bc23ecbdf8b232153b0f40"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/veerareddyvishal144/warren/releases/download/v0.2.0/warren-0.2.0-linux-x86_64.tar.gz"
      sha256 "348fe21032b352eafca2b30d4526e03c42e0684a97f94c83cb2fea1e4783278a"
    end
    on_arm do
      url "https://github.com/veerareddyvishal144/warren/releases/download/v0.2.0/warren-0.2.0-linux-arm64.tar.gz"
      sha256 "94467d4ef8d465f16cec1c2c35a1bd7b3ce15082ebba9541f6307ceb02b3f206"
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
        warren up                       start the daemon on this machine and create your key
        warren mac init --with-agents   (macOS) build the base macOS VM, once; needs Lume
        warren connect claude           add the Warren MCP server to Claude Code
        warren run -- claude            Claude Code inside a fresh instance bound to this directory
        warren doctor                   check the host and print a fix for anything missing

      Or run the daemon as a service: brew services start warren
      Documentation: https://github.com/veerareddyvishal144/warren/blob/main/docs/quickstart.md
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/warren --version")
    assert_match version.to_s, shell_output("#{bin}/warrend --version")
    assert_match version.to_s, shell_output("#{bin}/warren-gateway --version")
  end
end

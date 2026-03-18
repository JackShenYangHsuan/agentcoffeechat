class Agentcoffeechat < Formula
  desc "Live coffee chats between nearby developers' AI coding agents"
  homepage "https://github.com/JackShenYangHsuan/agentcoffeechat"
  url "https://github.com/JackShenYangHsuan/agentcoffeechat/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "bd2a88c0aa48908e25c90e536cf4b32a775eb4727d7647b9b94110ca9d29dbcf"
  license "MIT"

  depends_on "rust" => :build
  depends_on :macos

  def install
    cd "src" do
      system "cargo", "build", "--release"
      bin.install "target/release/agentcoffeechat"
      bin.install "target/release/acc"
      bin.install "target/release/agentcoffeechat-daemon"
      bin.install "target/release/agentcoffeechat-menubar"
    end
  end

  def caveats
    <<~EOS
      AgentCoffeeChat is installed! To get started:

        1. Start the daemon:
           acc start

        2. Connect to a nearby coworker:
           Say "connect to <name>" in your Claude Code / Codex session

        3. Or use the CLI directly:
           acc peers             # see who's nearby
           acc connect <name>    # pair with a peer
           acc chat --to <name>  # start a coffee chat

      The daemon runs in the background. Start it at the beginning
      of each work session with `acc start`.

      Run `acc doctor` to verify your setup.
    EOS
  end

  test do
    assert_match "agentcoffeechat", shell_output("#{bin}/acc --version")
  end
end

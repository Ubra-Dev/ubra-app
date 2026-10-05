cask "ubra" do
  version "0.1.0"
  # Rendered from the release's GitHub asset digest when the cask is submitted.
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  url "https://github.com/Ubra-Dev/ubra-app/releases/download/v#{version}/ubra-#{version}-universal.dmg"
  name "ubra"
  desc "Terminal workspace for running coding agents in parallel"
  homepage "https://getubra.com/"

  livecheck do
    url :url
    strategy :github_latest
  end

  auto_updates true
  depends_on macos: :sequoia

  app "ubra.app"

  # ~/Library/Application Support/Ubra is intentionally not zapped: it holds
  # user-written notes, saved agent account logins and the state of sessions
  # whose agent processes keep running after the app quits.
  zap trash: [
    "~/Library/Application Support/ubra",
    "~/Library/Caches/com.ubra.ubra",
    "~/Library/Caches/ubra",
    "~/Library/HTTPStorages/com.ubra.ubra",
    "~/Library/HTTPStorages/com.ubra.ubra.binarycookies",
    "~/Library/Preferences/com.ubra.ubra.plist",
    "~/Library/Saved Application State/com.ubra.ubra.savedState",
    "~/Library/WebKit/com.ubra.ubra",
  ]
end

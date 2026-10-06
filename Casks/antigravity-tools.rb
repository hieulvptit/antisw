cask "antigravity-tools" do
  version "5.2.0"
  sha256 :no_check

  name "Antigravity Switcher"
  desc "Professional Account Management for AI Services"
  homepage "https://github.com/talk114/antisw"

  on_macos do
    url "https://github.com/talk114/antisw/releases/download/v#{version}/Antigravity.Switcher_#{version}_universal.dmg"

    app "AntigravitySW.app"

    zap trash: [
      "~/Library/Application Support/com.lbjlaq.antigravity-Switcher",
      "~/Library/Caches/com.lbjlaq.antigravity-Switcher",
      "~/Library/Preferences/com.lbjlaq.antigravity-Switcher.plist",
      "~/Library/Saved Application State/com.lbjlaq.antigravity-Switcher.savedState",
    ]

    caveats <<~EOS
      If you encounter the "App is damaged" error, please run the following command:
        sudo xattr -rd com.apple.quarantine "/Applications/Antigravity Switcher.app"

      Or install with the --no-quarantine flag:
        brew install --cask --no-quarantine antigravity-Switcher
    EOS
  end

  on_linux do
    arch arm: "aarch64", intel: "amd64"

    url "https://github.com/talk114/antisw/releases/download/v#{version}/Antigravity.Switcher_#{version}_#{arch}.AppImage"
    binary "Antigravity.Switcher_#{version}_#{arch}.AppImage", target: "antigravity-Switcher"

    preflight do
      system_command "/bin/chmod", args: ["+x", "#{staged_path}/Antigravity.Switcher_#{version}_#{arch}.AppImage"]
    end
  end
end

### Install

1. Download **Masthead-macos-arm64.dmg** below (Apple Silicon, macOS 13+), open it and drag Masthead onto Applications.
2. Open Masthead. macOS says *"Masthead" Not Opened*, because the app isn't notarized by Apple. Click **Done**, not "Move to Bin".
3. Open **System Settings → Privacy & Security**, scroll down to *"Masthead" was blocked…* and click **Open Anyway**, then confirm.

Or skip steps 2–3 with one Terminal command: `xattr -dr com.apple.quarantine /Applications/Masthead.app`

This is only needed once: Masthead updates itself from then on (the **Update** button bottom right, or Masthead → Check for Updates…).

Already installed? Update from inside the app; the `.zip` is what the updater downloads.

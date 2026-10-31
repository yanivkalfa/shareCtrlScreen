# First run

This page covers the first time ShareCtrlScreen opens on a machine. Each system shows its
own security dialogs, and the app needs some of them answered before it can share or
control the screen. Find your system below.

The dialog text quoted here is what the platform research recorded. Wording changes with
the OS version, so match the meaning of the dialog rather than the exact letters. For
installing, see the [Install section](../README.md#install) of the README.

## Windows

The first time you run the installer, Windows SmartScreen shows a warning, because the build
is not code-signed. This is expected.

1. Click **More info**.
2. Click **Run anyway**. The installer then continues as usual.

For UAC prompts, the lock screen and the sign-in screen, install the Windows service
once. Open a terminal as administrator and run:

```
sharectrl-service.exe --install-service
```

## macOS (13 Ventura or later)

1. Open the `.dmg` and drag **ShareCtrlScreen** into **Applications**. Start the app from
   there.
2. macOS blocks the first launch with a dialog that starts **"Apple could not verify"**.
   Dismiss it. The app is signed with a self-signed certificate (or ad-hoc, in development
   builds). Apple has not notarized it, so this dialog is expected.
3. Open **System Settings → Privacy & Security**. Scroll down to the message about
   ShareCtrlScreen, click **Open Anyway**, and enter your administrator password. Start
   the app again.
   - Terminal alternative: `xattr -dr com.apple.quarantine /Applications/ShareCtrlScreen.app`
4. Turn on the permissions in **System Settings → Privacy & Security**:
   - **Screen & System Audio Recording**: needed to share this Mac's screen. If you
     clicked **Don't Allow** earlier, turn it on here. macOS asks only once.
   - **Accessibility**: needed so the remote side can control this Mac.
   - **Input Monitoring**: may also be needed for shortcut passthrough from this Mac
     (unverified).
5. On macOS 15 (Sequoia) and later, macOS asks about once a month whether ShareCtrlScreen
   may bypass the system private window picker. The text is "...is requesting to bypass
   the system private window picker". Click **Allow**. A Mac that nobody is sitting at
   stops sharing until someone does.

## Linux

1. Check which session you are in. In a terminal, run `echo $XDG_SESSION_TYPE`. It prints
   `x11` or `wayland`.
2. **X11:** there are no permission dialogs. Sharing and control work straight away.
3. **Wayland:** the first share opens the desktop's consent dialog. Allow it. The dialog
   may ask **Allow remote control?**. The choice is remembered in
   `~/.local/state/sharectrl/portal.token`, so later sessions usually skip the dialog. If
   the desktop refuses the remembered choice, the dialog appears once more.
4. **GNOME:** while your screen is being shared, an orange indicator shows that the screen
   is being shared. Click it to stop sharing.
5. **Hardware video:** run `vainfo` in a terminal. It prints the VA-API version and the video
   profiles your GPU supports. Ubuntu 22.04 has a libva too old for the hardware video
   path, so there the app uses software video. The app still works.
6. **Hyprland:** the screen can be viewed but not controlled unless you turn on the uinput
   setting in Settings. That setting asks for your password once.

If something does not work, see [Known limitations](../README.md#known-limitations-v1).

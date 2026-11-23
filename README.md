# Fuzzy Proton Drive

An unofficial Rust desktop client for Proton Drive on CachyOS and Arch Linux. It uses egui and native Wayland, not a webview or XWayland. The official `proton-drive` CLI handles authentication, encryption, and cloud operations.

## Run

- Install `proton-drive` from [Proton's CLI download page](https://proton.me/download/drive/cli/) and put it on `PATH`. The app targets the **0.9.0** command and JSON interface. Set `FUZZY_PROTON_CLI` to an absolute executable path to use a different installation.
- Use a Plasma **Wayland** session with a default browser and a working Secret Service provider (e.g. KWallet configured to provide `org.freedesktop.secrets`). The GUI does not ask for a Proton password or store Proton credentials. It uses the same session and keyring as the CLI; signing out in the app also signs out the CLI.
- Install a Rust toolchain (1.88 or newer), `base-devel`, `wayland`, and an EGL/OpenGL provider such as `mesa` or the appropriate vendor driver. `ffmpeg` is needed for thumbnails, `kdialog` for the KDE folder picker, and `gio` (from `glib2` on Arch) to move local files to the desktop Trash. Paths can also be entered manually. `python` is needed for the mock CLI test.

```bash
cargo run --release --locked
```

To install the application and its KDE launcher system-wide after building:

```bash
cargo build --release --locked
sudo install -Dm755 target/release/fuzzy-proton-drive /usr/local/bin/fuzzy-proton-drive
sudo install -Dm644 packaging/io.github.fuzzy_proton_drive.desktop /usr/local/share/applications/io.github.fuzzy_proton_drive.desktop
sudo install -Dm644 packaging/io.github.fuzzy_proton_drive.svg /usr/local/share/icons/hicolor/scalable/apps/io.github.fuzzy_proton_drive.svg
```

Plasma matches the taskbar icon to the installed desktop entry by Wayland app ID. Install the launcher and icon even if you normally start the binary from a terminal.

## Browse and share

Sign in using the default browser, or reuse your existing CLI session. Browse **My files** from the expandable directory tree. Switch between **List** and **Details**, sort by name, size, or modified date, and filter within the current directory. Right-click an item or use the inspector to save a file, invite someone as viewer/editor, or create, copy, or revoke a public viewer link.

Select an image or video and click **Load thumbnail** for a still preview. To avoid unexpected bandwidth use, the app only downloads previews on request and requires a known file size of **32 MiB or less**. Preview support depends on FFmpeg's decoders. Previews are cached by remote revision. Online-only Proton Docs and Sheets are not downloadable through this integration.

## Two-way sync

1. Open the remote folder you want to pair. Start with a disposable test folder.
2. Choose **Set up sync…** and select an existing local folder.
3. Keep the app open. It attempts a sync shortly after launch and every 60 seconds after the previous pass. **Sync now** runs a pass on demand; **Pause** stops future passes.

This version supports **one folder pair**, including subfolders. It copies new files in both directions and transfers edits when the other side is unchanged. It creates remote revisions for local edits instead of trashing the original.

File moves and renames within the pair are reflected on the other side without transferring the file again when the app can match the file's identity. Removing a previously synced local file from the pair sends the unchanged cloud copy to Proton Drive Trash. A deletion in the Proton web app sends the unchanged local copy to desktop Trash when the CLI confirms the cloud file is in Trash. If the cloud file is already permanently deleted and the CLI can no longer confirm what happened, the local copy stays in place. The app does not permanently delete files or propagate moves and deletions of entire directories.

Conflicting changes, occupied destinations, and deletions the app cannot confirm are reported rather than silently overwritten. If the local filesystem does not support desktop Trash, the local file is left intact. If both sides initially contain the same name, the app only treats them as synchronized when it can match content via the remote digest. Symlinks and special files are not synced. Click the sync result in the status bar to review counts and issues.

Sync operates by polling, not by subscribing to cloud events. It checks file state before transferring, moving, or trashing, but these checks and actions are not one transaction: a concurrent change can still land between them. Keep backups and use a test folder before relying on sync for important files. Closing the app stops sync; there is no background daemon.

To try path changes safely, sync a disposable file first. Rename it locally, then move it between subfolders in the Proton web UI, running **Sync now** after each change. Finally, delete it in the web UI and verify the local file is in your desktop Trash rather than permanently removed.

## Local data and performance

- Folder listings and thumbnails are cached by account in `$XDG_CACHE_HOME/fuzzy-proton-drive` (default `~/.cache/fuzzy-proton-drive`). Both can contain private data in plaintext. Signing out clears the GUI's in-memory state, not the on-disk cache. The app only exposes a cache after verifying the account. It does not support fully offline startup.
- Preferences and the sync journal are in `$XDG_STATE_HOME/fuzzy-proton-drive` (default `~/.local/state/fuzzy-proton-drive`). The journal stores file paths, fingerprints and revisions, not file contents. Do not remove it while relying on existing sync baselines.
- The folder tree and file table render visible rows only. CLI calls, disk I/O, hashing, and thumbnail generation are off the UI thread. Visited folders open from memory while refreshing. CLI jobs are serialized so they don't compete over session and cache state; an active sync pass or transfer can delay a fresh fetch.

## Checks

```bash
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
desktop-file-validate packaging/io.github.fuzzy_proton_drive.desktop
```

The sync integration test uses a mock CLI and a temporary directory; it does not authenticate or contact Proton.

The launcher icon is drawn in SVG. To regenerate the PNG embedded in the app after editing the SVG:

```bash
rsvg-convert -o packaging/io.github.fuzzy_proton_drive.png packaging/io.github.fuzzy_proton_drive.svg
```

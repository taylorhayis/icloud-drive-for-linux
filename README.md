# iCloud Drive for Linux

A Linux client for iCloud Drive. The whole drive shows up as a folder in your file manager. Files that still live in the cloud keep their real names and sizes. Opening one downloads it. Files you choose to keep downloaded stay on disk and keep syncing.

Sign-in happens in a small GTK window. A background service owns the mount, the filename index, and the transfers. The window and a panel icon show what that service is doing.

This uses the same web API as iCloud.com. Apple does not publish that API, and they can change it.

## What you can do

- Browse the whole drive from Files, Dolphin, Nautilus, or any other program that reads a normal folder.
- Leave most files in the cloud. The local index is enough to search them by name.
- Pin a file or folder with **Keep downloaded** so it stays local and is uploaded again after you edit it.
- Sign in, enter a verification code, accept updated terms, or approve Advanced Data Protection from the window.
- Watch a sync or a single-file transfer on a progress bar, on the panel icon, and in one notification that updates in place.
- Resolve a conflict, retry a transfer that stopped, or dismiss a file Apple no longer has. Those actions wait for you.

The folder is `~/iCloud Drive` unless you set another path.

## How a file gets here

The service lists every folder and stores the names. That first walk of a large drive takes a while. Later checks are local until something actually has to move.

A cloud file is a placeholder with the size Apple reported. The bytes are downloaded when a program opens the file. The download runs where a stall can be cancelled, so a stuck transfer does not freeze the file manager. Zero-byte files are recorded as complete without a download, because iCloud rejects that request.

Transfers run one at a time. Overlapping them makes the web API slower, not faster. The service asks Apple again about every five minutes. iCloud does not push changes to this client, so an edit from a phone shows up on the next check, or when you run `icloud-drive sync`.

Pinned files are fetched in the background after sign-in and kept up to date. Everything else stays a name until you open it.

## Sign-in

The password is used to complete sign-in and then cleared. It is not written to the config file or the session file. The session is stored at `~/.local/state/icloud-drive/session.json` with mode `0600`.

A normal trusted session lasts on the order of two months. With Advanced Data Protection on, Apple can ask a trusted device to approve web access much more often, sometimes about every eight hours. The window has a step for that approval, for an SMS code, and for a code shown on a trusted device.

The command line never asks for a password. `status`, `search`, `pin`, `unpin`, and `sync` talk to the service that is already running. If the session has expired, the service stays up and the window says sign-in is needed.

Accounts on iCloud in mainland China can set `china = true` in the config so sign-in uses Apple's China hosts.

## Saving and deleting

Deleting from the file manager is refused until you turn on **Allow deletion**. The switch is off by default, and the choice is saved. With it on, a delete is sent to the iCloud trash. If Apple rejects it, the item stays queued and the window says the delete failed.

Saving from an editor still works with deletion off. LibreOffice and many other programs write a temporary file and rename it over the document you had open. That rename replaces the existing file only after the new copy is complete and flushed. An unfinished temp file is refused, and the file you already had is left as it was.

`chmod` and `chown` succeed for a file that exists. Dolphin and other file managers set those while copying. The service accepts them and does not treat that as an edit to upload.

## When the two sides disagree

A finished folder listing is the only thing that may drop a name from the index. The name is removed when Apple no longer lists it, the listing completed, and the local file has no unsaved edits. A listing that times out or fails is reported in the window and does not empty the folder.

If you edited a file that Apple no longer has, it becomes a conflict. The window can keep your copy, keep theirs, or keep both. Keep both writes a separate local file, so the two copies do not share one blob.

A download or upload that stops short of the expected size stays in the same list, with retry and discard. A document id Apple says is gone is remembered, so opening it again does not retry forever. Refresh in the window clears that and asks the service to look again.

Metadata that has not changed is left alone. A large drive is not rewritten in the database on every poll.

## The panel

The icon is a StatusNotifierItem. COSMIC and KDE show that in the status area. On GNOME it appears where AppIndicator is installed. The icon stays visible while the service is running. It draws a progress ring during a download or upload, and the notification carries the same percent. Clicking the icon opens the window.

## Install

You need Rust (stable) and the GTK 4, libadwaita, and FUSE 3 development packages.

```bash
sudo apt install libgtk-4-dev libadwaita-1-dev libfuse3-dev pkg-config build-essential
cargo build --release
```

Run it by hand:

```bash
./target/release/icloud-drive daemon
./target/release/icloud-drive-gui
```

Or install the user service:

```bash
sudo install -m 0755 target/release/icloud-drive target/release/icloud-drive-gui /usr/bin/
install -D -m 0644 packaging/icloud-drive.service ~/.config/systemd/user/icloud-drive.service
install -D -m 0644 packaging/icloud-drive.desktop ~/.local/share/applications/icloud-drive.desktop
systemctl --user daemon-reload
systemctl --user enable --now icloud-drive.service
```

Logs:

```bash
journalctl --user -u icloud-drive.service -f
```

`icloud-drive logs` prints the same journal.

## Commands

| Command | What it does |
| --- | --- |
| `icloud-drive daemon` | Mount the drive and sync |
| `icloud-drive status` | Show the current phase and anything that needs you |
| `icloud-drive search notes` | Search the local filename index |
| `icloud-drive pin PATH` | Keep a file or folder downloaded |
| `icloud-drive unpin PATH` | Stop keeping it downloaded |
| `icloud-drive sync` | Ask the service to check iCloud now |

## Config and data

Optional file: `~/.config/icloud-drive/config.toml`

```toml
mount_point = "/home/you/iCloud Drive"
allow_deletion = false
poll_seconds = 300
china = false
```

| Path | What it holds |
| --- | --- |
| `~/.local/share/icloud-drive/index.sqlite3` | Names, pins, conflicts, and the trash queue |
| `~/.cache/icloud-drive/blobs` | Downloaded file contents |
| `~/.local/state/icloud-drive/session.json` | The signed-in session, mode `0600` |

## Layout

The workspace is split so the Apple protocol, the sync rules, the mount, and the window can change separately.

| Crate | Role |
| --- | --- |
| `icloud-proto` | Sign-in and the Drive web calls |
| `icloud-engine` | The index, pins, uploads, and the rules that avoid losing a file |
| `icloud-mount` | The FUSE filesystem |
| `icloud-ipc` | The D-Bus API the window and the command line share |
| `icloud-cli` | `icloud-drive`, the service, and the panel icon |
| `icloud-gui` | The GTK 4 / libadwaita window |

## License

MIT. See [LICENSE](LICENSE).

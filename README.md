# iCloud Drive for Linux

A Linux client for iCloud Drive. The whole drive shows up as a folder in your file manager. A library of any size stays a list of names until you open a file or mark it to keep offline. Only those files occupy disk space.

Sign-in happens in a small GTK window. A background service owns the mount, the filename index, and the transfers. The window and a panel icon show what that service is doing.

This uses the same web API as iCloud.com. Apple does not publish that API, and they can change it.

## What you can do

- Browse the whole drive from Files, Dolphin, Nautilus, or any other program that reads a normal folder. The names are local. The bytes stay in iCloud.
- Search by filename without downloading the files.
- Pin a file or folder so that one item stays on disk and is uploaded again after you edit it. In COSMIC Files and GNOME Files that action is **iC - Pin Download**. In Dolphin it is **Keep downloaded**, inside an **iCloud Drive** menu.
- Sign in, enter a verification code, accept updated terms, or approve Advanced Data Protection from the window.
- Watch a sync or a single-file transfer on a progress bar, on the panel icon, and in one notification that updates in place.
- Resolve a conflict, retry a transfer that stopped, or dismiss a file Apple no longer has. Those actions wait for you.

The folder is `~/iCloud Drive` unless you set another path.

## How a file gets here

The service lists names one folder at a time. The root is first, so those files and folder names show up as soon as that listing returns. It then walks the other folders breadth-first: every folder you can already see is filled in before the sync descends further. Each finished folder is written immediately. The window's progress bar is files already indexed out of the file count Apple reports for the whole drive. The line under it shows the folder being listed. A large drive still takes a long time to finish, and only the listing is downloaded. Later checks compare those names. They do not fetch file contents.

Each cloud file is a placeholder. `ls` shows the size stored in iCloud, so you can see what opening it will download. The file allocates no local blocks until the bytes are here, so `du` and other disk-usage tools count it as empty. The bytes are written under `~/.cache/icloud-drive/blobs` in three cases: a program opens the file, you pin it, or you save a new file of your own. Unpinning a file that you have not edited removes that cache and leaves the name in place.

The download runs where a stall can be cancelled, so a stuck transfer does not freeze the file manager. Zero-byte files are recorded as complete without a download, because iCloud rejects that request.

Transfers run one at a time. Overlapping them makes the web API slower, not faster. The service asks Apple again about every five minutes. iCloud does not push changes to this client, so an edit from a phone shows up on the next check, or when you run `icloud-drive sync`. That refresh updates names. It does not pull the library down.

Pinned files are fetched in the background after sign-in and kept up to date.

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
icloud-drive install-menus
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
| `icloud-drive unpin PATH` | Stop keeping it downloaded and drop the cached copy |
| `icloud-drive sync` | Ask the service to check iCloud now |
| `icloud-drive install-menus` | Add the right-click actions below |

## Right-click

The window's **File manager menu** switch adds or removes the items. It is on by default. `install-menus` turns it on without opening the window. Reopen the file manager after changing it.

| File manager | Where it appears |
| --- | --- |
| COSMIC Files | **iC - Pin Download** and **iC - Unpin Download** on the menu. COSMIC shows custom actions for every file; choosing one outside this drive reports that and changes nothing. |
| Dolphin | An **iCloud Drive** submenu with **Keep downloaded** and **Unpin** |
| GNOME Files | **iC - Pin Download** and **iC - Unpin Download**, only for items inside the drive |

GNOME Files needs the `nautilus-python` package. Restart the file manager after the items are installed.

## Config and data

Optional file: `~/.config/icloud-drive/config.toml`

```toml
mount_point = "/home/you/iCloud Drive"
allow_deletion = false
file_browser = true
poll_seconds = 300
china = false
```

| Path | What it holds |
| --- | --- |
| `~/.local/share/icloud-drive/index.sqlite3` | Names, pins, conflicts, and the trash queue |
| `~/.cache/icloud-drive/blobs` | Contents of files you have opened or pinned |
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

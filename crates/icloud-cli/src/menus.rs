//! Right-click actions for COSMIC Files, Dolphin, and GNOME Files.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const MARK_BEGIN: &str = "// icloud-drive:begin";
const MARK_END: &str = "// icloud-drive:end";

pub fn uninstall() -> Result<(), String> {
    let home = dirs::home_dir().ok_or("no home directory")?;
    let cosmic = home.join(".config/cosmic/com.system76.CosmicFiles/v1/context_actions");
    if let Ok(existing) = fs::read_to_string(&cosmic) {
        fs::write(&cosmic, strip_cosmic(&existing)).map_err(|err| err.to_string())?;
    }
    let _ = fs::remove_file(home.join(".local/share/kio/servicemenus/icloud-drive.desktop"));
    let _ = fs::remove_file(home.join(".local/share/nautilus-python/extensions/icloud_drive.py"));
    Ok(())
}

pub fn install(exe: &Path) -> Result<(), String> {
    let home = dirs::home_dir().ok_or("no home directory")?;
    let mount = crate::configured_mount(&home);
    install_cosmic(&home, exe)?;
    install_dolphin(&home, exe)?;
    install_nautilus(&home, exe, &mount)?;
    Ok(())
}

/// Turn a file-manager path into the path the index uses.
///
/// `/home/you/iCloud Drive/Photos/a.jpg` becomes `/Photos/a.jpg`.
/// A path that is already inside the index, such as `/Photos/a.jpg`, is kept
/// when it is not a real file outside the mount.
pub fn drive_path(mount: &Path, input: &str) -> Result<String, String> {
    let input = input.trim().trim_end_matches('/');
    if input.is_empty() {
        return Ok("/".into());
    }
    let path = Path::new(input);
    if let Ok(rest) = path.strip_prefix(mount) {
        let rel = rest.to_string_lossy();
        if rel.is_empty() {
            return Ok("/".into());
        }
        return Ok(format!("/{rel}"));
    }
    if path.is_absolute() && path.exists() {
        return Err("That file is not in iCloud Drive".into());
    }
    if input.starts_with('/') {
        Ok(input.to_string())
    } else {
        Ok(format!("/{input}"))
    }
}

fn install_cosmic(home: &Path, exe: &Path) -> Result<(), String> {
    let path = home.join(".config/cosmic/com.system76.CosmicFiles/v1/context_actions");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let next = merge_cosmic(&existing, &cosmic_block(exe));
    fs::write(&path, next).map_err(|err| err.to_string())
}

fn install_dolphin(home: &Path, exe: &Path) -> Result<(), String> {
    let path = home.join(".local/share/kio/servicemenus/icloud-drive.desktop");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let exe = exe.display().to_string().replace('\'', "'\\''");
    let body = format!(
        "[Desktop Entry]\n\
         Type=Service\n\
         MimeType=all/allfiles;inode/directory;\n\
         X-KDE-Submenu=iCloud Drive\n\
         Actions=Keep;Unpin;\n\
         \n\
         [Desktop Action Keep]\n\
         Name=Keep downloaded\n\
         Exec='{exe}' --notify pin %F\n\
         \n\
         [Desktop Action Unpin]\n\
         Name=Unpin\n\
         Exec='{exe}' --notify unpin %F\n"
    );
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o755)
        .open(&path)
        .map_err(|err| err.to_string())?;
    file.write_all(body.as_bytes()).map_err(|err| err.to_string())?;
    Ok(())
}

fn install_nautilus(home: &Path, exe: &Path, mount: &Path) -> Result<(), String> {
    let path = home.join(".local/share/nautilus-python/extensions/icloud_drive.py");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let body = nautilus_script(&exe.display().to_string(), &mount.display().to_string());
    fs::write(&path, body).map_err(|err| err.to_string())
}

fn cosmic_block(exe: &Path) -> String {
    let exe = exe.display().to_string().replace('\'', "'\\''");
    format!(
        "{MARK_BEGIN}\n    (\n        name: \"iC - Pin Download\",\n        confirm: false,\n        selection: Any,\n        steps: [\n            \"'{exe}' --notify pin %F\",\n        ],\n    ),\n    (\n        name: \"iC - Unpin Download\",\n        confirm: false,\n        selection: Any,\n        steps: [\n            \"'{exe}' --notify unpin %F\",\n        ],\n    ),\n{MARK_END}"
    )
}

fn merge_cosmic(existing: &str, block: &str) -> String {
    if let Some(start) = existing.find(MARK_BEGIN) {
        if let Some(end_rel) = existing[start..].find(MARK_END) {
            let end = start + end_rel + MARK_END.len();
            let mut out = String::new();
            out.push_str(&existing[..start]);
            out.push_str(block);
            out.push_str(&existing[end..]);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            return out;
        }
    }
    let trimmed = existing.trim();
    if trimmed.is_empty() {
        return format!("[\n{block}\n]\n");
    }
    if let Some(idx) = trimmed.rfind(']') {
        let head = trimmed[..idx].trim_end();
        let mut out = head.to_string();
        if !head.ends_with('[') && !head.ends_with(',') {
            out.push(',');
        }
        out.push('\n');
        out.push_str(block);
        out.push_str(",\n]\n");
        return out;
    }
    format!("[\n{block}\n]\n")
}

fn strip_cosmic(existing: &str) -> String {
    let Some(start) = existing.find(MARK_BEGIN) else {
        return existing.to_string();
    };
    let Some(end_rel) = existing[start..].find(MARK_END) else {
        return existing.to_string();
    };
    let end = start + end_rel + MARK_END.len();
    let mut out = existing[..start].trim_end().to_string();
    out.push_str(existing[end..].trim_start_matches('\n'));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn nautilus_script(exe: &str, mount: &str) -> String {
    format!(
        "import os\n\
         import subprocess\n\
         \n\
         EXE = {exe}\n\
         MOUNT = {mount}\n\
         \n\
         try:\n\
         \x20   from gi.repository import GObject, Nautilus\n\
         except ImportError:\n\
         \x20   Nautilus = None\n\
         \n\
         def _inside(path):\n\
         \x20   path = os.path.abspath(path)\n\
         \x20   mount = os.path.abspath(MOUNT)\n\
         \x20   return path == mount or path.startswith(mount + os.sep)\n\
         \n\
         if Nautilus is not None:\n\
         \x20   class IcloudDriveMenu(GObject.GObject, Nautilus.MenuProvider):\n\
         \x20       def get_file_items(self, *args):\n\
         \x20           files = args[-1]\n\
         \x20           paths = []\n\
         \x20           mount = os.path.abspath(MOUNT)\n\
         \x20           for item in files:\n\
         \x20               location = item.get_location()\n\
         \x20               path = None if location is None else location.get_path()\n\
         \x20               if not path or not _inside(path) or os.path.abspath(path) == mount:\n\
         \x20                   return []\n\
         \x20               paths.append(path)\n\
         \x20           if not paths:\n\
         \x20               return []\n\
         \x20           keep = Nautilus.MenuItem(name=\"IcloudDrive::Keep\", label=\"iC - Pin Download\")\n\
         \x20           keep.connect(\"activate\", self._run, \"pin\", paths)\n\
         \x20           unpin = Nautilus.MenuItem(name=\"IcloudDrive::Unpin\", label=\"iC - Unpin Download\")\n\
         \x20           unpin.connect(\"activate\", self._run, \"unpin\", paths)\n\
         \x20           return [keep, unpin]\n\
         \n\
         \x20       def get_background_items(self, *args):\n\
         \x20           return []\n\
         \n\
         \x20       def _run(self, _item, action, paths):\n\
         \x20           subprocess.Popen([EXE, \"--notify\", action, *paths])\n",
        exe = py_literal(exe),
        mount = py_literal(mount),
    )
}

fn py_literal(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

pub fn binary_path() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_prefix_becomes_a_drive_path() {
        let mount = Path::new("/home/pika/iCloud Drive");
        let path = drive_path(mount, "/home/pika/iCloud Drive/Photos/a.jpg").unwrap();
        assert_eq!(path, "/Photos/a.jpg");
        assert_eq!(drive_path(mount, "/home/pika/iCloud Drive").unwrap(), "/");
        assert_eq!(drive_path(mount, "/Photos/a.jpg").unwrap(), "/Photos/a.jpg");
    }

    #[test]
    fn cosmic_actions_are_replaced_in_place() {
        let first = merge_cosmic("", &cosmic_block(Path::new("/usr/bin/icloud-drive")));
        assert!(first.contains("iC - Pin Download"));
        assert!(first.contains("iC - Unpin Download"));
        let second = merge_cosmic(&first, &cosmic_block(Path::new("/tmp/icloud-drive")));
        assert!(second.contains("/tmp/icloud-drive"));
        assert_eq!(second.matches("iC - Pin Download").count(), 1);
        let with_user = merge_cosmic(
            "[\n    (\n        name: \"Open with Code\",\n        confirm: false,\n        selection: Any,\n        steps: [\"code %F\"],\n    ),\n]\n",
            &cosmic_block(Path::new("/usr/bin/icloud-drive")),
        );
        assert!(with_user.contains("Open with Code"));
        assert!(with_user.contains("iC - Pin Download"));
        let stripped = strip_cosmic(&with_user);
        assert!(stripped.contains("Open with Code"));
        assert!(!stripped.contains("iC - Pin Download"));
        assert!(!stripped.contains(MARK_BEGIN));
    }
}

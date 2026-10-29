//! Path helpers used when a command is given a file from the file manager.

use std::fs;
use std::path::Path;

const MARK_BEGIN: &str = "// icloud-drive:begin";
const MARK_END: &str = "// icloud-drive:end";

/// Remove the pin and unpin actions from COSMIC Files, Dolphin, and GNOME Files.
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

/// Turn a file-manager path into the path the index uses.
///
/// `/home/you/iCloud Drive/Photos/a.jpg` becomes `/Photos/a.jpg`.
/// A path that is already inside the index, such as `/Photos/a.jpg`, is kept
/// when it is not a real file outside the folder.
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
    fn cosmic_actions_are_removed_without_touching_other_items() {
        let existing = "[\n    (\n        name: \"Open with Code\",\n        confirm: false,\n        selection: Any,\n        steps: [\"code %F\"],\n    ),\n// icloud-drive:begin\n    (\n        name: \"iC - Pin Download\",\n        confirm: false,\n        selection: Any,\n        steps: [\"icloud-drive pin %F\"],\n    ),\n// icloud-drive:end\n]\n";
        let stripped = strip_cosmic(existing);
        assert!(stripped.contains("Open with Code"));
        assert!(!stripped.contains("iC - Pin Download"));
        assert!(!stripped.contains(MARK_BEGIN));
    }
}

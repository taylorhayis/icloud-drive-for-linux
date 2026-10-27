use unicode_normalization::UnicodeNormalization;

/// Linux file name for an Apple Drive item. Apple stores NFD; the mount shows NFC.
pub fn local_name(apple_name: &str, extension: Option<&str>) -> String {
    let mut raw = apple_name.to_string();
    if let Some(ext) = extension.filter(|ext| !ext.is_empty()) {
        let suffix = format!(".{ext}");
        if !raw.ends_with(&suffix) {
            raw.push('.');
            raw.push_str(ext);
        }
    }
    let nfc: String = raw.nfc().collect();
    nfc.replace('/', "∕")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfd_name_becomes_nfc() {
        let nfd = "Cafe\u{0301}";
        assert_eq!(local_name(nfd, None), "Café");
    }

    #[test]
    fn extension_is_appended_once() {
        assert_eq!(local_name("Photo", Some("jpg")), "Photo.jpg");
        assert_eq!(local_name("Photo.jpg", Some("jpg")), "Photo.jpg");
    }

    #[test]
    fn slash_cannot_escape_a_directory() {
        assert_eq!(local_name("a/b", None), "a∕b");
    }
}

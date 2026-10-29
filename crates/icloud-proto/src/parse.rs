use chrono::DateTime;
use serde_json::Value;

use crate::names::local_name;

pub const ROOT_DRIVEWSID: &str = "FOLDER::com.apple.CloudDocs::root";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKind {
    File,
    Folder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteNode {
    pub drivewsid: String,
    pub docwsid: String,
    pub name_apple: String,
    pub name_local: String,
    pub extension: Option<String>,
    pub kind: RemoteKind,
    pub size: u64,
    pub etag: String,
    pub mtime_ms: i64,
}

pub fn parse_container(body: &Value) -> Option<(String, String)> {
    let node = body.as_array().and_then(|items| items.first()).unwrap_or(body);
    let drivewsid = node.get("drivewsid").and_then(|v| v.as_str())?;
    let docwsid = node.get("docwsid").and_then(|v| v.as_str())?;
    if drivewsid.is_empty() || docwsid.is_empty() {
        return None;
    }
    Some((drivewsid.to_string(), docwsid.to_string()))
}

pub fn parse_created(body: &[u8]) -> (Option<String>, Option<String>, String) {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let node = value
        .get("folders")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .or_else(|| value.as_array().and_then(|items| items.first()))
        .unwrap_or(&value);
    (
        node.get("drivewsid").and_then(|v| v.as_str()).map(str::to_string),
        node.get("docwsid").and_then(|v| v.as_str()).map(str::to_string),
        node.get("etag").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    )
}

pub fn parse_file_count(body: &Value) -> Option<u64> {
    let node = body.as_array().and_then(|items| items.first()).unwrap_or(body);
    node.get("fileCount").and_then(|v| v.as_u64())
}

pub fn parse_folder(body: &Value) -> Result<Vec<RemoteNode>, String> {
    let node = body
        .as_array()
        .and_then(|items| items.first())
        .or(Some(body))
        .ok_or_else(|| "empty Drive listing".to_string())?;
    let items = node
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            let status = node.get("status").and_then(|v| v.as_str()).unwrap_or("no items");
            format!("folder listing was not usable ({status})")
        })?;
    Ok(items.iter().filter_map(parse_node).collect())
}

pub fn parse_node(value: &Value) -> Option<RemoteNode> {
    let drivewsid = value.get("drivewsid")?.as_str()?.to_string();
    let docwsid = value
        .get("docwsid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name_apple = value.get("name")?.as_str()?.to_string();
    if name_apple.is_empty() || name_apple == ".localized" {
        return None;
    }
    let extension = value
        .get("extension")
        .and_then(|v| v.as_str())
        .filter(|ext| !ext.is_empty())
        .map(str::to_string);
    let kind = match value.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "FILE" => RemoteKind::File,
        _ => RemoteKind::Folder,
    };
    let size = value.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
    let etag = value
        .get("etag")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mtime_ms = value
        .get("dateModified")
        .or_else(|| value.get("dateChanged"))
        .and_then(|v| v.as_str())
        .and_then(parse_time)
        .unwrap_or(0);
    Some(RemoteNode {
        name_local: local_name(&name_apple, extension.as_deref()),
        drivewsid,
        docwsid,
        name_apple,
        extension,
        kind,
        size,
        etag,
        mtime_ms,
    })
}

fn parse_time(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.timestamp_millis())
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%SZ")
                .ok()
                .map(|time| time.and_utc().timestamp_millis())
        })
}

pub fn parse_download_url(body: &Value) -> Result<String, String> {
    for key in ["data_token", "package_token"] {
        if let Some(url) = body
            .get(key)
            .and_then(|token| token.get("url"))
            .and_then(|url| url.as_str())
        {
            return Ok(url.to_string());
        }
    }
    Err("iCloud did not return a download URL".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn app_library_is_a_folder() {
        let body = json!([{
            "items": [
                {
                    "drivewsid": "FOLDER::com.apple.CloudDocs::obs",
                    "docwsid": "obs",
                    "name": "Obsidian",
                    "type": "APP_LIBRARY",
                    "etag": "1"
                },
                {
                    "drivewsid": "FILE::com.apple.CloudDocs::pic",
                    "docwsid": "pic",
                    "name": "Cafe\u{0301}",
                    "extension": "jpg",
                    "type": "FILE",
                    "size": 12,
                    "etag": "2",
                    "dateModified": "2024-01-02T03:04:05Z"
                }
            ]
        }]);
        let nodes = parse_folder(&body).unwrap();
        assert_eq!(nodes[0].kind, RemoteKind::Folder);
        assert_eq!(nodes[1].name_local, "Café.jpg");
        assert_eq!(nodes[1].size, 12);
        assert!(nodes[1].mtime_ms > 0);
    }

    #[test]
    fn missing_items_is_an_error() {
        let err = parse_folder(&json![{"status": "NOT_FOUND"}]).unwrap_err();
        assert!(err.contains("NOT_FOUND"));
    }

    #[test]
    fn root_file_count_is_the_drive_total() {
        let body = json!([{
            "drivewsid": "FOLDER::com.apple.CloudDocs::root",
            "fileCount": 7,
            "directChildrenCount": 1,
            "items": [{
                "drivewsid": "FILE::com.apple.CloudDocs::a",
                "docwsid": "a",
                "name": "Notes",
                "extension": "txt",
                "type": "FILE",
                "size": 4,
                "etag": "e"
            }]
        }]);
        assert_eq!(parse_file_count(&body), Some(7));
    }
}

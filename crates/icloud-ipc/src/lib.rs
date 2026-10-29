use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

pub const BUS_NAME: &str = "dev.icloud_drive.Service";
pub const BUS_PATH: &str = "/dev/icloud_drive/Service";

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct StatusSnapshot {
    pub phase: String,
    pub label: String,
    pub current_path: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub bytes_known: bool,
    pub items_done: u64,
    pub items_total: u64,
    pub items_known: bool,
    pub attention_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct AttentionSnapshot {
    pub id: i64,
    pub kind: String,
    pub path: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct SearchHit {
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ActivityEvent {
    pub at_ms: i64,
    pub kind: String,
    pub path: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct FolderChoice {
    pub name: String,
    pub path: String,
    /// This folder itself was chosen.
    pub synced: bool,
    /// This folder is inside a chosen folder, or is one.
    pub included: bool,
}

#[zbus::proxy(
    interface = "dev.icloud_drive.Service",
    default_service = "dev.icloud_drive.Service",
    default_path = "/dev/icloud_drive/Service"
)]
pub trait Service {
    fn status(&self) -> zbus::Result<StatusSnapshot>;
    fn attention(&self) -> zbus::Result<Vec<AttentionSnapshot>>;
    fn activity(&self) -> zbus::Result<Vec<ActivityEvent>>;
    fn search(&self, query: &str) -> zbus::Result<Vec<SearchHit>>;
    fn child_folders(&self, path: &str) -> zbus::Result<Vec<FolderChoice>>;
    fn sync_folders(&self) -> zbus::Result<Vec<String>>;
    fn set_sync_folder(&self, path: &str, enabled: bool) -> zbus::Result<()>;
    fn begin_sign_in(&self, apple_id: &str, password: &str) -> zbus::Result<String>;
    fn submit_code(&self, code: &str) -> zbus::Result<String>;
    fn request_sms(&self) -> zbus::Result<String>;
    fn accept_terms(&self) -> zbus::Result<String>;
    fn request_device_approval(&self) -> zbus::Result<String>;
    fn set_pinned(&self, path: &str, pinned: bool) -> zbus::Result<()>;
    fn set_allow_deletion(&self, enabled: bool) -> zbus::Result<()>;
    fn allow_deletion(&self) -> zbus::Result<bool>;
    fn resolve_attention(&self, id: i64, action: &str) -> zbus::Result<()>;
    fn sync_now(&self) -> zbus::Result<()>;
    fn mount_point(&self) -> zbus::Result<String>;
    fn quit(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn closing(&self) -> zbus::Result<()>;
}

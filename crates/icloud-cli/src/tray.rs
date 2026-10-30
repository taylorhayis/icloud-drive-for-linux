use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use icloud_engine::{Engine, Phase};
use zbus::zvariant::{OwnedValue, Value};
use zbus::{interface, Connection};

pub async fn run(
    connection: Connection,
    engine: Arc<Engine>,
    stop: tokio::sync::watch::Sender<bool>,
) -> zbus::Result<()> {
    let item = TrayItem {
        engine,
        pixmap: std::sync::Mutex::new(Vec::new()),
        tooltip: std::sync::Mutex::new("iCloud Drive".to_string()),
        status: std::sync::Mutex::new("Active".to_string()),
    };
    connection
        .object_server()
        .at("/StatusNotifierItem/Menu", TrayMenu { stop })
        .await?;
    connection.object_server().at("/StatusNotifierItem", item).await?;
    // One registration per process. Registering both the bus name and the
    // object path leaves a ghost icon: COSMIC drops only the first item when
    // the process exits.
    let name = format!("org.kde.StatusNotifierItem-icloud-{}", std::process::id());
    let owned_name = match connection.request_name(name.as_str()).await {
        Ok(()) => Some(name.as_str()),
        Err(err) => {
            tracing::warn!("panel icon bus name was not acquired: {err}");
            None
        }
    };
    let watcher = WatcherProxy::new(&connection).await?;
    register_once(&watcher, owned_name).await?;

    let notifications = NotificationsProxy::new(&connection).await.ok();
    let mut replaces_id = 0u32;
    let mut last_note = String::new();
    let mut shown: Option<(String, Vec<(i32, i32, Vec<u8>)>, String)> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let iface = connection.object_server().interface::<_, TrayItem>("/StatusNotifierItem").await?;
        let status = iface.get().await.engine.status();
        let next = presentation(&status);
        if shown.as_ref() != Some(&next) {
            let icon_changed = shown.as_ref().map(|(_, pixmap, _)| pixmap) != Some(&next.1);
            let status_changed = shown.as_ref().map(|(_, _, sni_status)| sni_status) != Some(&next.2);
            let tip_changed = shown.as_ref().map(|(label, _, _)| label) != Some(&next.0);
            {
                let item = iface.get_mut().await;
                *item.tooltip.lock().expect("tooltip") = next.0.clone();
                *item.pixmap.lock().expect("pixmap") = next.1.clone();
                *item.status.lock().expect("status") = next.2.clone();
            }
            let context = iface.signal_context();
            if icon_changed {
                let _ = TrayItem::new_icon(context).await;
            }
            if status_changed {
                let _ = TrayItem::new_status(context).await;
            }
            if tip_changed {
                let _ = TrayItem::new_tool_tip(context).await;
            }
            shown = Some(next);
        }
        let label = shown.as_ref().map(|(label, _, _)| label.clone()).unwrap_or_default();
        let transferring = status.phase == Phase::Downloading || status.phase == Phase::Uploading;
        if transferring && label != last_note {
            if let Some(notifications) = &notifications {
                let percent = match status.bytes_total {
                    Some(total) if total > 0 => {
                        ((status.bytes_done as f64 / total as f64) * 100.0).round() as i32
                    }
                    _ => -1,
                };
                let mut hints = std::collections::HashMap::new();
                if percent >= 0 {
                    hints.insert("value", Value::I32(percent));
                }
                if let Ok(id) = notifications
                    .notify(
                        "iCloud Drive",
                        replaces_id,
                        "folder-remote-symbolic",
                        "iCloud Drive",
                        &label,
                        &[],
                        hints,
                        4000,
                    )
                    .await
                {
                    replaces_id = id;
                    last_note = label;
                }
            }
        } else if !transferring {
            last_note.clear();
        }
    }
}

fn open_window() {
    let binary = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join("icloud-drive-gui")))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| std::path::PathBuf::from("icloud-drive-gui"));
    let _ = std::process::Command::new(binary).spawn();
}

struct TrayItem {
    engine: Arc<Engine>,
    pixmap: std::sync::Mutex<Vec<(i32, i32, Vec<u8>)>>,
    tooltip: std::sync::Mutex<String>,
    status: std::sync::Mutex<String>,
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl TrayItem {
    fn activate(&self, _x: i32, _y: i32) {
        open_window();
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {}

    fn scroll(&self, _delta: i32, _orientation: &str) {}

    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "icloud-drive"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "iCloud Drive"
    }

    #[zbus(property)]
    fn status(&self) -> String {
        self.status.lock().expect("status").clone()
    }

    #[zbus(property)]
    fn icon_name(&self) -> &str {
        "folder-remote-symbolic"
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        self.pixmap.lock().expect("pixmap").clone()
    }

    #[zbus(property)]
    fn tool_tip(&self) -> (String, Vec<(i32, i32, Vec<u8>)>, String, String) {
        (
            "folder-remote-symbolic".into(),
            Vec::new(),
            "iCloud Drive".into(),
            self.tooltip.lock().expect("tooltip").clone(),
        )
    }

    #[zbus(property)]
    fn menu(&self) -> zbus::zvariant::OwnedObjectPath {
        "/StatusNotifierItem/Menu".try_into().expect("menu path")
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    /// The icon name or pixmap changed. Hosts read the properties after this.
    #[zbus(signal)]
    async fn new_icon(ctxt: &zbus::object_server::SignalContext<'_>) -> zbus::Result<()>;

    /// `status` changed.
    #[zbus(signal)]
    async fn new_status(ctxt: &zbus::object_server::SignalContext<'_>) -> zbus::Result<()>;

    /// The tooltip changed.
    #[zbus(signal)]
    async fn new_tool_tip(ctxt: &zbus::object_server::SignalContext<'_>) -> zbus::Result<()>;
}

fn notifier_service(owned_name: Option<&str>) -> &str {
    owned_name.unwrap_or("/StatusNotifierItem")
}

async fn register_once(watcher: &WatcherProxy<'_>, owned_name: Option<&str>) -> zbus::Result<()> {
    let service = notifier_service(owned_name);
    if owned_name.is_some() && watcher.register_status_notifier_item(service).await.is_ok() {
        return Ok(());
    }
    if owned_name.is_some() {
        tracing::warn!("panel icon registration by name failed; using the object path");
    }
    watcher
        .register_status_notifier_item("/StatusNotifierItem")
        .await
}

const OPEN_ITEM: i32 = 1;
const QUIT_ITEM: i32 = 2;

struct TrayMenu {
    stop: tokio::sync::watch::Sender<bool>,
}

#[interface(name = "com.canonical.dbusmenu")]
impl TrayMenu {
    fn get_layout(
        &self,
        parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, MenuNode)> {
        let node = match parent_id {
            0 => root_menu(),
            OPEN_ITEM => action(OPEN_ITEM, "Open"),
            QUIT_ITEM => action(QUIT_ITEM, "Quit"),
            _ => return Err(zbus::fdo::Error::InvalidArgs("unknown menu id".into())),
        };
        Ok((1, node))
    }

    fn event(&self, id: i32, event_id: &str, _data: zbus::zvariant::OwnedValue, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        if id == OPEN_ITEM {
            open_window();
        } else if id == QUIT_ITEM {
            let _ = self.stop.send(true);
        }
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }
}

#[derive(serde::Serialize, zbus::zvariant::Type)]
struct MenuNode {
    id: i32,
    properties: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    children: Vec<zbus::zvariant::OwnedValue>,
}

fn root_menu() -> MenuNode {
    MenuNode {
        id: 0,
        properties: HashMap::from([(
            "children-display".into(),
            owned("submenu"),
        )]),
        children: vec![owned_node(action(OPEN_ITEM, "Open")), owned_node(action(QUIT_ITEM, "Quit"))],
    }
}

fn action(id: i32, label: &str) -> MenuNode {
    MenuNode {
        id,
        properties: HashMap::from([
            ("label".into(), owned(label)),
            ("enabled".into(), owned_bool(true)),
            ("visible".into(), owned_bool(true)),
        ]),
        children: Vec::new(),
    }
}

fn owned(text: &str) -> zbus::zvariant::OwnedValue {
    zbus::zvariant::Value::from(text).try_into().expect("text")
}

fn owned_bool(value: bool) -> zbus::zvariant::OwnedValue {
    zbus::zvariant::Value::from(value).try_into().expect("bool")
}

fn node_value(node: MenuNode) -> zbus::zvariant::Value<'static> {
    let structure = zbus::zvariant::Structure::try_from((node.id, node.properties, node.children))
        .expect("menu structure");
    zbus::zvariant::Value::from(structure)
}

fn owned_node(node: MenuNode) -> zbus::zvariant::OwnedValue {
    node_value(node).try_into().expect("owned menu node")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_process_picks_one_service_string() {
        assert_eq!(
            notifier_service(Some("org.kde.StatusNotifierItem-icloud-1")),
            "org.kde.StatusNotifierItem-icloud-1"
        );
        assert_eq!(notifier_service(None), "/StatusNotifierItem");
    }

    #[test]
    fn quit_menu_matches_the_dbusmenu_layout() {
        let value = node_value(root_menu());
        assert_eq!(value.value_signature().as_str(), "(ia{sv}av)");
        let text = format!("{value:?}");
        assert!(text.contains("Quit"));
        assert!(text.contains("Open"));
    }
}

fn presentation(status: &icloud_engine::SyncStatus) -> (String, Vec<(i32, i32, Vec<u8>)>, String) {
    let sni_status = if status.attention > 0 || status.phase == Phase::NeedsAuth || status.phase == Phase::Error {
        "NeedsAttention"
    } else {
        "Active"
    };
    let fraction = match status.bytes_total {
        Some(total) if total > 0 => Some(status.bytes_done as f32 / total as f32),
        _ => None,
    };
    let label = if let Some(fraction) = fraction {
        format!("{} · {}%", status.label, (fraction * 100.0).round() as u32)
    } else {
        status.label.clone()
    };
    let pixmap = if status.phase == Phase::Downloading || status.phase == Phase::Uploading {
        vec![(64, 64, draw_ring(fraction.unwrap_or(0.05)))]
    } else {
        Vec::new()
    };
    (label, pixmap, sni_status.to_string())
}

fn draw_ring(fraction: f32) -> Vec<u8> {
    let mut pixels = vec![0u8; 64 * 64 * 4];
    let fraction = fraction.clamp(0.0, 1.0);
    for y in 0..64 {
        for x in 0..64 {
            let dx = x as f32 - 31.5;
            let dy = y as f32 - 31.5;
            let dist = (dx * dx + dy * dy).sqrt();
            if !(22.0..30.0).contains(&dist) {
                continue;
            }
            let angle = dy.atan2(dx);
            let mut turned = angle + std::f32::consts::FRAC_PI_2;
            if turned < 0.0 {
                turned += std::f32::consts::TAU;
            }
            let along = turned / std::f32::consts::TAU;
            let on = along <= fraction.max(0.02);
            let index = ((y * 64 + x) * 4) as usize;
            pixels[index] = 255;
            if on {
                pixels[index + 1] = 90;
                pixels[index + 2] = 170;
                pixels[index + 3] = 255;
            } else {
                pixels[index + 1] = 160;
                pixels[index + 2] = 170;
                pixels[index + 3] = 180;
            }
        }
    }
    pixels
}

#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait Watcher {
    fn register_status_notifier_item(&self, service: &str) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: std::collections::HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

#[allow(dead_code)]
fn _value(value: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(value).unwrap_or(OwnedValue::from(0i32))
}

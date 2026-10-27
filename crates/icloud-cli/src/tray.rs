use std::sync::Arc;
use std::time::Duration;

use icloud_engine::{Engine, Phase};
use zbus::zvariant::{OwnedValue, Value};
use zbus::{interface, Connection};

pub async fn run(connection: Connection, engine: Arc<Engine>) -> zbus::Result<()> {
    let item = TrayItem {
        engine,
        pixmap: std::sync::Mutex::new(Vec::new()),
        tooltip: std::sync::Mutex::new("iCloud Drive".to_string()),
        status: std::sync::Mutex::new("Active".to_string()),
    };
    connection.object_server().at("/StatusNotifierItem", item).await?;
    let name = format!("org.kde.StatusNotifierItem-icloud-{}", std::process::id());
    let _ = connection.request_name(name.clone()).await;
    let watcher = WatcherProxy::new(&connection).await?;
    watcher.register_status_notifier_item(&name).await.or(watcher.register_status_notifier_item("/StatusNotifierItem").await)?;

    let notifications = NotificationsProxy::new(&connection).await.ok();
    let mut replaces_id = 0u32;
    let mut last_note = String::new();
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let iface = connection.object_server().interface::<_, TrayItem>("/StatusNotifierItem").await?;
        let status = iface.get().await.engine.status();
        let (label, pixmap, sni_status) = presentation(&status);
        {
            let mut item = iface.get_mut().await;
            *item.tooltip.lock().expect("tooltip") = label.clone();
            *item.pixmap.lock().expect("pixmap") = pixmap;
            *item.status.lock().expect("status") = sni_status;
        }
        let context = iface.signal_context();
        let _ = iface.get().await.icon_pixmap_changed(context).await;
        let _ = iface.get().await.status_changed(context).await;
        let _ = iface.get().await.tool_tip_changed(context).await;
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

struct TrayItem {
    engine: Arc<Engine>,
    pixmap: std::sync::Mutex<Vec<(i32, i32, Vec<u8>)>>,
    tooltip: std::sync::Mutex<String>,
    status: std::sync::Mutex<String>,
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl TrayItem {
    fn activate(&self, _x: i32, _y: i32) {
        let _ = std::process::Command::new("icloud-drive-gui").spawn();
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

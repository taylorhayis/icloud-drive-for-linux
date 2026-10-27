mod tray;

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use icloud_engine::Engine;
use icloud_ipc::{
    AttentionSnapshot, SearchHit, ServiceProxy, StatusSnapshot, BUS_NAME, BUS_PATH,
};
use icloud_proto::IcloudClient;
use serde::Deserialize;
use zbus::{interface, ConnectionBuilder};

#[derive(Parser)]
#[command(name = "icloud-drive", about = "iCloud Drive for Linux")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the mount and sync service.
    Daemon,
    /// Show sync status and items that need attention.
    Status,
    /// Search the local filename index.
    Search { query: String },
    /// Keep a file or folder downloaded.
    Pin { path: String },
    /// Stop keeping a file or folder downloaded.
    Unpin { path: String },
    /// Ask the service to refresh iCloud now.
    Sync,
    /// Follow the service log in the journal.
    Logs,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    mount_point: Option<String>,
    allow_deletion: Option<bool>,
    poll_seconds: Option<u32>,
    china: Option<bool>,
}

struct Paths {
    database: PathBuf,
    blobs: PathBuf,
    session: PathBuf,
    config: PathBuf,
    mount: PathBuf,
    allow_deletion: bool,
    poll_seconds: u32,
    china: bool,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "icloud_drive=info,icloud_proto=info,icloud_engine=info".into()),
        )
        .init();
    let cli = Cli::parse();
    if let Err(err) = run(cli).await {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Daemon => daemon().await,
        Command::Status => client_status().await,
        Command::Search { query } => client_search(&query).await,
        Command::Pin { path } => client_pin(&path, true).await,
        Command::Unpin { path } => client_pin(&path, false).await,
        Command::Sync => client_sync().await,
        Command::Logs => logs(),
    }
}

async fn daemon() -> Result<(), String> {
    let paths = load_paths()?;
    let client = match load_session(&paths.session) {
        Some(saved) => IcloudClient::from_saved(saved).map_err(|err| err.to_string())?,
        None => IcloudClient::new(paths.china).map_err(|err| err.to_string())?,
    };
    let engine = Arc::new(
        Engine::open(
            &paths.database,
            &paths.blobs,
            client,
            paths.allow_deletion,
            paths.poll_seconds,
        )
        .map_err(|err| err.to_string())?,
    );
    let service = DaemonService {
        engine: Arc::clone(&engine),
        mount_point: paths.mount.display().to_string(),
        session_path: paths.session.clone(),
        config_path: paths.config.clone(),
    };
    let connection = ConnectionBuilder::session()
        .map_err(|err| err.to_string())?
        .name(BUS_NAME)
        .map_err(|err| err.to_string())?
        .serve_at(BUS_PATH, service)
        .map_err(|err| err.to_string())?
        .build()
        .await
        .map_err(|err| err.to_string())?;

    let tray_engine = Arc::clone(&engine);
    let conn_for_tray = connection.clone();
    tokio::spawn(async move {
        if let Err(err) = tray::run(conn_for_tray, tray_engine).await {
            tracing::warn!("panel icon is unavailable: {err}");
        }
    });

    let mount_engine = Arc::clone(&engine);
    let mount_point = paths.mount.clone();
    let rt = tokio::runtime::Handle::current();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = icloud_mount::mount_blocking(mount_engine, &mount_point, rt);
        let _ = tx.send(result);
    });

    let engine_loop = Arc::clone(&engine);
    tokio::spawn(async move {
        engine_loop.run_until_shutdown().await;
    });

    wait_for_stop().await;
    engine.shutdown();
    let mount = paths.mount.clone();
    let _ = std::process::Command::new("fusermount3").args(["-u", &mount.display().to_string()]).status();
    let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
    drop(connection);
    Ok(())
}

async fn wait_for_stop() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            if let Some(signal) = terminate.as_mut() {
                signal.recv().await;
            }
        } => {}
    }
}

struct DaemonService {
    engine: Arc<Engine>,
    mount_point: String,
    session_path: PathBuf,
    config_path: PathBuf,
}

#[interface(name = "dev.icloud_drive.Service")]
impl DaemonService {
    fn status(&self) -> StatusSnapshot {
        let status = self.engine.status();
        StatusSnapshot {
            phase: status.phase.as_str().to_string(),
            label: status.label,
            current_path: status.current_path,
            bytes_done: status.bytes_done,
            bytes_total: status.bytes_total.unwrap_or(0),
            bytes_known: status.bytes_total.is_some(),
            items_done: status.items_done,
            items_total: status.items_total.unwrap_or(0),
            items_known: status.items_total.is_some(),
            attention_count: status.attention,
        }
    }

    fn attention(&self) -> Vec<AttentionSnapshot> {
        self.engine
            .attention()
            .unwrap_or_default()
            .into_iter()
            .map(|item| AttentionSnapshot {
                id: item.id,
                kind: item.kind.as_str().to_string(),
                path: item.path,
                detail: item.detail,
            })
            .collect()
    }

    fn search(&self, query: &str) -> Vec<SearchHit> {
        self.engine
            .search(query)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|node| {
                let path = node_path(&self.engine, node.ino).ok()?;
                Some(SearchHit {
                    path,
                    kind: if node.kind == icloud_engine::NodeKind::Directory {
                        "folder".into()
                    } else {
                        "file".into()
                    },
                    size: node.size,
                    pinned: node.pin,
                })
            })
            .collect()
    }

    async fn begin_sign_in(&self, apple_id: &str, password: &str) -> zbus::fdo::Result<String> {
        self.auth_call(self.engine.sign_in(apple_id, password).await).await
    }

    async fn submit_code(&self, code: &str) -> zbus::fdo::Result<String> {
        self.auth_call(self.engine.submit_code(code).await).await
    }

    async fn request_sms(&self) -> zbus::fdo::Result<String> {
        self.auth_call(self.engine.request_sms().await).await
    }

    async fn accept_terms(&self) -> zbus::fdo::Result<String> {
        self.auth_call(self.engine.accept_terms().await).await
    }

    async fn request_device_approval(&self) -> zbus::fdo::Result<String> {
        self.auth_call(self.engine.request_device_approval().await).await
    }

    fn set_pinned(&self, path: &str, pinned: bool) -> zbus::fdo::Result<()> {
        self.engine
            .set_pin(path, pinned)
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    fn set_allow_deletion(&self, enabled: bool) -> zbus::fdo::Result<()> {
        self.engine.set_allow_deletion(enabled);
        let _ = persist_allow_deletion(&self.config_path, enabled);
        Ok(())
    }

    fn allow_deletion(&self) -> bool {
        self.engine.allow_deletion()
    }

    fn resolve_attention(&self, id: i64, action: &str) -> zbus::fdo::Result<()> {
        self.engine
            .resolve_attention(id, action)
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    fn sync_now(&self) -> zbus::fdo::Result<()> {
        self.engine.poke_sync();
        Ok(())
    }

    fn mount_point(&self) -> String {
        self.mount_point.clone()
    }
}

impl DaemonService {
    async fn auth_call(&self, result: icloud_engine::Result<icloud_proto::AuthStep>) -> zbus::fdo::Result<String> {
        let step = result.map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
        if matches!(step, icloud_proto::AuthStep::SignedIn) {
            let saved = self.engine.export_session().await;
            if let Ok(bytes) = serde_json::to_vec(&saved) {
                let _ = store_session(&self.session_path, &bytes);
            }
        }
        Ok(match step {
            icloud_proto::AuthStep::SignedIn => "signed_in".into(),
            icloud_proto::AuthStep::NeedsCode => "needs_code".into(),
            icloud_proto::AuthStep::NeedsTerms => "needs_terms".into(),
            icloud_proto::AuthStep::NeedsDeviceApproval => "needs_device".into(),
            icloud_proto::AuthStep::Failed(message) => format!("failed:{message}"),
        })
    }
}

async fn proxy() -> Result<ServiceProxy<'static>, String> {
    let connection = zbus::Connection::session().await.map_err(|err| err.to_string())?;
    ServiceProxy::new(&connection)
        .await
        .map_err(|_| "iCloud Drive is not running. Open the app or run icloud-drive daemon.".to_string())
}

async fn client_status() -> Result<(), String> {
    let proxy = proxy().await?;
    let status = proxy.status().await.map_err(|err| err.to_string())?;
    println!("{} — {}", status.phase, status.label);
    if status.bytes_known && status.bytes_total > 0 {
        println!("file {} / {} bytes", status.bytes_done, status.bytes_total);
    }
    if status.items_done > 0 {
        println!("indexed {} folders", status.items_done);
    }
    println!("{} item(s) need attention", status.attention_count);
    for item in proxy.attention().await.map_err(|err| err.to_string())? {
        println!("- [{}] {} — {}", item.kind, item.path, item.detail);
    }
    Ok(())
}

async fn client_search(query: &str) -> Result<(), String> {
    let proxy = proxy().await?;
    for hit in proxy.search(query).await.map_err(|err| err.to_string())? {
        println!("{}  {}  {}", hit.path, hit.size, if hit.pinned { "pinned" } else { "cloud" });
    }
    Ok(())
}

async fn client_pin(path: &str, pinned: bool) -> Result<(), String> {
    let proxy = proxy().await?;
    proxy.set_pinned(path, pinned).await.map_err(|err| err.to_string())
}

async fn client_sync() -> Result<(), String> {
    let proxy = proxy().await?;
    proxy.sync_now().await.map_err(|err| err.to_string())
}

fn logs() -> Result<(), String> {
    let status = std::process::Command::new("journalctl")
        .args(["--user", "-u", "icloud-drive.service", "-n", "200", "--no-pager"])
        .status()
        .map_err(|err| err.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("journalctl could not read the icloud-drive user service".into())
    }
}

fn load_paths() -> Result<Paths, String> {
    let config_dir = dirs::config_dir().ok_or("no config directory")?.join("icloud-drive");
    let data_dir = dirs::data_dir().ok_or("no data directory")?.join("icloud-drive");
    let cache_dir = dirs::cache_dir().ok_or("no cache directory")?.join("icloud-drive");
    let state_dir = dirs::state_dir().unwrap_or(data_dir.clone());
    let config_path = config_dir.join("config.toml");
    let file_config = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|text| toml::from_str::<FileConfig>(&text).ok())
        .unwrap_or_default();
    let home = dirs::home_dir().ok_or("no home directory")?;
    let mount = file_config
        .mount_point
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("iCloud Drive"));
    std::fs::create_dir_all(&state_dir).ok();
    Ok(Paths {
        database: data_dir.join("index.sqlite3"),
        blobs: cache_dir.join("blobs"),
        session: state_dir.join("icloud-drive").join("session.json"),
        config: config_path,
        mount,
        allow_deletion: file_config.allow_deletion.unwrap_or(false),
        poll_seconds: file_config.poll_seconds.unwrap_or(300),
        china: file_config.china.unwrap_or(false),
    })
}

fn load_session(path: &std::path::Path) -> Option<icloud_proto::SavedSession> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn persist_allow_deletion(path: &std::path::Path, enabled: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let line = format!("allow_deletion = {enabled}");
    let mut found = false;
    let mut lines: Vec<String> = existing
        .lines()
        .map(|row| {
            if row.trim_start().starts_with("allow_deletion") {
                found = true;
                line.clone()
            } else {
                row.to_string()
            }
        })
        .collect();
    if !found {
        lines.push(line);
    }
    let mut text = lines.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    std::fs::write(path, text)
}

fn store_session(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    let mut file = OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

fn node_path(engine: &Engine, ino: i64) -> icloud_engine::Result<String> {
    let node = engine.getattr(ino)?;
    if node.parent == 0 || node.ino == 1 {
        return Ok("/".into());
    }
    let parent = node_path(engine, node.parent)?;
    if parent == "/" {
        Ok(format!("/{}", node.name))
    } else {
        Ok(format!("{parent}/{}", node.name))
    }
}

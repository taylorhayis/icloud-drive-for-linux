use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib::{self, ControlFlow};
use icloud_ipc::{AttentionSnapshot, ServiceProxy, StatusSnapshot};

struct Widgets {
    status: gtk::Label,
    detail: gtk::Label,
    progress: gtk::ProgressBar,
    attention: gtk::ListBox,
    search_results: gtk::ListBox,
    apple_id: gtk::Entry,
    password: gtk::PasswordEntry,
    code: gtk::Entry,
    auth_status: gtk::Label,
    allow_deletion: gtk::Switch,
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("dev.icloud_drive.Gui")
        .build();
    app.connect_activate(build);
    app.run()
}

fn build(app: &adw::Application) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("iCloud Drive")
        .default_width(720)
        .default_height(640)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    root.append(&header);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);

    let status = gtk::Label::new(Some("Connecting to the sync service"));
    status.set_xalign(0.0);
    status.add_css_class("title-2");
    let detail = gtk::Label::new(Some(""));
    detail.set_xalign(0.0);
    let progress = gtk::ProgressBar::new();
    progress.set_show_text(true);
    content.append(&status);
    content.append(&detail);
    content.append(&progress);

    let auth = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let apple_id = gtk::Entry::builder().placeholder_text("Apple ID").build();
    let password = gtk::PasswordEntry::builder().placeholder_text("Password").build();
    let code = gtk::Entry::builder().placeholder_text("Verification code").build();
    let auth_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let sign_in = gtk::Button::with_label("Sign in");
    let submit_code = gtk::Button::with_label("Submit code");
    let sms = gtk::Button::with_label("Text me a code");
    let terms = gtk::Button::with_label("Accept terms");
    let approve = gtk::Button::with_label("Approve on iPhone");
    auth_buttons.append(&sign_in);
    auth_buttons.append(&submit_code);
    auth_buttons.append(&sms);
    auth_buttons.append(&terms);
    auth_buttons.append(&approve);
    let auth_status = gtk::Label::new(Some("The password is not saved. A trusted session is."));
    auth_status.set_xalign(0.0);
    auth.append(&apple_id);
    auth.append(&password);
    auth.append(&code);
    auth.append(&auth_buttons);
    auth.append(&auth_status);
    content.append(&auth);

    content.append(&section_label("Needs attention"));
    let attention = gtk::ListBox::new();
    attention.add_css_class("boxed-list");
    content.append(&attention);

    content.append(&section_label("Search"));
    let search = gtk::SearchEntry::new();
    let search_results = gtk::ListBox::new();
    search_results.add_css_class("boxed-list");
    content.append(&search);
    content.append(&search_results);

    let deletion_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let deletion_label = gtk::Label::new(Some("Allow deletion"));
    deletion_label.set_hexpand(true);
    deletion_label.set_xalign(0.0);
    let allow_deletion = gtk::Switch::new();
    deletion_row.append(&deletion_label);
    deletion_row.append(&allow_deletion);
    content.append(&deletion_row);
    let deletion_note = gtk::Label::new(Some(
        "Off by default. Saving a file still works. Turning this on can remove files from iCloud Drive.",
    ));
    deletion_note.set_xalign(0.0);
    deletion_note.set_wrap(true);
    content.append(&deletion_note);

    let open_folder = gtk::Button::with_label("Open iCloud Drive");
    content.append(&open_folder);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&content));
    root.append(&scroll);
    window.set_content(Some(&root));

    let widgets = Rc::new(Widgets {
        status,
        detail,
        progress,
        attention,
        search_results,
        apple_id,
        password,
        code,
        auth_status,
        allow_deletion,
    });

    let (tx, rx) = std::sync::mpsc::channel::<UiMsg>();

    let sign_widgets = Rc::clone(&widgets);
    let sign_tx = tx.clone();
    sign_in.connect_clicked(move |_| {
        call_auth(sign_tx.clone(), &sign_widgets, "begin");
    });
    let code_widgets = Rc::clone(&widgets);
    let code_tx = tx.clone();
    submit_code.connect_clicked(move |_| call_auth(code_tx.clone(), &code_widgets, "code"));
    let sms_widgets = Rc::clone(&widgets);
    let sms_tx = tx.clone();
    sms.connect_clicked(move |_| call_auth(sms_tx.clone(), &sms_widgets, "sms"));
    let terms_widgets = Rc::clone(&widgets);
    let terms_tx = tx.clone();
    terms.connect_clicked(move |_| call_auth(terms_tx.clone(), &terms_widgets, "terms"));
    let approve_widgets = Rc::clone(&widgets);
    let approve_tx = tx.clone();
    approve.connect_clicked(move |_| call_auth(approve_tx.clone(), &approve_widgets, "approve"));

    let search_tx = tx.clone();
    search.connect_search_changed(move |entry| {
        let query = entry.text().to_string();
        let tx = search_tx.clone();
        std::thread::spawn(move || {
            let hits = block_on(async move {
                let connection = zbus::Connection::session().await.ok()?;
                let proxy = ServiceProxy::new(&connection).await.ok()?;
                proxy.search(&query).await.ok()
            });
            if let Some(hits) = hits {
                let _ = tx.send(UiMsg::Search(hits));
            }
        });
    });

    let deletion_widgets = Rc::clone(&widgets);
    widgets.allow_deletion.clone().connect_state_set(move |_, state| {
        if state {
            let widgets = Rc::clone(&deletion_widgets);
            confirm_deletion(move || {
                let _ = std::thread::spawn(move || {
                    let _ = block_on(async {
                        let connection = zbus::Connection::session().await.ok()?;
                        let proxy = ServiceProxy::new(&connection).await.ok()?;
                        proxy.set_allow_deletion(true).await.ok()?;
                        Some(())
                    });
                });
                widgets.allow_deletion.set_state(true);
            });
            return true.into();
        }
        let _ = std::thread::spawn(|| {
            let _ = block_on(async {
                let connection = zbus::Connection::session().await.ok()?;
                let proxy = ServiceProxy::new(&connection).await.ok()?;
                proxy.set_allow_deletion(false).await.ok()
            });
        });
        false.into()
    });

    open_folder.connect_clicked(|_| {
        let _ = std::thread::spawn(|| {
            let mount = block_on(async {
                let connection = zbus::Connection::session().await.ok()?;
                let proxy = ServiceProxy::new(&connection).await.ok()?;
                proxy.mount_point().await.ok()
            });
            if let Some(mount) = mount {
                let _ = std::process::Command::new("xdg-open").arg(mount).spawn();
            }
        });
    });

    let poll_tx = tx.clone();
    std::thread::spawn(move || loop {
        let update = block_on(poll());
        if poll_tx.send(UiMsg::Poll(update)).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(700));
    });
    let tick_widgets = Rc::clone(&widgets);
    let rx = Rc::new(RefCell::new(rx));
    glib::timeout_add_local(Duration::from_millis(300), move || {
        let mut poll = None;
        let mut searches = Vec::new();
        let mut auths = Vec::new();
        while let Ok(msg) = rx.borrow_mut().try_recv() {
            match msg {
                UiMsg::Poll(update) => poll = Some(update),
                UiMsg::Search(hits) => searches.push(hits),
                UiMsg::Auth(message) => auths.push(message),
            }
        }
        if let Some(update) = poll {
            match update {
                Some((status, attention, allow)) => apply_status(&tick_widgets, &status, &attention, allow),
                None => {
                    tick_widgets.status.set_text("Sync service is not running");
                    tick_widgets.detail.set_text("Start it with icloud-drive daemon");
                }
            }
        }
        for hits in searches {
            fill_search(&tick_widgets, &hits);
        }
        for message in auths {
            if message == "signed_in" {
                tick_widgets.password.set_text("");
                tick_widgets.auth_status.set_text("Signed in");
            } else {
                tick_widgets.auth_status.set_text(&message);
            }
        }
        ControlFlow::Continue
    });

    window.present();
}

fn section_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("heading");
    label
}

fn apply_status(widgets: &Widgets, status: &StatusSnapshot, attention: &[AttentionSnapshot], allow: bool) {
    widgets.status.set_text(&status.label);
    let mut detail = status.phase.clone();
    if !status.current_path.is_empty() {
        detail.push_str(" · ");
        detail.push_str(&status.current_path);
    }
    if status.attention_count > 0 {
        detail.push_str(&format!(" · {} need attention", status.attention_count));
    }
    widgets.detail.set_text(&detail);
    if status.bytes_known && status.bytes_total > 0 {
        widgets.progress.set_fraction(status.bytes_done as f64 / status.bytes_total as f64);
        widgets.progress.set_text(Some(&format!(
            "{} / {} bytes",
            status.bytes_done, status.bytes_total
        )));
    } else if status.phase == "indexing" {
        widgets.progress.pulse();
        widgets.progress.set_text(Some(&format!("{} folders", status.items_done)));
    } else {
        widgets.progress.set_fraction(0.0);
        widgets.progress.set_text(Some(""));
    }
    if widgets.allow_deletion.state() != allow {
        widgets.allow_deletion.set_state(allow);
    }
    clear(&widgets.attention);
    for item in attention {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_margin_top(8);
        row.set_margin_bottom(8);
        row.set_margin_start(12);
        row.set_margin_end(12);
        let text = gtk::Label::new(Some(&format!("{}\n{}", item.path, item.detail)));
        text.set_xalign(0.0);
        text.set_hexpand(true);
        row.append(&text);
        for (label, action) in actions_for(&item.kind) {
            let id = item.id;
            let button = gtk::Button::with_label(label);
            button.connect_clicked(move |_| {
                let action = action.to_string();
                std::thread::spawn(move || {
                    let _ = block_on(async {
                        let connection = zbus::Connection::session().await.ok()?;
                        let proxy = ServiceProxy::new(&connection).await.ok()?;
                        proxy.resolve_attention(id, &action).await.ok()
                    });
                });
            });
            row.append(&button);
        }
        widgets.attention.append(&row);
    }
}

fn actions_for(kind: &str) -> Vec<(&'static str, &'static str)> {
    match kind {
        "conflict" => vec![
            ("Keep mine", "keep_mine"),
            ("Keep theirs", "keep_theirs"),
            ("Keep both", "keep_both"),
        ],
        "stalled_download" | "stalled_upload" => vec![("Retry", "retry"), ("Discard partial", "discard")],
        "remote_missing" => vec![("Refresh", "refresh"), ("Dismiss", "dismiss")],
        "folder_skipped" => vec![("Retry folder", "retry")],
        _ => vec![("Dismiss", "dismiss")],
    }
}

fn fill_search(widgets: &Widgets, hits: &[icloud_ipc::SearchHit]) {
    clear(&widgets.search_results);
    for hit in hits {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_margin_top(8);
        row.set_margin_bottom(8);
        row.set_margin_start(12);
        row.set_margin_end(12);
        let text = gtk::Label::new(Some(&format!("{}  ·  {}", hit.path, hit.kind)));
        text.set_hexpand(true);
        text.set_xalign(0.0);
        row.append(&text);
        let path = hit.path.clone();
        let pin = gtk::Button::with_label(if hit.pinned { "Unpin" } else { "Keep downloaded" });
        let pinned = hit.pinned;
        pin.connect_clicked(move |_| {
            let path = path.clone();
            std::thread::spawn(move || {
                let _ = block_on(async {
                    let connection = zbus::Connection::session().await.ok()?;
                    let proxy = ServiceProxy::new(&connection).await.ok()?;
                    proxy.set_pinned(&path, !pinned).await.ok()
                });
            });
        });
        row.append(&pin);
        widgets.search_results.append(&row);
    }
}

fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn call_auth(tx: std::sync::mpsc::Sender<UiMsg>, widgets: &Rc<Widgets>, kind: &str) {
    let apple_id = widgets.apple_id.text().to_string();
    let password = widgets.password.text().to_string();
    let code = widgets.code.text().to_string();
    let kind = kind.to_string();
    std::thread::spawn(move || {
        let result = block_on(async move {
            let connection = zbus::Connection::session().await.ok()?;
            let proxy = ServiceProxy::new(&connection).await.ok()?;
            let step = match kind.as_str() {
                "code" => proxy.submit_code(&code).await.ok()?,
                "sms" => proxy.request_sms().await.ok()?,
                "terms" => proxy.accept_terms().await.ok()?,
                "approve" => proxy.request_device_approval().await.ok()?,
                _ => proxy.begin_sign_in(&apple_id, &password).await.ok()?,
            };
            Some(step)
        });
        let message = result.unwrap_or_else(|| "Could not reach the sync service".into());
        let _ = tx.send(UiMsg::Auth(message));
    });
}

fn confirm_deletion(on_yes: impl Fn() + 'static) {
    let dialog = adw::MessageDialog::new(
        None::<&gtk::Window>,
        Some("Allow deletion?"),
        Some("This can remove files from iCloud Drive, including from your other devices."),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("allow", "Allow deletion");
    dialog.set_response_appearance("allow", adw::ResponseAppearance::Destructive);
    let on_yes = Rc::new(RefCell::new(Some(on_yes)));
    dialog.connect_response(None, move |dialog, response| {
        if response == "allow" {
            if let Some(action) = on_yes.borrow_mut().take() {
                action();
            }
        }
        dialog.close();
    });
    dialog.present();
}

type PollUpdate = Option<(StatusSnapshot, Vec<AttentionSnapshot>, bool)>;

enum UiMsg {
    Poll(PollUpdate),
    Search(Vec<icloud_ipc::SearchHit>),
    Auth(String),
}

async fn poll() -> PollUpdate {
    let connection = zbus::Connection::session().await.ok()?;
    let proxy = ServiceProxy::new(&connection).await.ok()?;
    let status = proxy.status().await.ok()?;
    let attention = proxy.attention().await.ok()?;
    let allow = proxy.allow_deletion().await.unwrap_or(false);
    Some((status, attention, allow))
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

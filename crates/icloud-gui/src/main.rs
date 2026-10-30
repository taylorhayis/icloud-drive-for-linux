use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;

use adw::prelude::*;
use gtk::gio;
use gtk::glib::{self, ControlFlow};
use icloud_ipc::{AttentionSnapshot, ServiceProxy, StatusSnapshot};

/// Set by the command line before the first window is built.
static LAUNCH_IN_BACKGROUND: AtomicBool = AtomicBool::new(false);

struct Widgets {
    status: gtk::Label,
    detail: gtk::Label,
    progress: gtk::ProgressBar,
    attention: gtk::ListBox,
    search_results: gtk::ListBox,
    apple_id: gtk::Entry,
    password: gtk::PasswordEntry,
    sign_in: gtk::Button,
    code: gtk::Entry,
    submit_code: gtk::Button,
    sms: gtk::Button,
    terms: gtk::Button,
    approve: gtk::Button,
    auth_status: gtk::Label,
    auth: gtk::Box,
    /// True after this window finished a sign-in, until the service leaves `needs_auth`.
    hold_login: Cell<bool>,
    allow_deletion: gtk::Switch,
    start: gtk::Button,
    quit: gtk::Button,
    crash_alerted: Cell<bool>,
    seen_service: Cell<bool>,
    folder_path: gtk::Label,
    folder_list: gtk::ListBox,
    sync_roots: gtk::ListBox,
    sync_all: gtk::Button,
    folders_signature: RefCell<String>,
    activity: gtk::ListBox,
    activity_signature: RefCell<String>,
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("dev.icloud_drive.Gui")
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.connect_command_line(|app, command_line| {
        if let Some(window) = app.windows().first().cloned() {
            window.present();
            return 0;
        }
        let background = command_line.arguments().iter().any(|arg| arg == "--background");
        LAUNCH_IN_BACKGROUND.store(background, Ordering::Relaxed);
        app.activate();
        0
    });
    app.connect_activate(build);
    app.run()
}

fn build(app: &adw::Application) {
    let background = LAUNCH_IN_BACKGROUND.swap(false, Ordering::Relaxed);
    // The guard releases the hold when dropped. Closing the window hides it,
    // so this process has to stay up and keep watching the daemon.
    std::mem::forget(app.hold());
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("iCloud Drive")
        .default_width(720)
        .default_height(640)
        .hide_on_close(true)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    let start = gtk::Button::with_label("Start");
    start.add_css_class("suggested-action");
    let quit = gtk::Button::with_label("Quit");
    quit.set_visible(false);
    header.pack_start(&start);
    header.pack_end(&quit);
    let stack = adw::ViewStack::new();
    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    header.set_title_widget(Some(&switcher));
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
    code.set_visible(false);
    let sign_in = gtk::Button::with_label("Sign in");
    sign_in.add_css_class("suggested-action");
    let submit_code = gtk::Button::with_label("Submit code");
    submit_code.set_visible(false);
    submit_code.add_css_class("suggested-action");
    let sms = gtk::Button::with_label("Text me a code");
    sms.set_visible(false);
    let terms = gtk::Button::with_label("Accept terms");
    terms.set_visible(false);
    let approve = gtk::Button::with_label("I've approved it");
    approve.set_visible(false);
    let auth_status = gtk::Label::new(Some("The password is not saved."));
    auth_status.set_xalign(0.0);
    auth_status.set_wrap(true);
    auth.append(&apple_id);
    auth.append(&password);
    auth.append(&sign_in);
    auth.append(&code);
    auth.append(&submit_code);
    auth.append(&sms);
    auth.append(&terms);
    auth.append(&approve);
    auth.append(&auth_status);
    auth.set_visible(false);
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

    content.append(&section_label("Folders to sync"));
    let folder_note = gtk::Label::new(Some(
        "Chosen folders are saved into ~/iCloud Drive with their real contents. A file shows up only after it has fully downloaded, so a photo viewer does not open an empty file. Edits in that folder upload back. You can choose the whole drive.",
    ));
    folder_note.set_xalign(0.0);
    folder_note.set_wrap(true);
    content.append(&folder_note);
    let sync_roots = gtk::ListBox::new();
    sync_roots.add_css_class("boxed-list");
    content.append(&sync_roots);
    let sync_all = gtk::Button::with_label("Sync entire iCloud Drive");
    content.append(&sync_all);
    let folder_nav = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let folder_back = gtk::Button::with_label("Back");
    let folder_path = gtk::Label::new(Some("/"));
    folder_path.set_xalign(0.0);
    folder_path.set_hexpand(true);
    folder_nav.append(&folder_back);
    folder_nav.append(&folder_path);
    content.append(&folder_nav);
    let folder_list = gtk::ListBox::new();
    folder_list.add_css_class("boxed-list");
    content.append(&folder_list);

    let deletion_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let deletion_label = gtk::Label::new(Some("Allow deletion"));
    deletion_label.set_hexpand(true);
    deletion_label.set_xalign(0.0);
    let allow_deletion = gtk::Switch::new();
    deletion_row.append(&deletion_label);
    deletion_row.append(&allow_deletion);
    content.append(&deletion_row);
    let deletion_note = gtk::Label::new(Some(
        "Off by default. Pinned files stay in the iCloud Drive folder after you quit. Turning this on can remove them from iCloud.",
    ));
    deletion_note.set_xalign(0.0);
    deletion_note.set_wrap(true);
    content.append(&deletion_note);

    let open_folder = gtk::Button::with_label("Open iCloud Drive");
    content.append(&open_folder);

    let startup_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let startup_label = gtk::Label::new(Some("Start when you log in"));
    startup_label.set_hexpand(true);
    startup_label.set_xalign(0.0);
    let startup = gtk::Switch::new();
    startup.set_active(autostart_is_enabled());
    startup_row.append(&startup_label);
    startup_row.append(&startup);
    content.append(&startup_row);
    let startup_note = gtk::Label::new(Some(STARTUP_NOTE));
    startup_note.set_xalign(0.0);
    startup_note.set_wrap(true);
    content.append(&startup_note);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&content));
    stack.add_titled(&scroll, Some("sync"), "Sync");

    let activity_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    activity_page.set_margin_top(16);
    activity_page.set_margin_bottom(16);
    activity_page.set_margin_start(16);
    activity_page.set_margin_end(16);
    let activity_note = gtk::Label::new(Some(
        "Downloads from iCloud, uploads from this computer, and other sync events. Newest first.",
    ));
    activity_note.set_xalign(0.0);
    activity_note.set_wrap(true);
    activity_page.append(&activity_note);
    let activity = gtk::ListBox::new();
    activity.add_css_class("boxed-list");
    activity_page.append(&activity);
    let activity_scroll = gtk::ScrolledWindow::new();
    activity_scroll.set_vexpand(true);
    activity_scroll.set_child(Some(&activity_page));
    stack.add_titled(&activity_scroll, Some("activity"), "Activity");
    root.append(&stack);
    window.set_content(Some(&root));

    let widgets = Rc::new(Widgets {
        status,
        detail,
        progress,
        attention,
        search_results,
        apple_id,
        password,
        sign_in: sign_in.clone(),
        code,
        submit_code: submit_code.clone(),
        sms: sms.clone(),
        terms: terms.clone(),
        approve: approve.clone(),
        auth_status,
        auth,
        hold_login: Cell::new(false),
        allow_deletion,
        start: start.clone(),
        quit: quit.clone(),
        crash_alerted: Cell::new(false),
        seen_service: Cell::new(false),
        folder_path,
        folder_list,
        sync_roots,
        sync_all: sync_all.clone(),
        folders_signature: RefCell::new(String::new()),
        activity,
        activity_signature: RefCell::new(String::new()),
    });

    let browser = std::sync::Arc::new(std::sync::Mutex::new("/".to_string()));

    let (tx, rx) = std::sync::mpsc::channel::<UiMsg>();

    let sign_widgets = Rc::clone(&widgets);
    let sign_tx = tx.clone();
    sign_in.connect_clicked(move |_| {
        sign_widgets.auth_status.set_text("Signing in…");
        sign_widgets.sign_in.set_sensitive(false);
        call_auth(sign_tx.clone(), &sign_widgets, "begin");
    });
    let password_sign = sign_in.clone();
    widgets.password.connect_activate(move |_| {
        password_sign.emit_clicked();
    });
    let code_widgets = Rc::clone(&widgets);
    let code_tx = tx.clone();
    submit_code.connect_clicked(move |_| {
        code_widgets.auth_status.set_text("Checking the code…");
        code_widgets.submit_code.set_sensitive(false);
        call_auth(code_tx.clone(), &code_widgets, "code");
    });
    let code_submit = submit_code.clone();
    widgets.code.connect_activate(move |_| {
        code_submit.emit_clicked();
    });
    let sms_widgets = Rc::clone(&widgets);
    let sms_tx = tx.clone();
    sms.connect_clicked(move |_| {
        sms_widgets.auth_status.set_text("Sending a text…");
        call_auth(sms_tx.clone(), &sms_widgets, "sms");
    });
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

    quit.connect_clicked(|_| {
        std::thread::spawn(|| {
            let _ = block_on(async {
                let connection = zbus::Connection::session().await.ok()?;
                let proxy = ServiceProxy::new(&connection).await.ok()?;
                proxy.quit().await.ok()
            });
        });
    });
    start.connect_clicked(|_| {
        std::thread::spawn(start_daemon);
    });
    let startup_note_row = startup_note.clone();
    startup.connect_state_set(move |switch, enabled| {
        match set_autostart(enabled) {
            Ok(()) => {
                startup_note_row.set_text(STARTUP_NOTE);
                false.into()
            }
            Err(_) => {
                startup_note_row.set_text("Could not save the login setting.");
                switch.set_state(!enabled);
                true.into()
            }
        }
    });

    let back_browser = std::sync::Arc::clone(&browser);
    folder_back.connect_clicked(move |_| {
        let mut path = back_browser.lock().expect("browser");
        if *path == "/" {
            return;
        }
        match path.rfind('/') {
            Some(0) | None => *path = "/".into(),
            Some(index) => path.truncate(index),
        }
    });
    sync_all.connect_clicked(|_| {
        confirm(
            "Sync entire iCloud Drive?",
            "This downloads every file into ~/iCloud Drive. A large library can fill this disk. You can stop it later.",
            "Sync entire drive",
            false,
            || set_sync_folder("/", true),
        );
    });

    open_folder.connect_clicked(|_| {
        let _ = std::thread::spawn(|| {
            let mount = block_on(async {
                let connection = zbus::Connection::session().await.ok()?;
                let proxy = ServiceProxy::new(&connection).await.ok()?;
                proxy.mount_point().await.ok()
            });
            let mount = mount.unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                format!("{home}/iCloud Drive")
            });
            let _ = std::process::Command::new("xdg-open").arg(mount).spawn();
        });
    });

    let poll_tx = tx.clone();
    let poll_browser = std::sync::Arc::clone(&browser);
    std::thread::spawn(move || {
        block_on(watch_service(poll_tx, poll_browser));
    });
    let tick_widgets = Rc::clone(&widgets);
    let browser_ui = std::sync::Arc::clone(&browser);
    let rx = Rc::new(RefCell::new(rx));
    let alert_app = app.clone();
    let alert_window = window.clone();
    glib::timeout_add_local(Duration::from_millis(300), move || {
        let mut poll = None;
        let mut searches = Vec::new();
        let mut auths = Vec::new();
        let mut folders = None;
        let mut activity = None;
        let mut crashed = false;
        while let Ok(msg) = rx.borrow_mut().try_recv() {
            match msg {
                UiMsg::Poll(update) => poll = Some(update),
                UiMsg::Crashed => crashed = true,
                UiMsg::Search(hits) => searches.push(hits),
                UiMsg::Auth(message) => auths.push(message),
                UiMsg::Folders(update) => folders = Some(update),
                UiMsg::Activity(events) => activity = Some(events),
            }
        }
        if let Some(update) = poll {
            match update {
                Some((status, attention, allow)) => {
                    tick_widgets.crash_alerted.set(false);
                    tick_widgets.seen_service.set(true);
                    tick_widgets.start.set_visible(false);
                    tick_widgets.quit.set_visible(true);
                    apply_status(&tick_widgets, &status, &attention, allow)
                }
                None => {
                    show_stopped(&tick_widgets);
                    if tick_widgets.seen_service.replace(false) && !alert_window.is_visible() {
                        alert_window.present();
                    }
                }
            }
        }
        if crashed {
            show_stopped(&tick_widgets);
            if !tick_widgets.crash_alerted.get() {
                tick_widgets.crash_alerted.set(true);
                let note = gio::Notification::new("iCloud Drive stopped");
                note.set_body(Some("Sync stopped. Press Start to run it again."));
                alert_app.send_notification(Some("daemon-stopped"), &note);
                alert_window.present();
            }
        }
        if let Some((path, choices, roots)) = folders {
            fill_folders(&tick_widgets, &browser_ui, &path, &choices, &roots);
        }
        if let Some(events) = activity {
            fill_activity(&tick_widgets, &events);
        }
        for hits in searches {
            fill_search(&tick_widgets, &hits);
        }
        for message in auths {
            apply_auth(&tick_widgets, &message);
        }
        ControlFlow::Continue
    });

    if background {
        std::thread::spawn(start_daemon);
    } else {
        window.present();
    }
}

fn section_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("heading");
    label
}

fn apply_status(
    widgets: &Widgets,
    status: &StatusSnapshot,
    attention: &[AttentionSnapshot],
    allow: bool,
) {
    let hold = hold_after_sign_in(widgets.hold_login.get(), &status.phase, false);
    widgets.hold_login.set(hold);
    widgets.auth.set_visible(login_visible(&status.phase, hold));
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
    } else if status.phase == "indexing" && status.items_known && status.items_total > 0 {
        let fraction = (status.items_done as f64 / status.items_total as f64).clamp(0.0, 1.0);
        widgets.progress.set_fraction(fraction);
        widgets.progress.set_text(Some(&format!(
            "{} / {} files",
            status.items_done, status.items_total
        )));
    } else if status.phase == "indexing" {
        widgets.progress.pulse();
        widgets.progress.set_text(Some(&format!("{} files", status.items_done)));
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

fn login_visible(phase: &str, hold_after_sign_in: bool) -> bool {
    phase == "needs_auth" && !hold_after_sign_in
}

fn hold_after_sign_in(previous: bool, phase: &str, just_signed_in: bool) -> bool {
    if just_signed_in {
        return true;
    }
    previous && phase == "needs_auth"
}

fn apply_auth(widgets: &Widgets, message: &str) {
    widgets.sign_in.set_sensitive(true);
    widgets.submit_code.set_sensitive(true);
    match message {
        "signed_in" => {
            widgets.password.set_text("");
            widgets.code.set_text("");
            set_auth_step(widgets, AuthStep::SignedIn);
            widgets.auth_status.set_text("Signed in");
            widgets.hold_login.set(true);
            widgets.auth.set_visible(false);
        }
        "needs_code" => {
            widgets.hold_login.set(false);
            widgets.auth.set_visible(true);
            set_auth_step(widgets, AuthStep::Code);
            widgets.auth_status.set_text("Enter the verification code Apple sent.");
            widgets.code.grab_focus();
        }
        "needs_terms" => {
            widgets.hold_login.set(false);
            widgets.auth.set_visible(true);
            set_auth_step(widgets, AuthStep::Terms);
            widgets.auth_status.set_text("Apple updated the terms. Accept them to continue.");
        }
        "needs_device" => {
            widgets.hold_login.set(false);
            widgets.auth.set_visible(true);
            set_auth_step(widgets, AuthStep::Device);
            widgets.auth_status.set_text("Approve this sign-in on a trusted device, then continue.");
        }
        other => {
            widgets.hold_login.set(false);
            widgets.auth.set_visible(true);
            let text = other.strip_prefix("failed:").unwrap_or(other);
            widgets.auth_status.set_text(text);
        }
    }
}

enum AuthStep {
    SignedIn,
    Code,
    Terms,
    Device,
}

fn set_auth_step(widgets: &Widgets, step: AuthStep) {
    let code = matches!(step, AuthStep::Code);
    widgets.code.set_visible(code);
    widgets.submit_code.set_visible(code);
    widgets.sms.set_visible(code);
    widgets.terms.set_visible(matches!(step, AuthStep::Terms));
    widgets.approve.set_visible(matches!(step, AuthStep::Device));
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
                "code" => proxy.submit_code(&code).await,
                "sms" => proxy.request_sms().await,
                "terms" => proxy.accept_terms().await,
                "approve" => proxy.request_device_approval().await,
                _ => proxy.begin_sign_in(&apple_id, &password).await,
            };
            Some(match step {
                Ok(step) => step,
                Err(err) => format!("failed:{err}"),
            })
        });
        let message = result.unwrap_or_else(|| "failed:Could not reach the sync service".into());
        let _ = tx.send(UiMsg::Auth(message));
    });
}

fn confirm_deletion(on_yes: impl Fn() + 'static) {
    confirm(
        "Allow deletion?",
        "This can remove files from iCloud Drive, including from your other devices.",
        "Allow deletion",
        true,
        on_yes,
    );
}

fn confirm(title: &str, body: &str, accept: &str, destructive: bool, on_yes: impl Fn() + 'static) {
    let dialog = adw::MessageDialog::new(None::<&gtk::Window>, Some(title), Some(body));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", accept);
    dialog.set_response_appearance(
        "ok",
        if destructive {
            adw::ResponseAppearance::Destructive
        } else {
            adw::ResponseAppearance::Suggested
        },
    );
    let on_yes = Rc::new(RefCell::new(Some(on_yes)));
    dialog.connect_response(None, move |dialog, response| {
        if response == "ok" {
            if let Some(action) = on_yes.borrow_mut().take() {
                action();
            }
        }
        dialog.close();
    });
    dialog.present();
}

fn set_sync_folder(path: &str, enabled: bool) {
    let path = path.to_string();
    std::thread::spawn(move || {
        let _ = block_on(async move {
            let connection = zbus::Connection::session().await.ok()?;
            let proxy = ServiceProxy::new(&connection).await.ok()?;
            proxy.set_sync_folder(&path, enabled).await.ok()
        });
    });
}

fn start_daemon() {
    if std::env::var_os("FLATPAK_ID").is_none() {
        let started = std::process::Command::new("systemctl")
            .args(["--user", "start", "icloud-drive.service"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if started {
            return;
        }
    }
    let binary = daemon_binary();
    let _ = std::process::Command::new(binary).arg("daemon").spawn();
}

fn daemon_binary() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join("icloud-drive")))
        .filter(|path| path.is_file())
        .or_else(|| {
            std::env::var("HOME").ok().map(|home| std::path::PathBuf::from(home).join(".local/bin/icloud-drive"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("icloud-drive"))
}

fn autostart_dir() -> std::path::PathBuf {
    if std::env::var_os("FLATPAK_ID").is_some() {
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config/autostart")
    } else if let Some(config) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        std::path::PathBuf::from(config).join("autostart")
    } else {
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config/autostart")
    }
}

fn autostart_path() -> std::path::PathBuf {
    autostart_dir().join("dev.icloud_drive.Gui.desktop")
}

fn autostart_command() -> String {
    if let Ok(id) = std::env::var("FLATPAK_ID") {
        format!("flatpak run --command=icloud-drive-gui {id} --background")
    } else {
        let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("icloud-drive-gui"));
        format!("{} --background", desktop_quote(&exe.display().to_string()))
    }
}

fn desktop_quote(arg: &str) -> String {
    if arg.contains([' ', '\t', '"', '\\']) {
        format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

fn autostart_desktop(command: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=iCloud Drive\nComment=Keep chosen iCloud Drive folders in sync\nExec={command}\nIcon=dev.icloud_drive.Gui\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    )
}

fn autostart_is_enabled() -> bool {
    std::fs::read_to_string(autostart_path())
        .ok()
        .is_some_and(|text| !text.lines().any(|line| line.trim() == "Hidden=true"))
}

fn set_autostart(enabled: bool) -> std::io::Result<()> {
    let path = autostart_path();
    if !enabled {
        match std::fs::remove_file(&path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, autostart_desktop(&autostart_command()))
    }
}

fn fill_folders(
    widgets: &Widgets,
    browser: &std::sync::Arc<std::sync::Mutex<String>>,
    path: &str,
    folders: &[icloud_ipc::FolderChoice],
    roots: &[String],
) {
    let signature = format!(
        "{path}|{}|{}",
        folders
            .iter()
            .map(|folder| format!("{}:{}{}", folder.path, folder.synced as u8, folder.included as u8))
            .collect::<Vec<_>>()
            .join(","),
        roots.join(",")
    );
    if widgets.folders_signature.borrow().as_str() == signature {
        return;
    }
    *widgets.folders_signature.borrow_mut() = signature;
    widgets.folder_path.set_text(if path == "/" { "iCloud Drive" } else { path });
    widgets.sync_all.set_visible(!roots.iter().any(|root| root == "/"));

    clear(&widgets.sync_roots);
    if roots.is_empty() {
        widgets.sync_roots.append(&note_row("No folders selected yet."));
    }
    for root in roots {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        pad_row(&row);
        let label = if root == "/" { "Entire iCloud Drive" } else { root.as_str() };
        let text = gtk::Label::new(Some(label));
        text.set_hexpand(true);
        text.set_xalign(0.0);
        row.append(&text);
        let path = root.clone();
        let stop = gtk::Button::with_label("Stop");
        stop.connect_clicked(move |_| {
            let path = path.clone();
            let title = if path == "/" {
                "Stop syncing all of iCloud Drive?".to_string()
            } else {
                format!("Stop syncing {path}?")
            };
            confirm(
                &title,
                "Files you have not edited are removed from the iCloud Drive folder on this computer. They stay on iCloud.",
                "Stop syncing",
                true,
                move || set_sync_folder(&path, false),
            );
        });
        row.append(&stop);
        widgets.sync_roots.append(&row);
    }

    clear(&widgets.folder_list);
    if path != "/" {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        pad_row(&row);
        let text = gtk::Label::new(Some(path));
        text.set_hexpand(true);
        text.set_xalign(0.0);
        row.append(&text);
        if roots.iter().any(|root| root == path) {
            row.append(&gtk::Label::new(Some("Syncing")));
        } else if icloud_ancestor(path, roots) {
            row.append(&gtk::Label::new(Some("Included")));
        } else {
            let here = path.to_string();
            let button = gtk::Button::with_label("Sync this folder");
            button.connect_clicked(move |_| set_sync_folder(&here, true));
            row.append(&button);
        }
        widgets.folder_list.append(&row);
    }
    if folders.is_empty() {
        let waiting = if path == "/" {
            "Top-level folders show up after the first listing."
        } else if roots.iter().any(|root| root == path) || icloud_ancestor(path, roots) {
            "No subfolders."
        } else {
            "Sync this folder to list what is inside it."
        };
        widgets.folder_list.append(&note_row(waiting));
    }
    for folder in folders {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        pad_row(&row);
        let open = gtk::Button::with_label(&folder.name);
        open.set_hexpand(true);
        let next = folder.path.clone();
        let browser = std::sync::Arc::clone(browser);
        open.connect_clicked(move |_| {
            *browser.lock().expect("browser") = next.clone();
        });
        row.append(&open);
        if folder.synced {
            row.append(&gtk::Label::new(Some("Syncing")));
        } else if folder.included {
            row.append(&gtk::Label::new(Some("Included")));
        } else {
            let path = folder.path.clone();
            let button = gtk::Button::with_label("Sync");
            button.connect_clicked(move |_| set_sync_folder(&path, true));
            row.append(&button);
        }
        widgets.folder_list.append(&row);
    }
}

fn icloud_ancestor(path: &str, roots: &[String]) -> bool {
    roots.iter().any(|root| root == "/" || (root != path && path.starts_with(&format!("{root}/"))))
}

fn fill_activity(widgets: &Widgets, events: &[icloud_ipc::ActivityEvent]) {
    let signature = events
        .iter()
        .map(|event| format!("{}:{}:{}:{}", event.at_ms, event.kind, event.path, event.detail))
        .collect::<Vec<_>>()
        .join("\n");
    if widgets.activity_signature.borrow().as_str() == signature {
        return;
    }
    *widgets.activity_signature.borrow_mut() = signature;
    clear(&widgets.activity);
    if events.is_empty() {
        widgets.activity.append(&note_row("No sync activity yet."));
        return;
    }
    for event in events {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
        pad_row(&row);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let title = gtk::Label::new(Some(activity_title(&event.kind)));
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.add_css_class("heading");
        let when = gtk::Label::new(Some(&format_when(event.at_ms)));
        when.add_css_class("dim-label");
        head.append(&title);
        head.append(&when);
        row.append(&head);
        let path = gtk::Label::new(Some(&event.path));
        path.set_xalign(0.0);
        path.set_wrap(true);
        path.set_selectable(true);
        row.append(&path);
        if !event.detail.is_empty() {
            let detail = gtk::Label::new(Some(&event.detail));
            detail.set_xalign(0.0);
            detail.set_wrap(true);
            detail.add_css_class("dim-label");
            row.append(&detail);
        }
        widgets.activity.append(&row);
    }
}

fn activity_title(kind: &str) -> &'static str {
    match kind {
        "downloaded" => "Downloaded",
        "uploaded" => "Uploaded",
        "download_failed" => "Download failed",
        "upload_failed" => "Upload failed",
        "sync_on" => "Syncing folder",
        "sync_off" => "Stopped syncing",
        "conflict" => "Conflict",
        "local" => "Changed here",
        "renamed" => "Renamed",
        "moved" => "Moved",
        _ => "Activity",
    }
}

fn format_when(ms: i64) -> String {
    let Ok(when) = glib::DateTime::from_unix_local(ms / 1000) else {
        return String::new();
    };
    let same_day = glib::DateTime::now_local().ok().is_some_and(|now| {
        now.year() == when.year() && now.day_of_year() == when.day_of_year()
    });
    let fmt = if same_day { "%H:%M" } else { "%d %b %H:%M" };
    when.format(fmt).map(|text| text.to_string()).unwrap_or_default()
}

fn note_row(text: &str) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    pad_row(&row);
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_hexpand(true);
    row.append(&label);
    row
}

fn pad_row(row: &gtk::Box) {
    row.set_margin_top(8);
    row.set_margin_bottom(8);
    row.set_margin_start(12);
    row.set_margin_end(12);
}

type PollUpdate = Option<(StatusSnapshot, Vec<AttentionSnapshot>, bool)>;

enum UiMsg {
    Poll(PollUpdate),
    /// The daemon disappeared without the clean quit signal.
    Crashed,
    Search(Vec<icloud_ipc::SearchHit>),
    Auth(String),
    Folders((String, Vec<icloud_ipc::FolderChoice>, Vec<String>)),
    Activity(Vec<icloud_ipc::ActivityEvent>),
}

const STARTUP_NOTE: &str = "Starts sync at login with this window hidden. The panel icon opens it. If sync stops on its own, the window opens so you can press Start.";

/// A crash is a daemon that was running and did not emit the clean quit signal.
fn unexpected_stop(ever_connected: bool, clean_quit: bool) -> bool {
    ever_connected && !clean_quit
}

fn show_stopped(widgets: &Widgets) {
    widgets.start.set_visible(true);
    widgets.quit.set_visible(false);
    widgets.status.set_text("Sync service is stopped");
    widgets.detail.set_text("Press Start to run it again. ~/iCloud Drive stays where it is.");
    widgets.hold_login.set(false);
    widgets.auth.set_visible(false);
    widgets.progress.set_fraction(0.0);
    widgets.progress.set_text(Some(""));
}

async fn watch_service(tx: std::sync::mpsc::Sender<UiMsg>, browser: std::sync::Arc<std::sync::Mutex<String>>) {
    loop {
        let Some(connection) = zbus::Connection::session().await.ok() else {
            if tx.send(UiMsg::Poll(None)).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
            continue;
        };
        let Ok(proxy) = ServiceProxy::new(&connection).await else {
            if tx.send(UiMsg::Poll(None)).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
            continue;
        };
        let Ok(mut closing) = proxy.receive_closing().await else {
            tokio::time::sleep(Duration::from_millis(700)).await;
            continue;
        };
        let mut ever_connected = false;
        let mut clean_quit = false;
        loop {
            let update = read_status(&proxy).await;
            let connected = update.is_some();
            if connected {
                ever_connected = true;
            }
            if tx.send(UiMsg::Poll(update)).is_err() {
                return;
            }
            if connected {
                let path = browser.lock().expect("browser").clone();
                let folders = proxy.child_folders(&path).await.unwrap_or_default();
                let roots = proxy.sync_folders().await.unwrap_or_default();
                if tx.send(UiMsg::Folders((path, folders, roots))).is_err() {
                    return;
                }
                if let Ok(events) = proxy.activity().await {
                    if tx.send(UiMsg::Activity(events)).is_err() {
                        return;
                    }
                }
            }
            let closed = tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(700)) => false,
                _ = closing.next() => true,
            };
            if closed {
                clean_quit = true;
                let _ = tx.send(UiMsg::Poll(None));
                break;
            }
            if !connected {
                break;
            }
        }
        if unexpected_stop(ever_connected, clean_quit) {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if !service_is_up().await && tx.send(UiMsg::Crashed).is_err() {
                return;
            }
        }
    }
}

async fn service_is_up() -> bool {
    let Ok(connection) = zbus::Connection::session().await else {
        return false;
    };
    let Ok(proxy) = ServiceProxy::new(&connection).await else {
        return false;
    };
    proxy.status().await.is_ok()
}

async fn read_status(proxy: &ServiceProxy<'_>) -> PollUpdate {
    let status = proxy.status().await.ok()?;
    let attention = proxy.attention().await.ok()?;
    let allow = proxy.allow_deletion().await.unwrap_or(false);
    Some((status, attention, allow))
}

#[cfg(test)]
mod tests {
    use super::{autostart_desktop, desktop_quote, hold_after_sign_in, login_visible, unexpected_stop};

    #[test]
    fn login_stays_hidden_while_a_sync_is_running() {
        for phase in ["idle", "indexing", "downloading", "uploading", "error"] {
            assert!(!login_visible(phase, false), "{phase}");
        }
        assert!(login_visible("needs_auth", false));
    }

    #[test]
    fn a_finished_sign_in_stays_hidden_until_the_session_fails_again() {
        let hold = hold_after_sign_in(false, "needs_auth", true);
        assert!(!login_visible("needs_auth", hold));
        let hold = hold_after_sign_in(hold, "downloading", false);
        assert!(!hold);
        assert!(!login_visible("downloading", hold));
        let hold = hold_after_sign_in(hold, "needs_auth", false);
        assert!(login_visible("needs_auth", hold));
    }

    #[test]
    fn login_autostart_hides_the_window_and_starts_sync() {
        let text = autostart_desktop("flatpak run --command=icloud-drive-gui dev.icloud_drive.Gui --background");
        assert!(text.contains("Exec=flatpak run --command=icloud-drive-gui dev.icloud_drive.Gui --background"));
        assert!(text.contains("X-GNOME-Autostart-enabled=true"));
        assert_eq!(desktop_quote("/home/pika/.local/bin/icloud-drive-gui"), "/home/pika/.local/bin/icloud-drive-gui");
        assert_eq!(desktop_quote("/home/a b/icloud-drive-gui"), "\"/home/a b/icloud-drive-gui\"");
    }

    #[test]
    fn a_clean_quit_does_not_alert() {
        assert!(!unexpected_stop(false, true));
        assert!(!unexpected_stop(true, true));
        assert!(!unexpected_stop(false, false));
        assert!(unexpected_stop(true, false));
    }
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

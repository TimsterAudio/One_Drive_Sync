//! The trial sync agent: sign in, add folder pairs, sync. One window with a
//! status and a log; the syncing itself runs on the library's worker thread.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use eframe::egui;
use one_drive_sync::engine::Level;
use one_drive_sync::{
    Auth, DriveScope, Event, GraphDrive, KeyringStore, Mode, PairConfig, PairStatus, SyncOptions,
    SyncService,
};
use serde::{Deserialize, Serialize};
use time::UtcOffset;

const APP: &str = "OneDrive Sync Agent";
const LOG_LINES: usize = 500;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    /// The Azure app registration's client id.
    client_id: String,
    scope: DriveScope,
    pairs: Vec<PairConfig>,
}

fn data_dir() -> PathBuf {
    if cfg!(windows) {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_default();
        base.join(APP)
    } else {
        let base = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_default();
        base.join("onedrive-sync-agent")
    }
}

impl Settings {
    fn path() -> PathBuf {
        data_dir().join("settings.json")
    }

    fn load() -> Self {
        let mut s: Settings = std::fs::read(Self::path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if let Ok(id) = std::env::var("ODSYNC_CLIENT_ID") {
            s.client_id = id;
        }
        s
    }

    fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(data_dir())?;
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(Self::path(), bytes)
    }
}

struct LogEntry {
    at: String,
    level: Level,
    text: String,
}

/// What a background sign-in or account lookup sends back.
enum AuthNews {
    Account(String),
    Failed(String),
}

struct AgentApp {
    settings: Settings,
    offset: UtcOffset,
    ctx: egui::Context,
    auth: Option<Arc<Auth>>,
    service: Option<SyncService>,
    statuses: Vec<PairStatus>,
    account: Option<String>,
    auth_news: Option<Receiver<AuthNews>>,
    cancel_sign_in: Arc<AtomicBool>,
    log: VecDeque<LogEntry>,
    // The settings and add-pair forms.
    client_id_edit: String,
    scope_edit: DriveScope,
    new_local: String,
    new_remote: String,
    new_mode: Mode,
    new_poll: u64,
}

fn clock(offset: UtcOffset, t: time::OffsetDateTime) -> String {
    t.to_offset(offset)
        .format(time::macros::format_description!(
            "[hour]:[minute]:[second]"
        ))
        .unwrap_or_default()
}

impl AgentApp {
    fn new(ctx: egui::Context, offset: UtcOffset) -> Self {
        let settings = Settings::load();
        let mut app = Self {
            client_id_edit: settings.client_id.clone(),
            scope_edit: settings.scope,
            settings,
            offset,
            ctx,
            auth: None,
            service: None,
            statuses: Vec::new(),
            account: None,
            auth_news: None,
            cancel_sign_in: Arc::new(AtomicBool::new(false)),
            log: VecDeque::new(),
            new_local: String::new(),
            new_remote: "trips".into(),
            new_mode: Mode::TwoWay,
            new_poll: 20,
        };
        app.restart();
        app
    }

    fn note(&mut self, level: Level, text: impl Into<String>) {
        self.log.push_back(LogEntry {
            at: clock(self.offset, time::OffsetDateTime::now_utc()),
            level,
            text: text.into(),
        });
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save() {
            self.note(Level::Error, format!("Couldn't save settings: {e}"));
        }
    }

    /// (Re)starts the sign-in and the worker for the current settings.
    fn restart(&mut self) {
        self.service = None;
        self.auth = None;
        self.account = None;
        self.statuses = vec![PairStatus::Waiting; self.settings.pairs.len()];
        if self.settings.client_id.trim().is_empty() {
            self.note(
                Level::Warn,
                "Enter the Azure app's client id under Settings.",
            );
            return;
        }
        let store = KeyringStore::new(APP, self.settings.client_id.trim());
        let auth = Arc::new(Auth::new(
            &self.settings.client_id,
            self.settings.scope,
            Box::new(store),
        ));
        let drive = GraphDrive::new(auth.clone(), self.settings.scope);
        let ctx = self.ctx.clone();
        let service = SyncService::start(
            Box::new(drive),
            data_dir().join("state"),
            SyncOptions::default(),
            Box::new(move || ctx.request_repaint()),
        );
        service.set_pairs(self.settings.pairs.clone());
        if auth.is_signed_in() {
            self.lookup_account(auth.clone());
        } else {
            self.note(Level::Info, "Not signed in.");
        }
        self.auth = Some(auth);
        self.service = Some(service);
    }

    /// Runs `job` on a thread and picks up its news in `poll`.
    fn in_background(&mut self, job: impl FnOnce() -> AuthNews + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        });
        self.auth_news = Some(rx);
    }

    fn lookup_account(&mut self, auth: Arc<Auth>) {
        let scope = self.settings.scope;
        self.in_background(move || account_news(auth, scope));
    }

    fn sign_in(&mut self) {
        let Some(auth) = self.auth.clone() else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_sign_in = cancel.clone();
        let scope = self.settings.scope;
        self.in_background(move || {
            let mut open = |url: &str| open::that(url);
            match auth.sign_in(&mut open, &cancel) {
                Ok(()) => account_news(auth, scope),
                Err(e) => AuthNews::Failed(e.to_string()),
            }
        });
        self.note(Level::Info, "Signing in: finish in the browser.");
    }

    fn poll(&mut self) {
        if let Some(news) = self.auth_news.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.auth_news = None;
            match news {
                AuthNews::Account(name) => {
                    self.note(Level::Info, format!("Signed in as {name}."));
                    self.account = Some(name);
                    if let Some(s) = &self.service {
                        s.sync_now();
                    }
                }
                AuthNews::Failed(e) => self.note(Level::Error, e),
            }
        }
        let events = self
            .service
            .as_ref()
            .map(|s| s.events())
            .unwrap_or_default();
        for e in events {
            match e {
                Event::Log {
                    pair,
                    level,
                    message,
                    ..
                } => {
                    let label = pair
                        .and_then(|i| self.settings.pairs.get(i))
                        .map(|p| format!("{}: ", p.remote))
                        .unwrap_or_default();
                    self.note(level, format!("{label}{message}"));
                }
                Event::Status { pair, status } => {
                    if let Some(s) = self.statuses.get_mut(pair) {
                        *s = status;
                    }
                }
            }
        }
    }

    fn add_pair(&mut self) {
        let local = PathBuf::from(self.new_local.trim());
        if !local.is_dir() {
            self.note(
                Level::Warn,
                format!("\"{}\" isn't a folder.", local.display()),
            );
            return;
        }
        let remote = self.new_remote.trim().trim_matches('/').to_string();
        if remote.is_empty() {
            self.note(Level::Warn, "Name the OneDrive folder for the pair.");
            return;
        }
        let mut pair = PairConfig::new(local, &remote, self.new_mode);
        pair.poll_secs = self.new_poll.max(5);
        self.note(
            Level::Info,
            format!(
                "Added {} {} {remote}.",
                pair.local.display(),
                arrow(pair.mode)
            ),
        );
        self.settings.pairs.push(pair);
        self.pairs_changed();
        self.new_local.clear();
    }

    fn pairs_changed(&mut self) {
        self.save_settings();
        self.statuses = vec![PairStatus::Waiting; self.settings.pairs.len()];
        if let Some(s) = &self.service {
            s.set_pairs(self.settings.pairs.clone());
        }
    }

    fn account_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let signed_in = self.auth.as_ref().is_some_and(|a| a.is_signed_in());
            let busy = self.auth_news.is_some();
            let text = match (&self.account, signed_in) {
                _ if busy && !signed_in => "Signing in…".to_string(),
                (Some(name), true) => format!("Signed in as {name}"),
                (None, true) => "Signed in".to_string(),
                _ => "Not signed in".to_string(),
            };
            ui.strong(text);
            ui.separator();
            if signed_in {
                if ui.button("Sign out").clicked() {
                    if let Some(a) = &self.auth {
                        a.sign_out();
                    }
                    self.account = None;
                    self.note(Level::Info, "Signed out.");
                }
            } else if busy {
                if ui.button("Cancel").clicked() {
                    self.cancel_sign_in.store(true, Ordering::Relaxed);
                }
            } else if ui
                .add_enabled(self.auth.is_some(), egui::Button::new("Sign in"))
                .clicked()
            {
                self.sign_in();
            }
            if ui
                .add_enabled(self.service.is_some(), egui::Button::new("Sync now"))
                .clicked()
            {
                if let Some(s) = &self.service {
                    s.sync_now();
                }
            }
        });
    }

    fn settings_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Settings")
            .default_open(self.settings.client_id.is_empty())
            .show(ui, |ui| {
                egui::Grid::new("settings").num_columns(2).show(ui, |ui| {
                    ui.label("Azure app (client) id");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.client_id_edit)
                            .desired_width(320.0)
                            .hint_text("00000000-0000-0000-0000-000000000000"),
                    );
                    ui.end_row();
                    ui.label("Access");
                    ui.horizontal(|ui| {
                        ui.radio_value(
                            &mut self.scope_edit,
                            DriveScope::AppFolder,
                            "App folder only (Apps/<app>/)",
                        );
                        ui.radio_value(&mut self.scope_edit, DriveScope::FullDrive, "Whole drive");
                    });
                    ui.end_row();
                });
                let changed = self.client_id_edit.trim() != self.settings.client_id
                    || self.scope_edit != self.settings.scope;
                if ui
                    .add_enabled(changed, egui::Button::new("Apply"))
                    .clicked()
                {
                    self.settings.client_id = self.client_id_edit.trim().to_string();
                    self.settings.scope = self.scope_edit;
                    self.save_settings();
                    self.note(Level::Info, "Settings applied.");
                    self.restart();
                }
            });
    }

    fn pairs_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Folder pairs");
        if self.settings.pairs.is_empty() {
            ui.weak("None yet; add one below.");
        }
        let mut remove = None;
        for (i, p) in self.settings.pairs.iter().enumerate() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.strong(format!("OneDrive: {}", p.remote));
                    ui.label(match p.mode {
                        Mode::Push => "(push)",
                        Mode::TwoWay => "(two-way)",
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Remove").clicked() {
                            remove = Some(i);
                        }
                    });
                });
                ui.add(
                    egui::Label::new(egui::RichText::new(p.local.display().to_string()).weak())
                        .truncate(),
                );
                let (text, colour) = status_text(self.statuses.get(i), self.offset);
                ui.colored_label(colour, text);
            });
        }
        if let Some(i) = remove {
            let p = self.settings.pairs.remove(i);
            self.note(
                Level::Info,
                format!(
                    "Removed {} {} {}.",
                    p.local.display(),
                    arrow(p.mode),
                    p.remote
                ),
            );
            self.pairs_changed();
        }
    }

    fn add_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Add folder pair");
        egui::Grid::new("add").num_columns(2).show(ui, |ui| {
            ui.label("Local folder");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.new_local).desired_width(320.0));
                if ui.button("Browse…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.new_local = dir.display().to_string();
                    }
                }
            });
            ui.end_row();
            ui.label("OneDrive folder");
            ui.add(
                egui::TextEdit::singleline(&mut self.new_remote)
                    .desired_width(320.0)
                    .hint_text("e.g. weather or Waypoint/trips"),
            );
            ui.end_row();
            ui.label("Mode");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.new_mode, Mode::Push, "Push (local to OneDrive)");
                ui.radio_value(&mut self.new_mode, Mode::TwoWay, "Two-way");
            });
            ui.end_row();
            ui.label("Check OneDrive every");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut self.new_poll).range(5..=3600));
                ui.label("seconds");
            });
            ui.end_row();
        });
        if ui.button("Add pair").clicked() {
            self.add_pair();
        }
    }

    fn log_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Log");
            if ui.small_button("Clear").clicked() {
                self.log.clear();
            }
        });
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for e in &self.log {
                    let colour = level_colour(e.level, ui);
                    ui.horizontal_wrapped(|ui| {
                        ui.weak(&e.at);
                        ui.colored_label(colour, &e.text);
                    });
                }
            });
    }
}

fn account_news(auth: Arc<Auth>, scope: DriveScope) -> AuthNews {
    match GraphDrive::new(auth, scope).account_name() {
        Ok(name) => AuthNews::Account(name),
        Err(e) => AuthNews::Failed(e.to_string()),
    }
}

/// How a pair's mode reads between its folders.
fn arrow(mode: Mode) -> &'static str {
    match mode {
        Mode::Push => "pushed to",
        Mode::TwoWay => "two-way with",
    }
}

fn level_colour(level: Level, ui: &egui::Ui) -> egui::Color32 {
    match level {
        Level::Info => ui.visuals().text_color(),
        Level::Warn => ui.visuals().warn_fg_color,
        Level::Error => ui.visuals().error_fg_color,
    }
}

fn status_text(status: Option<&PairStatus>, offset: UtcOffset) -> (String, egui::Color32) {
    let amber = egui::Color32::from_rgb(200, 140, 40);
    match status {
        None | Some(PairStatus::Waiting) => ("waiting".into(), egui::Color32::GRAY),
        Some(PairStatus::Syncing) => ("syncing…".into(), egui::Color32::from_rgb(70, 130, 220)),
        Some(PairStatus::Done { report, at_ms }) => {
            let at = time::OffsetDateTime::from_unix_timestamp(at_ms / 1000)
                .map(|t| clock(offset, t))
                .unwrap_or_default();
            let colour = if report.failed > 0 {
                amber
            } else {
                egui::Color32::from_rgb(60, 160, 90)
            };
            (format!("{report} · {at}"), colour)
        }
        Some(PairStatus::Failed(m)) => (m.clone(), egui::Color32::from_rgb(210, 70, 60)),
        Some(PairStatus::SignInNeeded) => ("sign in needed".into(), amber),
    }
}

impl eframe::App for AgentApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        egui::Panel::top("account").show(ui, |ui| {
            ui.add_space(6.0);
            self.account_bar(ui);
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.settings_section(ui);
            ui.separator();
            self.pairs_section(ui);
            ui.separator();
            self.add_section(ui);
            ui.separator();
            self.log_section(ui);
        });
    }
}

fn main() -> eframe::Result {
    // Read before any thread starts; on Linux it can't be read safely after.
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP)
            .with_inner_size([820.0, 640.0])
            .with_min_inner_size([560.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP,
        options,
        Box::new(move |cc| Ok(Box::new(AgentApp::new(cc.egui_ctx.clone(), offset)))),
    )
}

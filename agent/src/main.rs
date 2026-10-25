//! Folder Sync: keeps folders on this computer in step with folders on
//! another (a Windows share mounted here), like SyncToy. One window with the
//! pairs, their status and a log; the syncing runs on the library's worker
//! thread.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use eframe::egui;
use one_drive_sync::engine::Level;
use one_drive_sync::{Event, Mode, PairConfig, PairStatus, SyncOptions, SyncService};
use serde::{Deserialize, Serialize};
use time::UtcOffset;

const APP: &str = "Folder Sync";
const LOG_LINES: usize = 500;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
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
        base.join("folder-sync")
    }
}

impl Settings {
    fn path() -> PathBuf {
        data_dir().join("settings.json")
    }

    fn load() -> Self {
        std::fs::read(Self::path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
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

struct App {
    settings: Settings,
    offset: UtcOffset,
    service: SyncService,
    statuses: Vec<PairStatus>,
    log: VecDeque<LogEntry>,
    /// Where the login entry goes; `None` where autostart isn't offered.
    autostart_dir: Option<PathBuf>,
    autostart_on: bool,
    // The add-pair form.
    new_local: String,
    new_share: String,
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

/// The last part of a path, to name a pair in the log.
fn short(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

impl App {
    fn new(ctx: egui::Context, offset: UtcOffset, minimized: bool) -> Self {
        if minimized {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        let settings = Settings::load();
        let service = SyncService::start_folders(
            data_dir().join("state"),
            SyncOptions::default(),
            Box::new(move || ctx.request_repaint()),
        );
        service.set_pairs(settings.pairs.clone());
        let mut app = Self {
            statuses: vec![PairStatus::Waiting; settings.pairs.len()],
            settings,
            offset,
            service,
            log: VecDeque::new(),
            autostart_on: autostart::dir().is_some_and(|d| autostart::is_on(&d)),
            autostart_dir: autostart::dir(),
            new_local: String::new(),
            new_share: String::new(),
            new_mode: Mode::TwoWay,
            new_poll: 20,
        };
        if let (true, Some(dir), Ok(exe)) = (
            app.autostart_on,
            app.autostart_dir.clone(),
            std::env::current_exe(),
        ) {
            if let Err(e) = autostart::refresh(&dir, &exe) {
                app.note(Level::Warn, format!("Couldn't update the login entry: {e}"));
            }
        }
        if app.settings.pairs.is_empty() {
            app.note(Level::Info, "Add a folder pair below to start.");
        }
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

    fn poll(&mut self) {
        for e in self.service.events() {
            match e {
                Event::Log {
                    pair,
                    level,
                    message,
                    ..
                } => {
                    let label = pair
                        .and_then(|i| self.settings.pairs.get(i))
                        .map(|p| format!("{}: ", short(&p.local)))
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
        let share = PathBuf::from(self.new_share.trim());
        if !local.is_dir() {
            self.note(
                Level::Warn,
                format!("\"{}\" isn't a folder.", local.display()),
            );
            return;
        }
        if !share.is_dir() {
            self.note(
                Level::Warn,
                format!(
                    "\"{}\" isn't a folder (is the share mounted?).",
                    share.display()
                ),
            );
            return;
        }
        if local.starts_with(&share) || share.starts_with(&local) {
            self.note(Level::Warn, "The two folders can't be inside each other.");
            return;
        }
        let mut pair = PairConfig::folders(local, share, self.new_mode);
        pair.poll_secs = self.new_poll.max(5);
        self.note(
            Level::Info,
            format!(
                "Added {} {} {}.",
                pair.local.display(),
                arrow(pair.mode),
                pair.remote_name()
            ),
        );
        self.settings.pairs.push(pair);
        self.pairs_changed();
        self.new_local.clear();
        self.new_share.clear();
    }

    fn pairs_changed(&mut self) {
        if let Err(e) = self.settings.save() {
            self.note(Level::Error, format!("Couldn't save the pairs: {e}"));
        }
        self.statuses = vec![PairStatus::Waiting; self.settings.pairs.len()];
        self.service.set_pairs(self.settings.pairs.clone());
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong(APP);
            ui.separator();
            let n = self.settings.pairs.len();
            ui.label(match n {
                1 => "1 pair".to_string(),
                n => format!("{n} pairs"),
            });
            ui.separator();
            if ui
                .add_enabled(n > 0, egui::Button::new("Sync now"))
                .clicked()
            {
                self.service.sync_now();
            }
            if self.autostart_dir.is_some() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut on = self.autostart_on;
                    let check = ui
                        .checkbox(&mut on, "Start when I log in")
                        .on_hover_text("Opens Folder Sync, minimised, each time you log in.");
                    if check.changed() {
                        self.set_autostart(on);
                    }
                });
            }
        });
    }

    fn set_autostart(&mut self, on: bool) {
        let Some(dir) = self.autostart_dir.clone() else {
            return;
        };
        let result =
            std::env::current_exe().and_then(|exe| autostart::set(&dir, on, &exe).map(|_| exe));
        match result {
            Ok(exe) => {
                self.autostart_on = on;
                let text = if on {
                    format!(
                        "Folder Sync will start when you log in ({}).",
                        exe.display()
                    )
                } else {
                    "Folder Sync won't start when you log in.".to_string()
                };
                self.note(Level::Info, text);
            }
            Err(e) => self.note(
                Level::Error,
                format!("Couldn't change the login entry: {e}"),
            ),
        }
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
                    ui.strong(short(&p.local));
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
                let path = format!(
                    "{}  {}  {}",
                    p.local.display(),
                    arrow(p.mode),
                    p.remote_name()
                );
                ui.add(egui::Label::new(egui::RichText::new(path).weak()).truncate());
                let (text, colour) = status_text(self.statuses.get(i), self.offset);
                ui.colored_label(colour, text);
            });
        }
        if let Some(i) = remove {
            let p = self.settings.pairs.remove(i);
            self.note(
                Level::Info,
                format!("Removed {}; no files were touched.", p.local.display()),
            );
            self.pairs_changed();
        }
    }

    fn add_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Add folder pair");
        egui::Grid::new("add").num_columns(2).show(ui, |ui| {
            ui.label("This computer");
            folder_field(ui, &mut self.new_local, "e.g. ~/Waypoint/trips");
            ui.end_row();
            ui.label("Other folder");
            folder_field(
                ui,
                &mut self.new_share,
                "the mounted share, e.g. /mnt/pc/trips",
            );
            ui.end_row();
            ui.label("Mode");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.new_mode, Mode::TwoWay, "Two-way");
                ui.radio_value(
                    &mut self.new_mode,
                    Mode::Push,
                    "Push (this computer to the other folder)",
                );
            });
            ui.end_row();
            ui.label("Check the other folder every");
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

fn folder_field(ui: &mut egui::Ui, value: &mut String, hint: &str) {
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(value)
                .desired_width(340.0)
                .hint_text(hint),
        );
        if ui.button("Browse…").clicked() {
            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                *value = dir.display().to_string();
            }
        }
    });
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

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        egui::Panel::top("bar").show(ui, |ui| {
            ui.add_space(6.0);
            self.top_bar(ui);
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
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
    let minimized = std::env::args().any(|a| a == autostart::MINIMIZED_ARG);
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
        Box::new(move |cc| Ok(Box::new(App::new(cc.egui_ctx.clone(), offset, minimized)))),
    )
}

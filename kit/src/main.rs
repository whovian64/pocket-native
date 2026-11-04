#![cfg_attr(windows, windows_subsystem = "windows")]
//! Pocket Native's kit in a window: games dropped on it or picked, found in
//! folders, built into a folder of your choice with their progress shown,
//! every game rebuilt, and the kit updated.

mod games;
mod jobs;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

use eframe::egui;
use games::Game;
use jobs::{Job, Kind};

fn main() -> eframe::Result {
    let kit = jobs::kit_root();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("Pocket Native").with_inner_size([900.0, 640.0]).with_min_inner_size([640.0, 420.0]).with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("Pocket Native", options, Box::new(move |cc| Ok(Box::new(App::new(cc, kit)))))
}

/// what the kit remembers between runs, beside it in kit.txt.
struct Saved {
    out: PathBuf,
    keys: Option<PathBuf>,
}

impl Saved {
    fn load(kit: &Path) -> Saved {
        let text = std::fs::read_to_string(kit.join("kit.txt")).unwrap_or_default();
        let value = |key: &str| text.lines().find_map(|line| line.strip_prefix(key)).map(PathBuf::from);
        Saved { out: value("out=").unwrap_or_else(|| kit.join("build")), keys: value("keys=").or_else(|| Some(kit.join("aes_keys.txt")).filter(|keys| keys.is_file())) }
    }

    fn store(&self, kit: &Path) {
        let mut text = format!("out={}\n", self.out.display());
        if let Some(keys) = &self.keys {
            text += &format!("keys={}\n", keys.display());
        }
        let _ = std::fs::write(kit.join("kit.txt"), text);
    }
}

struct App {
    kit: PathBuf,
    saved: Saved,
    list: Vec<(String, String)>,
    games: Vec<Game>,
    icons: Vec<Option<egui::TextureHandle>>,
    /// games still being read, off the window's thread.
    detecting: Option<Receiver<Game>>,
    job: Option<Job>,
    show_log: bool,
    engine: Option<&'static str>,
    newer: Option<Receiver<Option<String>>>,
    update: Option<String>,
    message: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext, kit: PathBuf) -> App {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let (send, newer) = channel();
        let checking = kit.clone();
        std::thread::spawn(move || drop(send.send(jobs::newer_version(&checking))));
        let mut app = App {
            saved: Saved::load(&kit),
            list: games::compatibility(&kit),
            kit,
            games: Vec::new(),
            icons: Vec::new(),
            detecting: None,
            job: None,
            show_log: false,
            engine: jobs::engine(),
            newer: Some(newer),
            update: None,
            message: None,
        };
        // games dropped on the program itself come as its arguments
        let given: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
        if !given.is_empty() {
            app.add(given);
        }
        app
    }

    /// reads the files given, each once, on a thread of its own.
    fn add(&mut self, paths: Vec<PathBuf>) {
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .flat_map(|path| if path.is_dir() { games::scan(&path) } else { vec![path] })
            .filter(|path| games::is_game(path) && !self.games.iter().any(|game| &game.path == path))
            .collect();
        if paths.is_empty() {
            self.message = Some("No 3DS games there: the build takes .3ds, .cci, .cia, .zcci and .zcia files.".into());
            return;
        }
        self.message = None;
        let (send, receive) = channel();
        let (kit, list) = (self.kit.clone(), self.list.clone());
        std::thread::spawn(move || {
            for path in paths {
                if send.send(games::detect(&path, &kit, &list)).is_err() {
                    return;
                }
            }
        });
        self.detecting = Some(receive);
    }

    fn start(&mut self, kind: Kind) {
        let roms: Vec<PathBuf> = self.games.iter().filter(|game| game.selected).map(|game| game.path.clone()).collect();
        match Job::start(&self.kit, kind, &self.saved.out, self.saved.keys.as_deref(), &roms) {
            Ok(job) => {
                self.job = Some(job);
                self.message = None;
            }
            Err(error) => self.message = Some(format!("Could not start the kit's script: {error}")),
        }
    }

    fn running(&self) -> bool {
        self.job.as_ref().is_some_and(|job| job.finished.is_none())
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let dropped: Vec<PathBuf> = ctx.input(|input| input.raw.dropped_files.iter().map(|file| file.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            self.add(dropped);
        }
        if let Some(detecting) = &self.detecting {
            while let Ok(game) = detecting.try_recv() {
                let icon = game.icon.as_ref().map(|pixels| ctx.load_texture(game.path.display().to_string(), egui::ColorImage::from_rgba_unmultiplied([48, 48], pixels), egui::TextureOptions::LINEAR));
                self.icons.push(icon);
                self.games.push(game);
            }
        }
        if let Some(newer) = &self.newer {
            if let Ok(found) = newer.try_recv() {
                self.update = found;
                self.newer = None;
            }
        }
        if let Some(job) = &mut self.job {
            job.poll();
            if job.finished.is_some() && job.kind == Kind::Update {
                self.update = None;
            }
        }
        if self.running() || self.detecting.is_some() || self.newer.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("Pocket Native");
                ui.label(egui::RichText::new(env!("CARGO_PKG_VERSION")).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match &self.update {
                        Some(version) => {
                            if ui.add_enabled(!self.running(), egui::Button::new(format!("Update to {version}"))).on_hover_text("Downloads the new version into this kit and builds every game in the build folder again").clicked() {
                                self.start(Kind::Update);
                            }
                        }
                        None if self.newer.is_some() => {
                            ui.label(egui::RichText::new("checking for updates").weak());
                        }
                        None => {
                            ui.label(egui::RichText::new("up to date").weak());
                        }
                    }
                    match self.engine {
                        Some(engine) => ui.label(egui::RichText::new(format!("builds with {engine}")).weak()),
                        None => ui.colored_label(egui::Color32::from_rgb(230, 150, 60), "No Docker or Podman found: install Docker Desktop to build"),
                    };
                });
            });
            ui.add_space(6.0);
        });

        egui::Panel::bottom("bottom").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Build into");
                ui.monospace(self.saved.out.display().to_string());
                if ui.add_enabled(!self.running(), egui::Button::new("Change…")).clicked() {
                    if let Some(folder) = rfd::FileDialog::new().set_directory(&self.saved.out).pick_folder() {
                        self.saved.out = folder;
                        self.saved.store(&self.kit);
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Console keys");
                match &self.saved.keys {
                    Some(keys) => ui.monospace(keys.display().to_string()),
                    None => ui.label(egui::RichText::new("none, needed only for encrypted games and .cia files").weak()),
                };
                if ui.add_enabled(!self.running(), egui::Button::new("Choose…")).clicked() {
                    if let Some(file) = rfd::FileDialog::new().add_filter("aes_keys.txt", &["txt"]).pick_file() {
                        self.saved.keys = Some(file);
                        self.saved.store(&self.kit);
                    }
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let selected = self.games.iter().filter(|game| game.selected).count();
                let can = !self.running() && self.engine.is_some();
                let build = egui::Button::new(egui::RichText::new(format!("Build {selected} game{}", if selected == 1 { "" } else { "s" })).size(18.0)).min_size(egui::vec2(160.0, 36.0));
                if ui.add_enabled(can && selected > 0, build).clicked() {
                    self.start(Kind::Build);
                }
                if ui.add_enabled(can, egui::Button::new("Rebuild all games").min_size(egui::vec2(0.0, 36.0))).on_hover_text("Builds every game in the build folder again, with the hints your handheld wrote down").clicked() {
                    self.start(Kind::Rebuild);
                }
                if self.running() && ui.button("Cancel").clicked() {
                    if let Some(job) = &mut self.job {
                        job.cancel();
                    }
                }
                if self.job.as_ref().is_some_and(|job| job.finished == Some(true) && job.kind != Kind::Update) && ui.button("Open the build folder").clicked() {
                    open(&self.saved.out);
                }
            });
            if let Some(message) = &self.message {
                ui.colored_label(egui::Color32::from_rgb(230, 150, 60), message);
            }
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(job) = &self.job {
                progress(ui, job, &mut self.show_log);
                ui.separator();
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.running(), egui::Button::new("Add games…")).clicked() {
                    if let Some(files) = rfd::FileDialog::new().add_filter("3DS games", &games::EXTENSIONS).pick_files() {
                        self.add(files);
                    }
                }
                if ui.add_enabled(!self.running(), egui::Button::new("Find games in a folder…")).clicked() {
                    if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                        self.add(vec![folder]);
                    }
                }
                if !self.games.is_empty() && ui.add_enabled(!self.running(), egui::Button::new("Clear")).clicked() {
                    self.games.clear();
                    self.icons.clear();
                }
            });
            ui.add_space(4.0);
            if self.games.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("Drop your 3DS games here\n.3ds, .cci, .cia, .zcci or .zcia").size(18.0).weak());
                });
                return;
            }
            let running = self.running();
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                egui::Grid::new("games").num_columns(4).spacing([12.0, 6.0]).striped(true).show(ui, |ui| {
                    for (game, icon) in self.games.iter_mut().zip(&self.icons) {
                        ui.add_enabled(!running, egui::Checkbox::without_text(&mut game.selected));
                        match icon {
                            Some(icon) => ui.image((icon.id(), egui::vec2(32.0, 32.0))),
                            None => ui.allocate_response(egui::vec2(32.0, 32.0), egui::Sense::hover()),
                        };
                        ui.vertical(|ui| {
                            ui.set_min_width((ui.available_width() - 140.0).max(320.0));
                            ui.strong(&game.name).on_hover_text(game.path.display().to_string());
                            let mut detail = game.id.map_or_else(|| "title id unknown".to_owned(), |id| format!("{id:016X}"));
                            detail += &format!(" · {}", games::size_text(game.size));
                            if game.encrypted {
                                detail += " · encrypted, needs your console's keys";
                            }
                            ui.label(egui::RichText::new(detail).weak().small());
                        });
                        match &game.status {
                            Some(status) => ui.colored_label(status_color(status), status).on_hover_text("From the compatibility list"),
                            None => ui.label(egui::RichText::new("not tried yet").weak()),
                        };
                        ui.end_row();
                    }
                });
            });
        });
    }
}

/// the job's overall bar, a bar a game, and its log behind a button.
fn progress(ui: &mut egui::Ui, job: &Job, show_log: &mut bool) {
    let what = match job.kind {
        Kind::Build => "Building",
        Kind::Rebuild => "Rebuilding every game",
        Kind::Update => "Updating Pocket Native",
    };
    let state = match job.finished {
        None => what.to_owned(),
        Some(true) => match job.kind {
            Kind::Update => "Updated. Copy the Pocket Native folder onto your card again.".into(),
            _ => "Done. Copy the Pocket Native folder onto your handheld's card.".into(),
        },
        Some(false) => "Stopped with an error, the log below says where.".into(),
    };
    ui.strong(state);
    ui.add(egui::ProgressBar::new(job.fraction()).show_percentage().animate(job.finished.is_none()));
    for game in &job.games {
        ui.horizontal(|ui| {
            ui.add_sized([280.0, 18.0], egui::Label::new(if game.name.is_empty() { &game.id } else { &game.name }).truncate());
            ui.add(egui::ProgressBar::new(jobs::stage_fraction(&game.stage)).desired_width(220.0).text(jobs::stage_text(&game.stage)));
        });
    }
    ui.toggle_value(show_log, "Show the log");
    if *show_log {
        egui::ScrollArea::vertical().max_height(180.0).stick_to_bottom(true).show(ui, |ui| {
            for line in &job.log {
                ui.monospace(line);
            }
        });
    }
}

fn status_color(status: &str) -> egui::Color32 {
    match status {
        "Plays well" => egui::Color32::from_rgb(110, 200, 110),
        "Playable" => egui::Color32::from_rgb(170, 210, 90),
        "Slow" | "Starts" => egui::Color32::from_rgb(230, 190, 70),
        _ => egui::Color32::from_rgb(220, 90, 80),
    }
}

/// shows a folder in the system's file manager.
fn open(folder: &Path) {
    let _ = std::fs::create_dir_all(folder);
    let opener = if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(folder).spawn();
}

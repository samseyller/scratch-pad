use std::fs;
use std::path::{Path, PathBuf};

use eframe::egui;

/// Minimal Notepad-like app (single-file editor) using eframe/egui + rfd dialogs.
///
/// Goals:
/// - Keep the code small and readable
/// - Keep state simple (one buffer, one optional path, one dirty flag)
/// - Use native file dialogs for Open/Save As
///
/// Notes:
/// - egui already supports common text shortcuts (Ctrl+C/V/X/A) in the text editor.
/// - Some eframe/egui versions differ in API; this code avoids newer methods like
///   TextEdit::wrap(bool) and Frame::close().
fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        // Keep it simple, but give a comfortable window size.
        viewport: egui::ViewportBuilder::default()
            .with_title("Scratchpad")
            .with_inner_size([900.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Scratchpad",
        native_options,
        Box::new(|_cc| Box::new(ScratchpadApp::default())),
    )
}

/// If the user has unsaved changes and tries to do something destructive,
/// we defer that action until they answer the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingAction {
    NewFile,
    OpenFile,
    OpenPath(PathBuf),
    Exit,
}

/// Application state: one text buffer + some file metadata.
///
/// We track a `saved_snapshot` so we can detect unsaved changes without any
/// complicated diffing.
struct ScratchpadApp {
    /// Current text in the editor.
    text: String,

    /// The file path we last opened/saved (if any).
    file_path: Option<PathBuf>,

    /// Text at last successful save/open. If text != snapshot => "dirty".
    saved_snapshot: String,

    /// If set, show the "Unsaved changes" dialog and then run this action.
    pending_action: Option<PendingAction>,

    /// Request focus for the editor on the next frame (after New/Open).
    request_editor_focus: bool,

    /// Show last error in a small status bar.
    last_error: Option<String>,
}

impl Default for ScratchpadApp {
    fn default() -> Self {
        Self {
            text: String::new(),
            file_path: None,
            saved_snapshot: String::new(),
            pending_action: None,
            request_editor_focus: true,
            last_error: None,
        }
    }
}

impl ScratchpadApp {
    /// Are there unsaved edits?
    fn is_dirty(&self) -> bool {
        self.text != self.saved_snapshot
    }

    /// Show an error message in the UI.
    fn set_error(&mut self, msg: impl Into<String>) {
        self.last_error = Some(msg.into());
    }

    /// Clear the error message.
    fn clear_error(&mut self) {
        self.last_error = None;
    }

    /// Start a new, empty document.
    fn do_new_file(&mut self) {
        self.text.clear();
        self.saved_snapshot.clear();
        self.file_path = None;
        self.clear_error();
        self.request_editor_focus = true;
    }

    /// Open a file chosen by the user and load it into the buffer.
    fn do_open_file(&mut self) {
        self.clear_error();

        let picked = rfd::FileDialog::new()
            .add_filter("Text", &["txt", "log", "md", "rs", "toml"])
            .add_filter("All files", &["*"])
            .pick_file();

        let Some(path) = picked else {
            // User canceled.
            return;
        };

        match fs::read_to_string(&path) {
            Ok(contents) => {
                self.text = contents;
                self.saved_snapshot = self.text.clone();
                self.file_path = Some(path);
                self.request_editor_focus = true;
            }
            Err(e) => self.set_error(format!("Failed to open file: {e}")),
        }
    }

    /// Open a specific file path (e.g. from drag-and-drop).
    fn do_open_path(&mut self, path: PathBuf) {
        self.clear_error();

        match fs::read_to_string(&path) {
            Ok(contents) => {
                self.text = contents;
                self.saved_snapshot = self.text.clone();
                self.file_path = Some(path);
                self.request_editor_focus = true;
            }
            Err(e) => self.set_error(format!("Failed to open file: {e}")),
        }
    }

    /// Save: if we already have a path, save there; otherwise do Save As.
    fn do_save(&mut self) {
        if self.file_path.is_some() {
            self.do_save_to_current_path();
        } else {
            self.do_save_as();
        }
    }

    /// Save to an existing path (assumes file_path is Some).
    fn do_save_to_current_path(&mut self) {
        self.clear_error();

        let Some(path) = self.file_path.clone() else {
            // Shouldn't happen, but be defensive.
            self.do_save_as();
            return;
        };

        if let Err(e) = write_all_text(&path, &self.text) {
            self.set_error(format!("Failed to save file: {e}"));
            return;
        }

        // Only mark "clean" after a successful write.
        self.saved_snapshot = self.text.clone();
    }

    /// Save As: pick a file path, then save there.
    fn do_save_as(&mut self) {
        self.clear_error();

        let picked = rfd::FileDialog::new()
            .set_file_name(self.suggested_file_name())
            .add_filter("Text", &["txt"])
            .add_filter("All files", &["*"])
            .save_file();

        let Some(path) = picked else {
            // User canceled.
            return;
        };

        if let Err(e) = write_all_text(&path, &self.text) {
            self.set_error(format!("Failed to save file: {e}"));
            return;
        }

        self.file_path = Some(path);
        self.saved_snapshot = self.text.clone();
    }

    /// Suggest a filename in the Save As dialog.
    fn suggested_file_name(&self) -> String {
        match &self.file_path {
            Some(p) => p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Untitled.txt")
                .to_string(),
            None => "Untitled.txt".to_string(),
        }
    }

    /// If there are unsaved changes, defer the action and show the modal.
    /// Otherwise run the action immediately.
    fn maybe_defer_or_run(&mut self, action: PendingAction) {
        if self.is_dirty() {
            self.pending_action = Some(action);
        } else {
            self.run_action(action);
        }
    }

    /// Execute an action immediately (assumes no dirty prompt needed).
    fn run_action(&mut self, action: PendingAction) {
        match action {
            PendingAction::NewFile => self.do_new_file(),
            PendingAction::OpenFile => self.do_open_file(),
            PendingAction::OpenPath(path) => self.do_open_path(path),
            PendingAction::Exit => {
                // Closing happens in update() via ViewportCommand::Close.
                self.pending_action = Some(PendingAction::Exit);
            }
        }
    }

    /// A compact status string: file name + dirty marker + some counts.
    fn status_text(&self) -> String {
        let name = self
            .file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("Untitled");

        let dirty = if self.is_dirty() { "*" } else { "" };

        let lines = if self.text.is_empty() {
            1
        } else {
            self.text.bytes().filter(|&b| b == b'\n').count() + 1
        };

        let chars = self.text.chars().count();

        format!("{name}{dirty}  |  Lines: {lines}  Chars: {chars}")
    }
}

impl eframe::App for ScratchpadApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ─────────────────────────────────────────────────────────────────────
        // Top menu bar
        // ─────────────────────────────────────────────────────────────────────
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New").clicked() {
                        ui.close_menu();
                        self.maybe_defer_or_run(PendingAction::NewFile);
                    }

                    if ui.button("Open…").clicked() {
                        ui.close_menu();
                        self.maybe_defer_or_run(PendingAction::OpenFile);
                    }

                    ui.separator();

                    // Enable save if dirty, or if the file has no path yet (so it can be saved).
                    let save_enabled = self.is_dirty() || self.file_path.is_none();
                    if ui
                        .add_enabled(save_enabled, egui::Button::new("Save"))
                        .clicked()
                    {
                        ui.close_menu();
                        self.do_save();
                    }

                    if ui.button("Save As…").clicked() {
                        ui.close_menu();
                        self.do_save_as();
                    }

                    ui.separator();

                    if ui.button("Exit").clicked() {
                        ui.close_menu();
                        self.maybe_defer_or_run(PendingAction::Exit);
                    }
                });

                ui.menu_button("Edit", |ui| {
                    // egui's TextEdit handles Ctrl+C/V/X/A on native platforms.
                    // These menu items are mostly for familiarity.
                    ui.label("Use Ctrl+C / Ctrl+V / Ctrl+X / Ctrl+A in the editor.");
                });

                // Right-side: show current full path (or Untitled)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(path) = &self.file_path {
                        ui.add(egui::Label::new(path.display().to_string()).truncate(true));
                    } else {
                        ui.add(egui::Label::new("Untitled"));
                    }
                });
            });
        });

        // ─────────────────────────────────────────────────────────────────────
        // Bottom status bar
        // ─────────────────────────────────────────────────────────────────────
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(self.status_text());

                if let Some(err) = &self.last_error {
                    ui.separator();
                    ui.label(egui::RichText::new(err).color(egui::Color32::LIGHT_RED));
                }
            });
        });

        // ─────────────────────────────────────────────────────────────────────
        // Central editor
        // ─────────────────────────────────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            // Stable ID helps keep focus/state consistent.
            let editor_id = ui.make_persistent_id("editor");

            let text_edit = egui::TextEdit::multiline(&mut self.text)
                .id(editor_id)
                .desired_width(f32::INFINITY);

            let response = egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| ui.add_sized(ui.available_size(), text_edit))
                .inner;

            // Grab focus after New/Open.
            if self.request_editor_focus {
                response.request_focus();
                self.request_editor_focus = false;
            }

            // Clicking into the editor clears stale error messages.
            if response.clicked() {
                self.clear_error();
            }
        });

        // Handle file drag-and-drop.
        let dropped_paths: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });

        if let Some(path) = dropped_paths.into_iter().next() {
            self.maybe_defer_or_run(PendingAction::OpenPath(path));
        }

        // ─────────────────────────────────────────────────────────────────────
        // Unsaved changes modal
        // ─────────────────────────────────────────────────────────────────────
        if let Some(action) = self.pending_action.clone() {
            let is_exit = matches!(action, PendingAction::Exit);
            // If the pending action is Exit and we are not dirty, close immediately.
            // (This can happen if the action was queued but changes were saved.)
            if is_exit && !self.is_dirty() {
                self.pending_action = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }

            egui::Window::new("Unsaved changes")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("You have unsaved changes. What do you want to do?");
                    ui.add_space(10.0);

                    ui.horizontal(|ui| {
                        // Save: attempt to save, then proceed only if we're no longer dirty.
                        if ui.button("Save").clicked() {
                            self.do_save();

                            // Proceed only if the save actually succeeded (i.e., now clean).
                            if !self.is_dirty() {
                                self.pending_action = None;
                                if is_exit {
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                } else {
                                    self.run_action(action.clone());
                                    self.pending_action = None;
                                }
                            }
                        }

                        // Don't Save: discard changes and proceed.
                        if ui.button("Don't Save").clicked() {
                            self.pending_action = None;

                            // Discard by reverting the buffer to the last saved snapshot.
                            self.text = self.saved_snapshot.clone();

                            if is_exit {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            } else {
                                self.run_action(action.clone());
                                self.pending_action = None;
                            }
                        }

                        // Cancel: abort the operation.
                        if ui.button("Cancel").clicked() {
                            self.pending_action = None;
                        }
                    });
                });
        }

        // If the user selected Exit while clean (no modal shown), close now.
        if self.pending_action == Some(PendingAction::Exit) && !self.is_dirty() {
            self.pending_action = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

/// Write the full buffer to disk.
/// This is intentionally tiny so the main app code reads cleanly.
fn write_all_text(path: &Path, contents: &str) -> std::io::Result<()> {
    fs::write(path, contents)
}

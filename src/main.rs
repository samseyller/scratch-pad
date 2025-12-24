#![windows_subsystem = "windows"]

use std::fs;
use std::path::{Path, PathBuf};

use arboard::Clipboard;
use eframe::egui;
use egui::text_edit::TextEditOutput;
use serde::{Deserialize, Serialize};
use regex::Regex;

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
            .with_inner_size([900.0, 600.0])
            .with_icon(make_app_icon()),
        ..Default::default()
    };

    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);

    eframe::run_native(
        "Scratchpad",
        native_options,
        Box::new(move |cc| Box::new(ScratchpadApp::new(cc, initial_path.clone()))),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum FontFamilySetting {
    Proportional,
    Monospace,
}

impl FontFamilySetting {
    fn to_egui(self) -> egui::FontFamily {
        match self {
            FontFamilySetting::Proportional => egui::FontFamily::Proportional,
            FontFamilySetting::Monospace => egui::FontFamily::Monospace,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineEnding {
    Lf,
    CrLf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct AppSettings {
    font_size: f32,
    font_family: FontFamilySetting,
    show_line_numbers: bool,
    recent_files: Vec<String>,
    word_wrap: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            font_size: 16.0,
            font_family: FontFamilySetting::Monospace,
            show_line_numbers: true,
            recent_files: Vec::new(),
            word_wrap: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FindMode {
    Literal,
    Regex,
}

#[derive(Clone, Debug)]
struct FindState {
    open: bool,
    show_replace: bool,
    query: String,
    replace: String,
    mode: FindMode,
    case_sensitive: bool,
    highlight_all: bool,
    last_result: Option<String>,
}

impl Default for FindState {
    fn default() -> Self {
        Self {
            open: false,
            show_replace: false,
            query: String::new(),
            replace: String::new(),
            mode: FindMode::Literal,
            case_sensitive: false,
            highlight_all: false,
            last_result: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct FindMatch {
    start_byte: usize,
    end_byte: usize,
    start_char: usize,
    end_char: usize,
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

    /// Original line-ending style of the file on disk.
    line_ending: LineEnding,

    /// If set, show the "Unsaved changes" dialog and then run this action.
    pending_action: Option<PendingAction>,

    /// Request focus for the editor on the next frame (after New/Open).
    request_editor_focus: bool,

    /// Clear editor selection/state on the next frame (after New/Open).
    reset_editor_state: bool,

    /// Show last error in a small status bar.
    last_error: Option<String>,

    /// UI settings that should persist between runs.
    settings: AppSettings,

    /// Find/replace panel state.
    find_state: FindState,
}

impl Default for ScratchpadApp {
    fn default() -> Self {
        Self {
            text: String::new(),
            file_path: None,
            saved_snapshot: String::new(),
            line_ending: LineEnding::Lf,
            pending_action: None,
            request_editor_focus: true,
            reset_editor_state: false,
            last_error: None,
            settings: AppSettings::default(),
            find_state: FindState::default(),
        }
    }
}

impl ScratchpadApp {
    fn new(cc: &eframe::CreationContext<'_>, initial_path: Option<PathBuf>) -> Self {
        let mut app = Self::default();
        if let Some(storage) = cc.storage {
            if let Some(settings) = eframe::get_value(storage, "scratchpad_settings") {
                app.settings = settings;
            }
        }
        if let Some(path) = initial_path {
            app.do_open_path(path);
        }
        app
    }

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

    fn normalize_line_endings(contents: &str) -> (String, LineEnding) {
        if contents.contains("\r\n") {
            (contents.replace("\r\n", "\n"), LineEnding::CrLf)
        } else {
            (contents.to_owned(), LineEnding::Lf)
        }
    }

    fn apply_line_ending(&self, contents: &str) -> String {
        match self.line_ending {
            LineEnding::Lf => contents.to_owned(),
            LineEnding::CrLf => contents.replace("\n", "\r\n"),
        }
    }

    /// Start a new, empty document.
    fn do_new_file(&mut self) {
        self.text.clear();
        self.saved_snapshot.clear();
        self.file_path = None;
        self.line_ending = LineEnding::Lf;
        self.clear_error();
        self.request_editor_focus = true;
        self.reset_editor_state = true;
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
                let (normalized, line_ending) = Self::normalize_line_endings(&contents);
                self.text = normalized;
                self.saved_snapshot = self.text.clone();
                self.line_ending = line_ending;
                self.push_recent(&path);
                self.file_path = Some(path);
                self.request_editor_focus = true;
                self.reset_editor_state = true;
            }
            Err(e) => self.set_error(format!("Failed to open file: {e}")),
        }
    }

    /// Open a specific file path (e.g. from drag-and-drop).
    fn do_open_path(&mut self, path: PathBuf) {
        self.clear_error();

        match fs::read_to_string(&path) {
            Ok(contents) => {
                let (normalized, line_ending) = Self::normalize_line_endings(&contents);
                self.text = normalized;
                self.saved_snapshot = self.text.clone();
                self.line_ending = line_ending;
                self.push_recent(&path);
                self.file_path = Some(path);
                self.request_editor_focus = true;
                self.reset_editor_state = true;
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

        let contents = self.apply_line_ending(&self.text);
        if let Err(e) = write_all_text(&path, &contents) {
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

        let contents = self.apply_line_ending(&self.text);
        if let Err(e) = write_all_text(&path, &contents) {
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

    fn handle_command(&mut self, command: PendingAction) {
        self.maybe_defer_or_run(command);
    }

    fn push_recent(&mut self, path: &Path) {
        let path_str = path.to_string_lossy().to_string();
        self.settings.recent_files.retain(|p| p != &path_str);
        self.settings.recent_files.insert(0, path_str);
        if self.settings.recent_files.len() > 10 {
            self.settings.recent_files.truncate(10);
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

    /// A compact status string: file name + dirty marker + counts + cursor info.
    fn status_text(&self, ctx: &egui::Context) -> String {
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

        let mut line = 1usize;
        let mut col = 1usize;
        let mut selection_len = 0usize;
        if let Some(state) = egui::TextEdit::load_state(ctx, egui::Id::new("editor")) {
            if let Some(range) = state.cursor.char_range() {
                let cursor_idx = range.primary.index;
                for (idx, ch) in self.text.chars().enumerate() {
                    if idx >= cursor_idx {
                        break;
                    }
                    if ch == '\n' {
                        line += 1;
                        col = 1;
                    } else {
                        col += 1;
                    }
                }

                let [min, max] = range.sorted();
                selection_len = max.index.saturating_sub(min.index);
            }
        }

        let size_bytes = self.apply_line_ending(&self.text).as_bytes().len();
        let size_text = format_size(size_bytes);

        let line_ending = match self.line_ending {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        };

        let selection_text = if selection_len > 0 {
            format!("  |  Sel: {selection_len}")
        } else {
            String::new()
        };

        format!(
            "{name}{dirty}  |  Ln {line}, Col {col}  |  Lines: {lines}  Chars: {chars}  |  Size: {size_text}  |  {line_ending}{selection_text}"
        )
    }

    fn apply_font_settings(&self, ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        for (_text_style, font_id) in style.text_styles.iter_mut() {
            font_id.size = 14.0;
        }
        ctx.set_style(style);
    }

    fn make_editor_layouter(
        wrap: bool,
        wrap_width: f32,
        font_id: egui::FontId,
    ) -> impl FnMut(&egui::Ui, &str, f32) -> std::sync::Arc<egui::Galley> {
        move |ui, text, _wrap_width| {
            let max_width = if wrap { wrap_width } else { f32::INFINITY };
            let mut job = egui::text::LayoutJob::simple(
                text.to_owned(),
                font_id.clone(),
                ui.visuals().text_color(),
                max_width,
            );
            job.wrap.max_width = max_width;
            ui.fonts(|f| f.layout_job(job))
        }
    }

    fn set_find_result(&mut self, msg: impl Into<String>) {
        self.find_state.last_result = Some(msg.into());
    }

    fn build_find_regex(&self) -> Result<Regex, String> {
        if self.find_state.query.is_empty() {
            return Err("Find text is empty".to_string());
        }
        let pattern = match self.find_state.mode {
            FindMode::Literal => regex::escape(&self.find_state.query),
            FindMode::Regex => self.find_state.query.clone(),
        };
        let mut builder = regex::RegexBuilder::new(&pattern);
        builder.case_insensitive(!self.find_state.case_sensitive);
        builder.build().map_err(|e| e.to_string())
    }

    fn byte_to_char_index(text: &str, byte_index: usize) -> usize {
        text[..byte_index].chars().count()
    }

    fn current_selection_char_range(&self, ctx: &egui::Context) -> Option<(usize, usize)> {
        let editor_id = egui::Id::new("editor");
        egui::TextEdit::load_state(ctx, editor_id)
            .and_then(|state| state.cursor.char_range())
            .map(|range| {
                let [min, max] = range.sorted();
                (min.index, max.index)
            })
    }

    fn select_match(&mut self, ctx: &egui::Context, start_char: usize, end_char: usize) {
        let editor_id = egui::Id::new("editor");
        if let Some(mut state) = egui::TextEdit::load_state(ctx, editor_id) {
            let range = egui::text::CCursorRange::two(
                egui::text::CCursor::new(start_char),
                egui::text::CCursor::new(end_char),
            );
            state.cursor.set_char_range(Some(range));
            state.store(ctx, editor_id);
        }
        ctx.memory_mut(|mem| mem.request_focus(editor_id));
        self.request_editor_focus = true;
    }

    fn find_matches_with(&self, regex: &Regex) -> Vec<FindMatch> {
        regex
            .find_iter(&self.text)
            .map(|m| {
                let start_char = Self::byte_to_char_index(&self.text, m.start());
                let end_char = Self::byte_to_char_index(&self.text, m.end());
                FindMatch {
                    start_byte: m.start(),
                    end_byte: m.end(),
                    start_char,
                    end_char,
                }
            })
            .collect()
    }

    fn find_next_match_index(&self, ctx: &egui::Context, matches: &[FindMatch]) -> usize {
        let start_from = self
            .current_selection_char_range(ctx)
            .map(|(_, end)| end)
            .unwrap_or(0);
        matches
            .iter()
            .position(|m| m.start_char >= start_from)
            .unwrap_or(0)
    }

    fn find_prev_match_index(&self, ctx: &egui::Context, matches: &[FindMatch]) -> usize {
        let start_from = self
            .current_selection_char_range(ctx)
            .map(|(start, _)| start)
            .unwrap_or(usize::MAX);
        let mut idx = None;
        for (i, m) in matches.iter().enumerate() {
            if m.start_char < start_from {
                idx = Some(i);
            }
        }
        idx.unwrap_or_else(|| matches.len().saturating_sub(1))
    }

    fn find_next(&mut self, ctx: &egui::Context) {
        let regex = match self.build_find_regex() {
            Ok(r) => r,
            Err(e) => {
                self.set_find_result(e);
                return;
            }
        };
        let matches = self.find_matches_with(&regex);
        if matches.is_empty() {
            self.set_find_result("No matches");
            return;
        }
        let idx = self.find_next_match_index(ctx, &matches);
        let m = matches[idx];
        self.select_match(ctx, m.start_char, m.end_char);
        self.set_find_result(format!("Match {}/{}", idx + 1, matches.len()));
    }

    fn find_previous(&mut self, ctx: &egui::Context) {
        let regex = match self.build_find_regex() {
            Ok(r) => r,
            Err(e) => {
                self.set_find_result(e);
                return;
            }
        };
        let matches = self.find_matches_with(&regex);
        if matches.is_empty() {
            self.set_find_result("No matches");
            return;
        }
        let idx = self.find_prev_match_index(ctx, &matches);
        let m = matches[idx];
        self.select_match(ctx, m.start_char, m.end_char);
        self.set_find_result(format!("Match {}/{}", idx + 1, matches.len()));
    }

    fn find_all(&mut self, ctx: &egui::Context) {
        let regex = match self.build_find_regex() {
            Ok(r) => r,
            Err(e) => {
                self.set_find_result(e);
                return;
            }
        };
        let matches = self.find_matches_with(&regex);
        if matches.is_empty() {
            self.set_find_result("No matches");
            return;
        }
        self.find_state.highlight_all = true;
        let m = matches[0];
        self.select_match(ctx, m.start_char, m.end_char);
        self.set_find_result(format!("Found {} matches", matches.len()));
    }

    fn replace_current(&mut self, ctx: &egui::Context) {
        let regex = match self.build_find_regex() {
            Ok(r) => r,
            Err(e) => {
                self.set_find_result(e);
                return;
            }
        };
        let matches = self.find_matches_with(&regex);
        if matches.is_empty() {
            self.set_find_result("No matches");
            return;
        }
        let mut target_idx = None;
        if let Some((sel_start, sel_end)) = self.current_selection_char_range(ctx) {
            for (i, m) in matches.iter().enumerate() {
                if m.start_char == sel_start && m.end_char == sel_end {
                    target_idx = Some(i);
                    break;
                }
            }
        }
        let idx = target_idx.unwrap_or_else(|| self.find_next_match_index(ctx, &matches));
        let m = matches[idx];
        let replacement = if self.find_state.mode == FindMode::Regex {
            regex
                .replace(&self.text[m.start_byte..m.end_byte], self.find_state.replace.as_str())
                .to_string()
        } else {
            self.find_state.replace.clone()
        };
        self.text.replace_range(m.start_byte..m.end_byte, &replacement);
        let new_end = m.start_char + replacement.chars().count();
        self.select_match(ctx, m.start_char, new_end);
        self.set_find_result(format!("Replaced {}/{}", idx + 1, matches.len()));
    }

    fn replace_all(&mut self) {
        let regex = match self.build_find_regex() {
            Ok(r) => r,
            Err(e) => {
                self.set_find_result(e);
                return;
            }
        };
        let matches = self.find_matches_with(&regex);
        if matches.is_empty() {
            self.set_find_result("No matches");
            return;
        }
        let replaced = regex
            .replace_all(&self.text, self.find_state.replace.as_str())
            .to_string();
        self.text = replaced;
        self.request_editor_focus = true;
        self.set_find_result(format!("Replaced {}", matches.len()));
    }

    fn paint_find_highlights(&mut self, ui: &egui::Ui, output: &TextEditOutput) {
        if !self.find_state.highlight_all {
            return;
        }
        let regex = match self.build_find_regex() {
            Ok(r) => r,
            Err(e) => {
                self.set_find_result(e);
                self.find_state.highlight_all = false;
                return;
            }
        };
        let matches = self.find_matches_with(&regex);
        if matches.is_empty() {
            return;
        }
        let painter = ui.painter_at(output.text_clip_rect);
        for m in matches {
            let range = egui::text::CursorRange::two(
                output
                    .galley
                    .from_ccursor(egui::text::CCursor::new(m.start_char)),
                output
                    .galley
                    .from_ccursor(egui::text::CCursor::new(m.end_char)),
            );
            egui::text_selection::visuals::paint_text_selection(
                &painter,
                ui.visuals(),
                output.galley_pos,
                &output.galley,
                &range,
                None,
            );
        }
    }

    fn send_editor_event(&mut self, ctx: &egui::Context, event: egui::Event) {
        let editor_id = egui::Id::new("editor");
        ctx.memory_mut(|mem| mem.request_focus(editor_id));
        ctx.input_mut(|i| i.events.push(event));
        self.request_editor_focus = true;
    }

    fn paste_from_clipboard(&mut self, ctx: &egui::Context) {
        match Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
            Ok(text) => {
                if !text.is_empty() {
                    self.send_editor_event(ctx, egui::Event::Paste(text));
                }
            }
            Err(e) => self.set_error(format!("Clipboard paste failed: {e}")),
        }
    }
}

impl eframe::App for ScratchpadApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "scratchpad_settings", &self.settings);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_font_settings(ctx);
        let mut open_find = false;
        let mut open_replace = false;
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::COMMAND, egui::Key::F) {
                open_find = true;
            }
            if i.consume_key(egui::Modifiers::COMMAND, egui::Key::R) {
                open_replace = true;
            }
        });
        if open_find {
            self.find_state.open = true;
            self.find_state.show_replace = false;
        }
        if open_replace {
            self.find_state.open = true;
            self.find_state.show_replace = true;
        }
        let mut hotkey_new = false;
        let mut hotkey_open = false;
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::COMMAND, egui::Key::N) {
                hotkey_new = true;
            }
            if i.consume_key(egui::Modifiers::COMMAND, egui::Key::O) {
                hotkey_open = true;
            }
        });
        if hotkey_new {
            self.handle_command(PendingAction::NewFile);
        }
        if hotkey_open {
            self.handle_command(PendingAction::OpenFile);
        }
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if close_requested && self.is_dirty() {
            self.pending_action = Some(PendingAction::Exit);
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        // ─────────────────────────────────────────────────────────────────────
        // Top menu bar
        // ─────────────────────────────────────────────────────────────────────
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New (Ctrl+N)").clicked() {
                        ui.close_menu();
                        self.handle_command(PendingAction::NewFile);
                    }

                    if ui.button("Open... (Ctrl+O)").clicked() {
                        ui.close_menu();
                        self.handle_command(PendingAction::OpenFile);
                    }

                    ui.menu_button("Recent", |ui| {
                        let mut to_open: Option<PathBuf> = None;
                        let mut remove_entry: Option<String> = None;
                        let recent = self.settings.recent_files.clone();

                        if recent.is_empty() {
                            ui.label("No recent files");
                        } else {
                            for entry in recent.into_iter().take(10) {
                                if ui.button(entry.as_str()).clicked() {
                                    ui.close_menu();
                                    let path = PathBuf::from(&entry);
                                    if path.exists() {
                                        to_open = Some(path);
                                    } else {
                                        remove_entry = Some(entry);
                                        self.set_error("Recent file missing; removed from list.");
                                    }
                                }
                            }
                        }

                        ui.separator();
                        if ui.button("Clear recent files").clicked() {
                            self.settings.recent_files.clear();
                        }

                        if let Some(entry) = remove_entry {
                            self.settings.recent_files.retain(|p| p != &entry);
                        }
                        if let Some(path) = to_open {
                            self.handle_command(PendingAction::OpenPath(path));
                        }
                    });

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
                    if ui.button("Copy (Ctrl+C)").clicked() {
                        ui.close_menu();
                        self.send_editor_event(ctx, egui::Event::Copy);
                    }

                    if ui.button("Cut (Ctrl+X)").clicked() {
                        ui.close_menu();
                        self.send_editor_event(ctx, egui::Event::Cut);
                    }

                    if ui.button("Paste (Ctrl+V)").clicked() {
                        ui.close_menu();
                        self.paste_from_clipboard(ctx);
                    }

                    if ui.button("Select All (Ctrl+A)").clicked() {
                        ui.close_menu();
                        self.send_editor_event(
                            ctx,
                            egui::Event::Key {
                                key: egui::Key::A,
                                physical_key: None,
                                pressed: true,
                                repeat: false,
                                modifiers: egui::Modifiers::COMMAND,
                            },
                        );
                    }

                    ui.separator();
                    if ui.button("Find (Ctrl+F)").clicked() {
                        ui.close_menu();
                        self.find_state.open = true;
                        self.find_state.show_replace = false;
                    }
                    if ui.button("Find and Replace (Ctrl+R)").clicked() {
                        ui.close_menu();
                        self.find_state.open = true;
                        self.find_state.show_replace = true;
                    }
                });

                ui.menu_button("View", |ui| {
                    let mut changed = false;

                    ui.label("Font size");
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut self.settings.font_size, 10.0..=24.0)
                                .step_by(0.5),
                        )
                        .changed();

                    ui.separator();
                    ui.label("Font family");
                    changed |= ui
                        .radio_value(
                            &mut self.settings.font_family,
                            FontFamilySetting::Proportional,
                            "Proportional",
                        )
                        .changed();
                    changed |= ui
                        .radio_value(
                            &mut self.settings.font_family,
                            FontFamilySetting::Monospace,
                            "Monospace",
                        )
                        .changed();

                    ui.separator();
                    changed |= ui
                        .checkbox(&mut self.settings.show_line_numbers, "Line numbers")
                        .changed();
                    changed |= ui
                        .checkbox(&mut self.settings.word_wrap, "Word wrap")
                        .changed();

                    if changed {
                        self.apply_font_settings(ctx);
                    }
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

        if self.find_state.open {
            let mut find_open = self.find_state.open;
            let title = if self.find_state.show_replace {
                "Find and Replace"
            } else {
                "Find"
            };
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .open(&mut find_open)
                .show(ctx, |ui| {
                    let mut find_changed = false;
                    ui.horizontal(|ui| {
                        ui.label("Find");
                        find_changed |= ui
                            .add(
                                egui::TextEdit::singleline(&mut self.find_state.query)
                                    .desired_width(240.0),
                            )
                            .changed();
                    });

                    if self.find_state.show_replace {
                        ui.horizontal(|ui| {
                            ui.label("Replace");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.find_state.replace)
                                    .desired_width(240.0),
                            );
                        });
                    }

                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("Mode");
                        find_changed |= ui
                            .radio_value(&mut self.find_state.mode, FindMode::Literal, "Literal")
                            .changed();
                        find_changed |= ui
                            .radio_value(&mut self.find_state.mode, FindMode::Regex, "Regex")
                            .changed();
                    });
                    find_changed |= ui
                        .checkbox(&mut self.find_state.case_sensitive, "Case sensitive")
                        .changed();

                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Find Next").clicked() {
                            self.find_next(ctx);
                        }
                        if ui.button("Find Previous").clicked() {
                            self.find_previous(ctx);
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Find All").clicked() {
                            self.find_all(ctx);
                        }
                    });
                    if self.find_state.show_replace {
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("Replace").clicked() {
                                self.replace_current(ctx);
                            }
                            if ui.button("Replace All").clicked() {
                                self.replace_all();
                            }
                        });
                    }

                    if let Some(msg) = &self.find_state.last_result {
                        ui.add_space(6.0);
                        ui.label(msg);
                    }

                    if find_changed {
                        self.find_state.highlight_all = false;
                    }
                });
            self.find_state.open = find_open;
            if !find_open {
                self.find_state.highlight_all = false;
            }
        }

        // ─────────────────────────────────────────────────────────────────────
        // Bottom status bar
        // ─────────────────────────────────────────────────────────────────────
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(self.status_text(ctx));

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
            let editor_id = egui::Id::new("editor");
            if self.reset_editor_state {
                egui::TextEdit::store_state(ctx, editor_id, Default::default());
                self.reset_editor_state = false;
            }
            let show_line_numbers = self.settings.show_line_numbers;

            let font_id =
                egui::FontId::new(self.settings.font_size, self.settings.font_family.to_egui());
            let line_number_color = ui.visuals().weak_text_color();

            let lines: Vec<String> = if show_line_numbers {
                if self.text.is_empty() {
                    vec![String::new()]
                } else {
                    self.text.split('\n').map(str::to_owned).collect()
                }
            } else {
                Vec::new()
            };

            let scroll_area = if self.settings.word_wrap {
                egui::ScrollArea::vertical()
            } else {
                egui::ScrollArea::both()
            };
            let response = scroll_area
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let available_width = ui.available_width();
                    let gutter_width = if show_line_numbers {
                        let total_lines = lines.len().max(1);
                        let digits = total_lines.to_string().len();
                        let sample = "9".repeat(digits);
                        let width = ui
                            .fonts(|f| {
                                f.layout_no_wrap(sample, font_id.clone(), line_number_color)
                            })
                            .size()
                            .x;
                        width + 8.0
                    } else {
                        0.0
                    };

                    let text_width = if show_line_numbers {
                        (available_width - gutter_width - ui.spacing().item_spacing.x).max(64.0)
                    } else {
                        available_width
                    };

                    let text_margin_x = 4.0;
                    let line_number_offset_y = -8.0;
                    let wrap_width = if self.settings.word_wrap {
                        (text_width - 2.0 * text_margin_x).max(1.0)
                    } else {
                        f32::INFINITY
                    };

                    ui.horizontal(|ui| {
                        if show_line_numbers {
                            let line_height = ui
                                .fonts(|f| {
                                    f.layout_no_wrap("0".to_owned(), font_id.clone(), line_number_color)
                                })
                                .size()
                                .y;

                            ui.allocate_ui_with_layout(
                                egui::vec2(gutter_width, 0.0),
                                egui::Layout::top_down(egui::Align::Max),
                                |ui| {
                                    ui.spacing_mut().item_spacing.y = 0.0;
                                    ui.add_space(line_number_offset_y);
                                    for (idx, line) in lines.iter().enumerate() {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new((idx + 1).to_string())
                                                    .color(line_number_color)
                                                    .font(font_id.clone()),
                                            )
                                            .wrap(false),
                                        );

                                        let line_text = if line.is_empty() { " " } else { line };
                                        let galley = ui.fonts(|f| {
                                            let mut job = egui::text::LayoutJob::simple(
                                                line_text.to_owned(),
                                                font_id.clone(),
                                                line_number_color,
                                                wrap_width,
                                            );
                                            job.wrap.max_width = wrap_width;
                                            f.layout_job(job)
                                        });
                                        let first_height = galley
                                            .rows
                                            .first()
                                            .map(|row| row.height())
                                            .unwrap_or(line_height);
                                        let extra = (galley.size().y - first_height).max(0.0);
                                        if extra > 0.0 {
                                            ui.add_space(extra);
                                        }
                                    }
                                },
                            );
                        }

                        let mut layouter = Self::make_editor_layouter(
                            self.settings.word_wrap,
                            wrap_width,
                            font_id.clone(),
                        );
                        let text_edit = egui::TextEdit::multiline(&mut self.text)
                            .id(editor_id)
                            .font(font_id.clone())
                            .layouter(&mut layouter)
                            .desired_width(text_width);

                        let output = ui
                            .allocate_ui_with_layout(
                                egui::vec2(text_width, ui.available_height()),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| text_edit.show(ui),
                            )
                            .inner;

                        if self.find_state.highlight_all {
                            self.paint_find_highlights(ui, &output);
                        }

                        output.response.clone()
                    })
                    .inner
                })
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

fn make_app_icon() -> egui::IconData {
    let size = 64u32;
    let radius = 12.0f32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5;
            let dy = y as f32 + 0.5;
            let (inside, alpha) = inside_rounded_rect(dx, dy, size as f32, radius);
            if inside {
                set_pixel(&mut rgba, size, x, y, 0, 0, 0, (alpha * 255.0) as u8);
            }
        }
    }

    draw_letter_s(&mut rgba, size);

    egui::IconData { rgba, width: size, height: size }
}

fn inside_rounded_rect(x: f32, y: f32, size: f32, radius: f32) -> (bool, f32) {
    let left = 0.0;
    let top = 0.0;
    let right = size;
    let bottom = size;

    let inner_left = left + radius;
    let inner_right = right - radius;
    let inner_top = top + radius;
    let inner_bottom = bottom - radius;

    if x >= inner_left && x <= inner_right && y >= top && y <= bottom {
        return (true, 1.0);
    }
    if y >= inner_top && y <= inner_bottom && x >= left && x <= right {
        return (true, 1.0);
    }

    let (cx, cy) = if x < inner_left && y < inner_top {
        (inner_left, inner_top)
    } else if x > inner_right && y < inner_top {
        (inner_right, inner_top)
    } else if x < inner_left && y > inner_bottom {
        (inner_left, inner_bottom)
    } else if x > inner_right && y > inner_bottom {
        (inner_right, inner_bottom)
    } else {
        return (false, 0.0);
    };

    let dx = x - cx;
    let dy = y - cy;
    let dist = (dx * dx + dy * dy).sqrt();
    if dist <= radius {
        let edge = (radius - dist).min(1.0);
        (true, edge)
    } else {
        (false, 0.0)
    }
}

fn draw_letter_s(rgba: &mut [u8], size: u32) {
    let glyph = [
        "01110",
        "10001",
        "10000",
        "01110",
        "00001",
        "10001",
        "01110",
    ];
    let scale = 6u32;
    let glyph_w = (glyph[0].len() as u32) * scale;
    let glyph_h = (glyph.len() as u32) * scale;
    let start_x = (size - glyph_w) / 2;
    let start_y = (size - glyph_h) / 2;

    for (row_idx, row) in glyph.iter().enumerate() {
        for (col_idx, ch) in row.chars().enumerate() {
            if ch != '1' {
                continue;
            }
            let px = start_x + (col_idx as u32) * scale;
            let py = start_y + (row_idx as u32) * scale;
            for y in py..(py + scale) {
                for x in px..(px + scale) {
                    set_pixel(rgba, size, x, y, 255, 255, 255, 255);
                }
            }
        }
    }
}

fn set_pixel(rgba: &mut [u8], size: u32, x: u32, y: u32, r: u8, g: u8, b: u8, a: u8) {
    let idx = ((y * size + x) * 4) as usize;
    if idx + 3 >= rgba.len() {
        return;
    }
    rgba[idx] = r;
    rgba[idx + 1] = g;
    rgba[idx + 2] = b;
    rgba[idx + 3] = a;
}

fn format_size(bytes: usize) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0usize;
    while size >= 1024.0 && unit + 1 < units.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", units[unit])
    } else if size >= 100.0 {
        format!("{:.0} {}", size, units[unit])
    } else if size >= 10.0 {
        format!("{:.1} {}", size, units[unit])
    } else {
        format!("{:.2} {}", size, units[unit])
    }
}

#![windows_subsystem = "windows"]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use arboard::Clipboard;
use eframe::egui;
use egui::text_edit::TextEditOutput;
use serde::{Deserialize, Serialize};
use regex::Regex;
use ron::ser::PrettyConfig;
use semver::Version;
use serde_json::Value;

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
const APP_ID: &str = "Scratchpad";
const BUILD_TIME: &str = env!("SCRATCHPAD_BUILD_TIME", "unknown");

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        // Keep it simple, but give a comfortable window size.
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_ID)
            .with_inner_size([900.0, 600.0])
            .with_icon(make_app_icon()),
        ..Default::default()
    };

    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);

    eframe::run_native(
        APP_ID,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl TextEncoding {
    fn label(self) -> &'static str {
        match self {
            TextEncoding::Utf8 => "UTF-8",
            TextEncoding::Utf16Le => "UTF-16 LE",
            TextEncoding::Utf16Be => "UTF-16 BE",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum HighlightMode {
    Off,
    Syntax,
    Markdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct AppSettings {
    font_size: f32,
    font_family: FontFamilySetting,
    show_line_numbers: bool,
    recent_files: Vec<String>,
    word_wrap: bool,
    highlight_mode: HighlightMode,
    check_updates: bool,
    watch_file_changes: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            font_size: 16.0,
            font_family: FontFamilySetting::Monospace,
            show_line_numbers: true,
            recent_files: Vec::new(),
            word_wrap: true,
            highlight_mode: HighlightMode::Syntax,
            check_updates: true,
            watch_file_changes: true,
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
    scroll_to_match: bool,
    request_focus: bool,
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
            scroll_to_match: false,
            request_focus: false,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorAction {
    Copy,
    Cut,
    Paste,
    SelectAll,
    Uppercase,
    Lowercase,
    Propercase,
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

    /// Current encoding for load/save.
    encoding: TextEncoding,

    /// Last known modified timestamp for the opened file.
    last_file_mtime: Option<SystemTime>,

    /// If set, show the "Unsaved changes" dialog and then run this action.
    pending_action: Option<PendingAction>,

    /// Request focus for the editor on the next frame (after New/Open).
    request_editor_focus: bool,

    /// Clear editor selection/state on the next frame (after New/Open).
    reset_editor_state: bool,

    /// Right-click editor context menu state.
    editor_context_menu_open: bool,
    editor_context_menu_pos: Option<egui::Pos2>,
    editor_context_menu_selection: Option<egui::text::CCursorRange>,
    editor_context_menu_index: usize,
    pending_editor_action: Option<EditorAction>,

    /// Show last error in a small status bar.
    last_error: Option<String>,

    /// UI settings that should persist between runs.
    settings: AppSettings,

    /// Find/replace panel state.
    find_state: FindState,
    about_open: bool,

    update_available: Option<String>,
    update_available_url: Option<String>,
    update_check_in_flight: bool,
    update_rx: Option<Receiver<UpdateCheckResult>>,

    file_change_prompt_open: bool,
    file_change_disabled: bool,
}

impl Default for ScratchpadApp {
    fn default() -> Self {
        Self {
            text: String::new(),
            file_path: None,
            saved_snapshot: String::new(),
            line_ending: LineEnding::Lf,
            encoding: TextEncoding::Utf8,
            last_file_mtime: None,
            pending_action: None,
            request_editor_focus: true,
            reset_editor_state: false,
            editor_context_menu_open: false,
            editor_context_menu_pos: None,
            editor_context_menu_selection: None,
            editor_context_menu_index: 0,
            pending_editor_action: None,
            last_error: None,
            settings: AppSettings::default(),
            find_state: FindState::default(),
            about_open: false,
            update_available: None,
            update_available_url: None,
            update_check_in_flight: false,
            update_rx: None,
            file_change_prompt_open: false,
            file_change_disabled: false,
        }
    }
}

impl ScratchpadApp {
    fn new(cc: &eframe::CreationContext<'_>, initial_path: Option<PathBuf>) -> Self {
        let mut app = Self::default();
        if let Some(settings) = load_config_settings() {
            app.settings = settings;
        }
        if let Some(path) = initial_path {
            app.do_open_path(path);
        }
        if app.settings.check_updates {
            app.start_update_check(cc.egui_ctx.clone());
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

    fn decode_bytes(bytes: &[u8], encoding: TextEncoding) -> Result<String, String> {
        match encoding {
            TextEncoding::Utf8 => String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string()),
            TextEncoding::Utf16Le => {
                if bytes.len() % 2 != 0 {
                    return Err("UTF-16 LE data has odd length.".to_string());
                }
                let mut units = Vec::with_capacity(bytes.len() / 2);
                for chunk in bytes.chunks_exact(2) {
                    units.push(u16::from_le_bytes([chunk[0], chunk[1]]));
                }
                String::from_utf16(&units).map_err(|e| e.to_string())
            }
            TextEncoding::Utf16Be => {
                if bytes.len() % 2 != 0 {
                    return Err("UTF-16 BE data has odd length.".to_string());
                }
                let mut units = Vec::with_capacity(bytes.len() / 2);
                for chunk in bytes.chunks_exact(2) {
                    units.push(u16::from_be_bytes([chunk[0], chunk[1]]));
                }
                String::from_utf16(&units).map_err(|e| e.to_string())
            }
        }
    }

    fn detect_encoding(bytes: &[u8]) -> (TextEncoding, usize) {
        if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            return (TextEncoding::Utf8, 3);
        }
        if bytes.starts_with(&[0xFF, 0xFE]) {
            return (TextEncoding::Utf16Le, 2);
        }
        if bytes.starts_with(&[0xFE, 0xFF]) {
            return (TextEncoding::Utf16Be, 2);
        }

        let mut even_zeros = 0usize;
        let mut odd_zeros = 0usize;
        for (idx, byte) in bytes.iter().enumerate() {
            if *byte == 0 {
                if idx % 2 == 0 {
                    even_zeros += 1;
                } else {
                    odd_zeros += 1;
                }
            }
        }

        if odd_zeros > even_zeros * 2 {
            (TextEncoding::Utf16Le, 0)
        } else if even_zeros > odd_zeros * 2 {
            (TextEncoding::Utf16Be, 0)
        } else {
            (TextEncoding::Utf8, 0)
        }
    }

    fn read_text_with_encoding(path: &Path) -> Result<(String, LineEnding, TextEncoding), String> {
        let bytes = fs::read(path).map_err(|e| e.to_string())?;
        let (encoding, bom_len) = Self::detect_encoding(&bytes);
        let text = Self::decode_bytes(&bytes[bom_len..], encoding)?;
        let (normalized, line_ending) = Self::normalize_line_endings(&text);
        Ok((normalized, line_ending, encoding))
    }

    fn write_text_with_encoding(
        &self,
        path: &Path,
        contents: &str,
        encoding: TextEncoding,
    ) -> std::io::Result<()> {
        let contents = self.apply_line_ending(contents);
        match encoding {
            TextEncoding::Utf8 => fs::write(path, contents.as_bytes()),
            TextEncoding::Utf16Le => {
                let mut bytes = Vec::with_capacity(contents.len() * 2 + 2);
                bytes.extend_from_slice(&[0xFF, 0xFE]);
                for unit in contents.encode_utf16() {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                }
                fs::write(path, bytes)
            }
            TextEncoding::Utf16Be => {
                let mut bytes = Vec::with_capacity(contents.len() * 2 + 2);
                bytes.extend_from_slice(&[0xFE, 0xFF]);
                for unit in contents.encode_utf16() {
                    bytes.extend_from_slice(&unit.to_be_bytes());
                }
                fs::write(path, bytes)
            }
        }
    }

    /// Start a new, empty document.
    fn do_new_file(&mut self) {
        self.text.clear();
        self.saved_snapshot.clear();
        self.file_path = None;
        self.line_ending = LineEnding::Lf;
        self.last_file_mtime = None;
        self.clear_error();
        self.request_editor_focus = true;
        self.reset_editor_state = true;
    }

    /// Open a file chosen by the user and load it into the buffer.
    fn do_open_file(&mut self) {
        self.clear_error();

        let picked = self.pick_open_file();

        let Some(path) = picked else {
            // User canceled.
            return;
        };

        match Self::read_text_with_encoding(&path) {
            Ok((normalized, line_ending, encoding)) => {
                self.text = normalized;
                self.saved_snapshot = self.text.clone();
                self.line_ending = line_ending;
                self.encoding = encoding;
                self.push_recent(&path);
                self.file_path = Some(path);
                self.last_file_mtime = Self::read_file_mtime(&self.file_path);
                self.request_editor_focus = true;
                self.reset_editor_state = true;
            }
            Err(e) => self.set_error(format!("Failed to open file: {e}")),
        }
    }

    /// Open a specific file path (e.g. from drag-and-drop).
    fn do_open_path(&mut self, path: PathBuf) {
        self.clear_error();

        match Self::read_text_with_encoding(&path) {
            Ok((normalized, line_ending, encoding)) => {
                self.text = normalized;
                self.saved_snapshot = self.text.clone();
                self.line_ending = line_ending;
                self.encoding = encoding;
                self.push_recent(&path);
                self.file_path = Some(path);
                self.last_file_mtime = Self::read_file_mtime(&self.file_path);
                self.request_editor_focus = true;
                self.reset_editor_state = true;
            }
            Err(e) => self.set_error(format!("Failed to open file: {e}")),
        }
    }

    fn open_in_new_window(&mut self, path: PathBuf) {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                self.set_error(format!("Failed to launch new window: {err}"));
                return;
            }
        };
        if let Err(err) = std::process::Command::new(exe).arg(path).spawn() {
            self.set_error(format!("Failed to launch new window: {err}"));
        }
    }

    fn pick_open_file(&self) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .add_filter("Text Files", &["txt", "log", "md", "rst", "adoc"])
            .add_filter("Config & Data", &["json", "yaml", "yml", "toml", "ini", "cfg", "ron"])
            .add_filter(
                "Source Code",
                &["rs", "c", "cpp", "cs", "java", "py", "js", "ts", "go"],
            )
            .add_filter("Web Files", &["html", "css", "js", "ts"])
            .add_filter("Structured Data", &["csv", "xml"])
            .add_filter("All Files", &["*"])
            .pick_file()
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

        if let Err(e) = self.write_text_with_encoding(&path, &self.text, self.encoding) {
            self.set_error(format!("Failed to save file: {e}"));
            return;
        }

        // Only mark "clean" after a successful write.
        self.saved_snapshot = self.text.clone();
        self.last_file_mtime = Self::read_file_mtime(&self.file_path);
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

        if let Err(e) = self.write_text_with_encoding(&path, &self.text, self.encoding) {
            self.set_error(format!("Failed to save file: {e}"));
            return;
        }

        self.file_path = Some(path);
        self.saved_snapshot = self.text.clone();
        self.last_file_mtime = Self::read_file_mtime(&self.file_path);
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

    fn read_file_mtime(path: &Option<PathBuf>) -> Option<SystemTime> {
        let path = path.as_ref()?;
        fs::metadata(path).and_then(|m| m.modified()).ok()
    }

    fn check_external_change(&mut self) {
        if self.file_change_disabled || !self.settings.watch_file_changes {
            return;
        }
        let Some(path) = &self.file_path else {
            return;
        };
        let Ok(metadata) = fs::metadata(path) else {
            return;
        };
        let Ok(modified) = metadata.modified() else {
            return;
        };
        if let Some(last) = self.last_file_mtime {
            if modified > last {
                self.file_change_prompt_open = true;
            }
        } else {
            self.last_file_mtime = Some(modified);
        }
    }

    fn reload_from_disk(&mut self) {
        let Some(path) = &self.file_path else {
            return;
        };
        match Self::read_text_with_encoding(path) {
            Ok((normalized, line_ending, encoding)) => {
                self.text = normalized;
                self.saved_snapshot = self.text.clone();
                self.line_ending = line_ending;
                self.encoding = encoding;
                self.last_file_mtime = Self::read_file_mtime(&self.file_path);
                self.request_editor_focus = true;
                self.reset_editor_state = true;
            }
            Err(e) => self.set_error(format!("Failed to reload file: {e}")),
        }
    }

    fn apply_encoding_selection(&mut self, encoding: TextEncoding) {
        if self.is_dirty() {
            self.encoding = encoding;
            self.set_error("Unsaved changes: encoding set for next save.");
            return;
        }
        let Some(path) = &self.file_path else {
            self.encoding = encoding;
            return;
        };
        let mut bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.set_error(format!("Failed to reload file: {e}"));
                return;
            }
        };
        if matches!(encoding, TextEncoding::Utf8)
            && bytes.starts_with(&[0xEF, 0xBB, 0xBF])
        {
            bytes.drain(0..3);
        } else if matches!(encoding, TextEncoding::Utf16Le)
            && bytes.starts_with(&[0xFF, 0xFE])
        {
            bytes.drain(0..2);
        } else if matches!(encoding, TextEncoding::Utf16Be)
            && bytes.starts_with(&[0xFE, 0xFF])
        {
            bytes.drain(0..2);
        }
        let text = match Self::decode_bytes(&bytes, encoding) {
            Ok(text) => text,
            Err(e) => {
                self.set_error(format!("Failed to reload file: {e}"));
                return;
            }
        };
        let (normalized, line_ending) = Self::normalize_line_endings(&text);
        self.text = normalized;
        self.saved_snapshot = self.text.clone();
        self.line_ending = line_ending;
        self.encoding = encoding;
        self.last_file_mtime = Self::read_file_mtime(&self.file_path);
        self.request_editor_focus = true;
        self.reset_editor_state = true;
    }

    fn start_update_check(&mut self, ctx: egui::Context) {
        if self.update_check_in_flight {
            return;
        }
        self.update_check_in_flight = true;
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        thread::spawn(move || {
            let result = check_for_update();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn poll_update_check(&mut self) {
        let Some(rx) = &self.update_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.update_check_in_flight = false;
                self.update_available = result.available;
                self.update_available_url = result.url;
                self.update_rx = None;
                if let Some(err) = result.error {
                    self.set_error(err);
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.update_check_in_flight = false;
                self.update_rx = None;
            }
        }
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
        let encoding = self.encoding.label();

        let selection_text = if selection_len > 0 {
            format!("  |  Sel: {selection_len}")
        } else {
            String::new()
        };

        let update_text = if let Some(version) = &self.update_available {
            format!("  |  New Verison Available: v{version}")
        } else {
            String::new()
        };

        format!(
            "{name}{dirty}  |  Ln {line}, Col {col}  |  Lines: {lines}  Chars: {chars}  |  Size: {size_text}  |  {line_ending}  |  {encoding}{selection_text}"
        )
        + &update_text
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
        highlight_mode: HighlightMode,
    ) -> impl FnMut(&egui::Ui, &str, f32) -> std::sync::Arc<egui::Galley> {
        move |ui, text, _wrap_width| {
            let max_width = if wrap { wrap_width } else { f32::INFINITY };
            match highlight_mode {
                HighlightMode::Markdown => {
                    let job =
                        Self::build_markdown_highlight_job(text, &font_id, max_width, ui.visuals());
                    ui.fonts(|f| f.layout_job(job))
                }
                HighlightMode::Syntax => {
                    let job =
                        Self::build_syntax_highlight_job(text, &font_id, max_width, ui.visuals());
                    ui.fonts(|f| f.layout_job(job))
                }
                HighlightMode::Off => {
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
        }
    }

    fn is_ident_start(ch: char) -> bool {
        ch == '_' || ch.is_ascii_alphabetic()
    }

    fn is_ident_char(ch: char) -> bool {
        Self::is_ident_start(ch) || ch.is_ascii_digit()
    }

    fn is_keyword(token: &str) -> bool {
        matches!(
            token,
                "as"
                | "async"
                | "await"
                | "break"
                | "const"
                | "continue"
                | "crate"
                | "dyn"
                | "else"
                | "enum"
                | "extern"
                | "false"
                | "fn"
                | "for"
                | "from"
                | "if"
                | "import"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pass"
                | "pub"
                | "ref"
                | "return"
                | "self"
                | "Self"
                | "static"
                | "struct"
                | "super"
                | "trait"
                | "try"
                | "except"
                | "finally"
                | "true"
                | "type"
                | "unsafe"
                | "use"
                | "where"
                | "while"
                | "def"
        )
    }

    fn build_syntax_highlight_job(
        text: &str,
        font_id: &egui::FontId,
        wrap_width: f32,
        visuals: &egui::Visuals,
    ) -> egui::text::LayoutJob {
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = wrap_width;

        let normal = egui::TextFormat {
            font_id: font_id.clone(),
            color: visuals.text_color(),
            ..Default::default()
        };
        let comment = egui::TextFormat {
            font_id: font_id.clone(),
            color: egui::Color32::from_gray(140),
            ..Default::default()
        };
        let string = egui::TextFormat {
            font_id: font_id.clone(),
            color: egui::Color32::from_rgb(160, 220, 160),
            ..Default::default()
        };
        let keyword = egui::TextFormat {
            font_id: font_id.clone(),
            color: egui::Color32::from_rgb(120, 170, 255),
            ..Default::default()
        };
        let number = egui::TextFormat {
            font_id: font_id.clone(),
            color: egui::Color32::from_rgb(220, 200, 140),
            ..Default::default()
        };

        let mut idx = 0;
        while idx < text.len() {
            let rest = &text[idx..];
            if rest.starts_with("//") || rest.starts_with('#') {
                let end = rest.find('\n').map(|p| idx + p).unwrap_or(text.len());
                job.append(&text[idx..end], 0.0, comment.clone());
                idx = end;
                continue;
            }

            if rest.starts_with('"') {
                let mut end = idx + 1;
                let mut escape = false;
                while end < text.len() {
                    let ch = text[end..].chars().next().unwrap();
                    let ch_len = ch.len_utf8();
                    if escape {
                        escape = false;
                        end += ch_len;
                        continue;
                    }
                    if ch == '\\' {
                        escape = true;
                        end += ch_len;
                        continue;
                    }
                    end += ch_len;
                    if ch == '"' {
                        break;
                    }
                }
                job.append(&text[idx..end], 0.0, string.clone());
                idx = end;
                continue;
            }

            let ch = rest.chars().next().unwrap();
            if Self::is_ident_start(ch) {
                let mut end = idx + ch.len_utf8();
                while end < text.len() {
                    let next = text[end..].chars().next().unwrap();
                    if !Self::is_ident_char(next) {
                        break;
                    }
                    end += next.len_utf8();
                }
                let token = &text[idx..end];
                if Self::is_keyword(token) {
                    job.append(token, 0.0, keyword.clone());
                } else {
                    job.append(token, 0.0, normal.clone());
                }
                idx = end;
                continue;
            }

            if ch.is_ascii_digit() {
                let mut end = idx + ch.len_utf8();
                while end < text.len() {
                    let next = text[end..].chars().next().unwrap();
                    if !(next.is_ascii_digit() || next == '.') {
                        break;
                    }
                    end += next.len_utf8();
                }
                job.append(&text[idx..end], 0.0, number.clone());
                idx = end;
                continue;
            }

            let ch_len = ch.len_utf8();
            job.append(&text[idx..idx + ch_len], 0.0, normal.clone());
            idx += ch_len;
        }

        job
    }

    fn build_markdown_highlight_job(
        text: &str,
        font_id: &egui::FontId,
        wrap_width: f32,
        visuals: &egui::Visuals,
    ) -> egui::text::LayoutJob {
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = wrap_width;

        let normal = egui::TextFormat {
            font_id: font_id.clone(),
            color: visuals.text_color(),
            ..Default::default()
        };

        let mut bold = normal.clone();
        bold.extra_letter_spacing = 1.0;
        bold.color = egui::Color32::from_rgb(235, 235, 235);
        let mut italics = normal.clone();
        let mut underline = normal.clone();
        italics.italics = true;
        underline.underline = egui::Stroke::new(1.0, underline.color);

        let mut h1 = bold.clone();
        h1.color = egui::Color32::from_rgb(160, 220, 160);
        h1.underline = egui::Stroke::new(1.0, h1.color);

        let mut h2 = bold.clone();
        h2.color = egui::Color32::from_rgb(220, 200, 140);

        let mut h3 = italics.clone();
        h3.color = egui::Color32::from_rgb(120, 170, 255);

        let mut code = normal.clone();
        code.color = egui::Color32::from_rgb(140, 170, 210);

        let mut in_code_fence = false;
        for line in text.split_inclusive('\n') {
            let trimmed = line.trim_start();
            let is_fence = trimmed.starts_with("```");
            if is_fence {
                job.append(line, 0.0, code.clone());
                in_code_fence = !in_code_fence;
                continue;
            }

            if in_code_fence {
                job.append(line, 0.0, code.clone());
                continue;
            }

            let line_ends_with_newline = line.ends_with('\n');
            let content = line.strip_suffix('\n').unwrap_or(line);
            let trimmed_content = content.trim_start();

            let heading_level = trimmed_content
                .chars()
                .take_while(|c| *c == '#')
                .count();
            if heading_level >= 1 && heading_level <= 3 {
                let heading_text = trimmed_content
                    .trim_start_matches('#')
                    .trim_start();
                let heading_format = match heading_level {
                    1 => &h1,
                    2 => &h2,
                    _ => &h3,
                };
                job.append(heading_text, 0.0, heading_format.clone());
                if line_ends_with_newline {
                    job.append("\n", 0.0, normal.clone());
                }
                continue;
            }

            let mut idx = 0;
            while idx < content.len() {
                let rest = &content[idx..];

                if rest.starts_with('`') {
                    let end_rel = rest[1..].find('`').map(|p| p + 2);
                    if let Some(end_rel) = end_rel {
                        let end = idx + end_rel;
                        let code_text = &content[idx + 1..end - 1];
                        job.append("`", 0.0, code.clone());
                        job.append(code_text, 0.0, code.clone());
                        job.append("`", 0.0, code.clone());
                        idx = end;
                        continue;
                    }
                }

                if rest.starts_with("**") {
                    let end_rel = rest[2..].find("**").map(|p| p + 4);
                    if let Some(end_rel) = end_rel {
                        let end = idx + end_rel;
                        let bold_text = &content[idx + 2..end - 2];
                        job.append(bold_text, 0.0, bold.clone());
                        idx = end;
                        continue;
                    }
                }

                if rest.starts_with("__") {
                    let end_rel = rest[2..].find("__").map(|p| p + 4);
                    if let Some(end_rel) = end_rel {
                        let end = idx + end_rel;
                        let underline_text = &content[idx + 2..end - 2];
                        job.append(underline_text, 0.0, underline.clone());
                        idx = end;
                        continue;
                    }
                }

                if rest.starts_with('*') {
                    let end_rel = rest[1..].find('*').map(|p| p + 2);
                    if let Some(end_rel) = end_rel {
                        let end = idx + end_rel;
                        let italic_text = &content[idx + 1..end - 1];
                        job.append(italic_text, 0.0, italics.clone());
                        idx = end;
                        continue;
                    }
                }

                if rest.starts_with('_') {
                    let end_rel = rest[1..].find('_').map(|p| p + 2);
                    if let Some(end_rel) = end_rel {
                        let end = idx + end_rel;
                        let italic_text = &content[idx + 1..end - 1];
                        job.append(italic_text, 0.0, italics.clone());
                        idx = end;
                        continue;
                    }
                }

                let ch = rest.chars().next().unwrap();
                let ch_len = ch.len_utf8();
                job.append(&content[idx..idx + ch_len], 0.0, normal.clone());
                idx += ch_len;
            }

            if line_ends_with_newline {
                job.append("\n", 0.0, normal.clone());
            }
        }

        job
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

    fn populate_find_from_selection(&mut self, ctx: &egui::Context) {
        let Some((start, end)) = self.current_selection_char_range(ctx) else {
            return;
        };
        if start >= end {
            return;
        }
        let selected = egui::text_selection::text_cursor_state::slice_char_range(
            &self.text,
            start..end,
        );
        self.find_state.query = selected.to_string();
        self.find_state.highlight_all = false;
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
        self.find_state.scroll_to_match = true;
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
        self.find_state.scroll_to_match = true;
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
        self.find_state.scroll_to_match = true;
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
        self.find_state.scroll_to_match = true;
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

    fn copy_to_clipboard(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Err(e) = Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text.to_string())) {
            self.set_error(format!("Clipboard copy failed: {e}"));
        }
    }

    fn transform_selection(
        &mut self,
        ctx: &egui::Context,
        range: egui::text::CCursorRange,
        transform: impl FnOnce(&str) -> String,
    ) {
        let editor_id = egui::Id::new("editor");
        let [min, max] = range.sorted();
        let start = min.index;
        let end = max.index;
        if end <= start {
            return;
        }

        let start_byte = Self::char_index_to_byte(&self.text, start);
        let end_byte = Self::char_index_to_byte(&self.text, end);
        let replacement = transform(&self.text[start_byte..end_byte]);
        self.text.replace_range(start_byte..end_byte, &replacement);

        let new_len = replacement.chars().count();
        let new_range = egui::text::CCursorRange::two(
            egui::text::CCursor::new(start),
            egui::text::CCursor::new(start + new_len),
        );
        self.set_editor_selection(ctx, editor_id, new_range);
    }

    fn read_clipboard_text(&mut self) -> Option<String> {
        match Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
            Ok(text) if !text.is_empty() => Some(text),
            Ok(_) => None,
            Err(e) => {
                self.set_error(format!("Clipboard paste failed: {e}"));
                None
            }
        }
    }

    fn char_index_to_byte(text: &str, char_index: usize) -> usize {
        if char_index == 0 {
            return 0;
        }
        let mut count = 0usize;
        for (byte_idx, _) in text.char_indices() {
            if count == char_index {
                return byte_idx;
            }
            count += 1;
        }
        text.len()
    }

    fn set_editor_selection(
        &mut self,
        ctx: &egui::Context,
        editor_id: egui::Id,
        range: egui::text::CCursorRange,
    ) {
        let mut state = egui::TextEdit::load_state(ctx, editor_id).unwrap_or_default();
        state.cursor.set_char_range(Some(range));
        egui::TextEdit::store_state(ctx, editor_id, state);
        self.editor_context_menu_selection = Some(range);
        ctx.memory_mut(|mem| mem.request_focus(editor_id));
    }

    fn editor_action_menu(&mut self, ui: &mut egui::Ui) -> Option<EditorAction> {
        if ui.button("Copy (Ctrl+C)").clicked() {
            return Some(EditorAction::Copy);
        }

        if ui.button("Cut (Ctrl+X)").clicked() {
            return Some(EditorAction::Cut);
        }

        if ui.button("Paste (Ctrl+V)").clicked() {
            return Some(EditorAction::Paste);
        }

        if ui.button("Select All (Ctrl+A)").clicked() {
            return Some(EditorAction::SelectAll);
        }

        None
    }

    fn perform_editor_action(
        &mut self,
        ctx: &egui::Context,
        action: EditorAction,
        selection: Option<egui::text::CCursorRange>,
        allow_fallback: bool,
    ) {
        let editor_id = egui::Id::new("editor");
        if let Some(range) = selection {
            let [min, max] = range.sorted();
            let start = min.index;
            let end = max.index;
            match action {
                EditorAction::Copy => {
                    if end > start {
                        let start_byte = Self::char_index_to_byte(&self.text, start);
                        let end_byte = Self::char_index_to_byte(&self.text, end);
                        let selected = self.text[start_byte..end_byte].to_string();
                        self.copy_to_clipboard(&selected);
                    }
                }
                EditorAction::Cut => {
                    if end > start {
                        let start_byte = Self::char_index_to_byte(&self.text, start);
                        let end_byte = Self::char_index_to_byte(&self.text, end);
                        let selected = self.text[start_byte..end_byte].to_string();
                        self.copy_to_clipboard(&selected);
                        self.text.replace_range(start_byte..end_byte, "");
                        let cursor = egui::text::CCursor::new(start);
                        let new_range = egui::text::CCursorRange::one(cursor);
                        self.set_editor_selection(ctx, editor_id, new_range);
                    }
                }
                EditorAction::Paste => {
                    if let Some(insert) = self.read_clipboard_text() {
                        let start_byte = Self::char_index_to_byte(&self.text, start);
                        let end_byte = Self::char_index_to_byte(&self.text, end);
                        self.text.replace_range(start_byte..end_byte, &insert);
                        let insert_chars = insert.chars().count();
                        let cursor = egui::text::CCursor::new(start + insert_chars);
                        let new_range = egui::text::CCursorRange::one(cursor);
                        self.set_editor_selection(ctx, editor_id, new_range);
                    }
                }
                EditorAction::SelectAll => {
                    let len = self.text.chars().count();
                    let new_range = egui::text::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        egui::text::CCursor::new(len),
                    );
                    self.set_editor_selection(ctx, editor_id, new_range);
                }
                EditorAction::Uppercase => self.transform_selection(ctx, range, |text| {
                    text.chars().flat_map(|ch| ch.to_uppercase()).collect()
                }),
                EditorAction::Lowercase => self.transform_selection(ctx, range, |text| {
                    text.chars().flat_map(|ch| ch.to_lowercase()).collect()
                }),
                EditorAction::Propercase => self.transform_selection(ctx, range, |text| {
                    let mut out = String::with_capacity(text.len());
                    let mut new_word = true;
                    for ch in text.chars() {
                        if ch.is_alphabetic() {
                            if new_word {
                                out.extend(ch.to_uppercase());
                            } else {
                                out.extend(ch.to_lowercase());
                            }
                            new_word = false;
                        } else {
                            out.push(ch);
                            new_word = true;
                        }
                    }
                    out
                }),
            }
            return;
        }

        if !allow_fallback {
            match action {
                EditorAction::Copy | EditorAction::Cut => return,
                EditorAction::Paste => self.paste_from_clipboard(ctx),
            EditorAction::SelectAll => {
                let len = self.text.chars().count();
                let new_range = egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(len),
                );
                self.set_editor_selection(ctx, editor_id, new_range);
            }
            EditorAction::Uppercase
            | EditorAction::Lowercase
            | EditorAction::Propercase => {}
        }
        return;
    }

        match action {
            EditorAction::Copy => self.send_editor_event(ctx, egui::Event::Copy),
            EditorAction::Cut => self.send_editor_event(ctx, egui::Event::Cut),
            EditorAction::Paste => self.paste_from_clipboard(ctx),
            EditorAction::SelectAll => self.send_editor_event(
                ctx,
                egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::COMMAND,
                },
            ),
            EditorAction::Uppercase
            | EditorAction::Lowercase
            | EditorAction::Propercase => {}
        }
    }

    fn show_editor_context_menu(&mut self, ctx: &egui::Context) {
        if !self.editor_context_menu_open {
            return;
        }

        ctx.input_mut(|i| {
            i.events.retain(|event| !matches!(event, egui::Event::Key { .. } | egui::Event::Text(_)));
        });

        let Some(pos) = self.editor_context_menu_pos else {
            self.editor_context_menu_open = false;
            self.editor_context_menu_selection = None;
            return;
        };

        let mut close_menu = false;
        let items: [(&str, EditorAction); 7] = [
            ("Copy (Ctrl+C)", EditorAction::Copy),
            ("Cut (Ctrl+X)", EditorAction::Cut),
            ("Paste (Ctrl+V)", EditorAction::Paste),
            ("Select All (Ctrl+A)", EditorAction::SelectAll),
            ("UPPERCASE", EditorAction::Uppercase),
            ("lowercase", EditorAction::Lowercase),
            ("Propercase", EditorAction::Propercase),
        ];
        let area_response = egui::Area::new(egui::Id::new("editor_context_menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .interactable(true)
            .sense(egui::Sense::hover())
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style())
                    .show(ui, |ui| {
                        ui.set_max_width(ui.spacing().menu_width);
                        ui.with_layout(
                            egui::Layout::top_down_justified(egui::Align::LEFT),
                            |ui| {
                                for (idx, (label, action)) in items.iter().enumerate() {
                                    if idx == 4 {
                                        ui.separator();
                                    }
                                    let selected = idx == self.editor_context_menu_index;
                                    let response =
                                        ui.add(egui::SelectableLabel::new(selected, *label));
                                    if response.hovered() {
                                        self.editor_context_menu_index = idx;
                                    }
                                    if response.clicked() {
                                        self.pending_editor_action = Some(*action);
                                        close_menu = true;
                                    }
                                }
                            },
                        );
                    })
                    .response
            });

        let menu_rect = area_response.response.rect;
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close_menu = true;
        }

        if ctx.input(|i| i.pointer.any_pressed()) {
            if let Some(pointer_pos) = ctx.input(|i| i.pointer.interact_pos()) {
                if !menu_rect.contains(pointer_pos) {
                    close_menu = true;
                }
            }
        }

        if close_menu {
            self.editor_context_menu_open = false;
            self.editor_context_menu_pos = None;
            if self.pending_editor_action.is_none() {
                self.editor_context_menu_selection = None;
            }
        }
    }
}

impl eframe::App for ScratchpadApp {
    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        let _ = save_config_settings(&self.settings);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_font_settings(ctx);
        self.poll_update_check();
        if self.editor_context_menu_open {
            ctx.input_mut(|i| {
                i.events.retain(|event| {
                    !matches!(event, egui::Event::Key { .. } | egui::Event::Text(_))
                });
            });
        }
        if let Some(action) = self.pending_editor_action.take() {
            self.perform_editor_action(ctx, action, self.editor_context_menu_selection, false);
            if !self.editor_context_menu_open {
                self.editor_context_menu_selection = None;
            }
        }
        let mut open_find = false;
        let mut open_replace = false;
        if !self.editor_context_menu_open {
            ctx.input_mut(|i| {
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::F) {
                    open_find = true;
                }
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::R) {
                    open_replace = true;
                }
            });
        }
        if open_find {
            self.find_state.open = true;
            self.find_state.show_replace = false;
            self.populate_find_from_selection(ctx);
            self.find_state.request_focus = true;
        }
        if open_replace {
            self.find_state.open = true;
            self.find_state.show_replace = true;
            self.populate_find_from_selection(ctx);
            self.find_state.request_focus = true;
        }
        self.check_external_change();
        let mut hotkey_new = false;
        let mut hotkey_open = false;
        let mut hotkey_save = false;
        let mut hotkey_save_as = false;
        let mut hotkey_cycle_highlighting = false;
        let mut font_step = 0.0;
        if !self.editor_context_menu_open {
            ctx.input_mut(|i| {
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::N) {
                    hotkey_new = true;
                }
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::O) {
                    hotkey_open = true;
                }
                if i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::S) {
                    hotkey_save_as = true;
                } else if i.consume_key(egui::Modifiers::COMMAND, egui::Key::S) {
                    hotkey_save = true;
                }
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::H) {
                    hotkey_cycle_highlighting = true;
                }
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::Plus)
                    || i.consume_key(egui::Modifiers::COMMAND, egui::Key::Equals)
                {
                    font_step = 0.5;
                } else if i.consume_key(egui::Modifiers::COMMAND, egui::Key::Minus) {
                    font_step = -0.5;
                }
            });
        }
        if hotkey_new {
            self.handle_command(PendingAction::NewFile);
        }
        if hotkey_open {
            self.handle_command(PendingAction::OpenFile);
        }
        if hotkey_save_as {
            self.do_save_as();
        } else if hotkey_save {
            self.do_save();
        }
        if hotkey_cycle_highlighting {
            self.settings.highlight_mode = match self.settings.highlight_mode {
                HighlightMode::Off => HighlightMode::Syntax,
                HighlightMode::Syntax => HighlightMode::Markdown,
                HighlightMode::Markdown => HighlightMode::Off,
            };
        }
        if font_step != 0.0 {
            let updated = (self.settings.font_size + font_step)
                .clamp(10.0, 24.0);
            self.settings.font_size = (updated * 2.0).round() / 2.0;
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

                    if ui.button("Open in New Window...").clicked() {
                        ui.close_menu();
                        if let Some(path) = self.pick_open_file() {
                            self.open_in_new_window(path);
                        }
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
                        .add_enabled(save_enabled, egui::Button::new("Save (Ctrl+S)"))
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
                    if let Some(action) = self.editor_action_menu(ui) {
                        self.perform_editor_action(ctx, action, None, true);
                        ui.close_menu();
                    }

                    ui.separator();
                    if ui.button("Find (Ctrl+F)").clicked() {
                        ui.close_menu();
                        self.find_state.open = true;
                        self.find_state.show_replace = false;
                        self.find_state.request_focus = true;
                    }
                    if ui.button("Find and Replace (Ctrl+R)").clicked() {
                        ui.close_menu();
                        self.find_state.open = true;
                        self.find_state.show_replace = true;
                        self.find_state.request_focus = true;
                    }

                    ui.separator();
                    let watch_changed = ui
                        .checkbox(
                            &mut self.settings.watch_file_changes,
                            "Monitor File for Changes",
                        )
                        .changed();
                    if watch_changed {
                        self.file_change_disabled = !self.settings.watch_file_changes;
                        if self.settings.watch_file_changes {
                            self.last_file_mtime = Self::read_file_mtime(&self.file_path);
                        }
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
                    ui.separator();
                    ui.label("Highlighting");
                    changed |= ui
                        .radio_value(&mut self.settings.highlight_mode, HighlightMode::Off, "Off")
                        .changed();
                    changed |= ui
                        .radio_value(
                            &mut self.settings.highlight_mode,
                            HighlightMode::Syntax,
                            "Syntax",
                        )
                        .changed();
                    changed |= ui
                        .radio_value(
                            &mut self.settings.highlight_mode,
                            HighlightMode::Markdown,
                            "Markdown",
                        )
                        .changed();
                    if changed {
                        self.apply_font_settings(ctx);
                    }
                });

                ui.menu_button("Help", |ui| {
                    if let Some(version) = &self.update_available {
                        if ui.button(format!("Update to v{version}")).clicked() {
                            ui.close_menu();
                            if let Some(url) = &self.update_available_url {
                                ctx.open_url(egui::OpenUrl::new_tab(url));
                            } else {
                                ctx.open_url(egui::OpenUrl::new_tab("https://github.com/samseyller/scratch-pad/releases"));
                            }
                        }
                    }
                    let mut changed = false;
                    changed |= ui
                        .checkbox(&mut self.settings.check_updates, "Check for Updates")
                        .changed();
                    if changed && self.settings.check_updates {
                        self.start_update_check(ctx.clone());
                    } else if changed && !self.settings.check_updates {
                        self.update_available = None;
                    }

                    if ui.button("Releases").clicked() {
                        ui.close_menu();
                        ctx.open_url(egui::OpenUrl::new_tab("https://github.com/samseyller/scratch-pad/releases"));
                    }

                                        if ui.button("About").clicked() {
                        ui.close_menu();
                        self.about_open = true;
                    }
                });

                ui.menu_button("Encoding", |ui| {
                    let mut selected = self.encoding;
                    if ui
                        .radio_value(&mut selected, TextEncoding::Utf8, "UTF-8")
                        .clicked()
                    {
                        ui.close_menu();
                        self.apply_encoding_selection(selected);
                    }
                    if ui
                        .radio_value(&mut selected, TextEncoding::Utf16Le, "UTF-16 LE")
                        .clicked()
                    {
                        ui.close_menu();
                        self.apply_encoding_selection(selected);
                    }
                    if ui
                        .radio_value(&mut selected, TextEncoding::Utf16Be, "UTF-16 BE")
                        .clicked()
                    {
                        ui.close_menu();
                        self.apply_encoding_selection(selected);
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

        if self.about_open {
            let about_width = 500.0;
            egui::Window::new("About Scratchpad")
                .collapsible(false)
                .resizable(false)
                .default_width(about_width)
                .min_width(about_width)
                .max_width(about_width)
                .open(&mut self.about_open)
                .show(ctx, |ui| {
                    let version = if cfg!(debug_assertions) {
                        format!("Scratchpad v{} (dev)", env!("CARGO_PKG_VERSION"))
                    } else {
                        format!("Scratchpad v{}", env!("CARGO_PKG_VERSION"))
                    };
                    ui.label(version);
                    ui.label(format!("Build time: {}", BUILD_TIME));
                    ui.add_space(8.0);
                    ui.label("MIT License");
                    ui.separator();
                    let license_size = 12.0;
                    ui.label(egui::RichText::new("Permission is hereby granted, free of charge, to any person obtaining a copy ").size(license_size));
                    ui.label(egui::RichText::new(r#"of this software and associated documentation files (the "Software"), to deal"#).size(license_size));
                    ui.label(egui::RichText::new("in the Software without restriction, including without limitation the rights").size(license_size));
                    ui.label(egui::RichText::new("to use, copy, modify, merge, publish, distribute, sublicense, and/or sell").size(license_size));
                    ui.label(egui::RichText::new("copies of the Software, and to permit persons to whom the Software is").size(license_size));
                    ui.label(egui::RichText::new("furnished to do so, subject to the following conditions:").size(license_size));
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("The above copyright notice and this permission notice shall be included in all").size(license_size));
                    ui.label(egui::RichText::new("copies or substantial portions of the Software.").size(license_size));
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(r#"THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR"#).size(license_size));
                    ui.label(egui::RichText::new("IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,").size(license_size));
                    ui.label(egui::RichText::new("FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE").size(license_size));
                    ui.label(egui::RichText::new("AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER").size(license_size));
                    ui.label(egui::RichText::new("LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,").size(license_size));
                    ui.label(egui::RichText::new("OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE").size(license_size));
                    ui.label(egui::RichText::new("SOFTWARE.").size(license_size));
                });
        }

        if self.file_change_prompt_open {
            let mut prompt_open = self.file_change_prompt_open;
            let mut action = None;
            egui::Window::new("File changed on disk")
                .collapsible(false)
                .resizable(false)
                .open(&mut prompt_open)
                .show(ctx, |ui| {
                    ui.label("The file has changed outside of Scratchpad.");
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        if ui.button("Reload").clicked() {
                            action = Some("reload");
                        }
                        if ui.button("Ignore").clicked() {
                            action = Some("ignore");
                        }
                        if ui.button("Disable monitoring").clicked() {
                            action = Some("disable");
                        }
                    });
                });
            self.file_change_prompt_open = prompt_open;
            match action {
                Some("reload") => {
                    self.reload_from_disk();
                    self.file_change_prompt_open = false;
                }
                Some("ignore") => {
                    self.last_file_mtime = Self::read_file_mtime(&self.file_path);
                    self.file_change_prompt_open = false;
                }
                Some("disable") => {
                    self.settings.watch_file_changes = false;
                    self.file_change_disabled = true;
                    self.file_change_prompt_open = false;
                }
                _ => {}
            }
        }

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
                        let find_response = ui.add(
                            egui::TextEdit::singleline(&mut self.find_state.query)
                                .desired_width(240.0)
                                .id(egui::Id::new("find_input")),
                        );
                        if self.find_state.request_focus {
                            find_response.request_focus();
                            self.find_state.request_focus = false;
                        }
                        find_changed |= find_response.changed();
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
            if self.editor_context_menu_open || self.pending_editor_action.is_some() {
                if let Some(range) = self.editor_context_menu_selection {
                    let mut state =
                        egui::TextEdit::load_state(ctx, editor_id).unwrap_or_default();
                    state.cursor.set_char_range(Some(range));
                    egui::TextEdit::store_state(ctx, editor_id, state);
                }
            }
            let previous_selection =
                egui::TextEdit::load_state(ctx, editor_id).and_then(|state| state.cursor.char_range());
            let (right_click, right_click_pos) = ctx.input(|i| {
                (i.pointer.secondary_pressed(), i.pointer.interact_pos())
            });
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
                            self.settings.highlight_mode,
                        );
                        let text_edit = egui::TextEdit::multiline(&mut self.text)
                            .id(editor_id)
                            .font(font_id.clone())
                            .layouter(&mut layouter)
                            .lock_focus(true)
                            .desired_width(text_width);

                        let output = ui
                            .allocate_ui_with_layout(
                                egui::vec2(text_width, ui.available_height()),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| text_edit.show(ui),
                            )
                            .inner;

                        if self.find_state.scroll_to_match {
                            if let Some(cursor_range) = output.cursor_range.clone() {
                                let row_height = output
                                    .galley
                                    .rows
                                    .first()
                                    .map(|row| row.height())
                                    .unwrap_or(self.settings.font_size);
                                let cursor_rect = egui::text_selection::text_cursor_state::cursor_rect(
                                    output.galley_pos,
                                    &output.galley,
                                    &cursor_range.primary,
                                    row_height,
                                );
                                ui.scroll_to_rect(cursor_rect, None);
                            }
                            self.find_state.scroll_to_match = false;
                        }

                        if self.find_state.highlight_all {
                            self.paint_find_highlights(ui, &output);
                        }

                        if right_click {
                            if let Some(pos) = right_click_pos {
                                if output.response.interact_rect.contains(pos) {
                                    self.editor_context_menu_open = true;
                                    self.editor_context_menu_pos = Some(pos);
                                    self.editor_context_menu_selection = previous_selection;
                                }
                            }
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

        self.show_editor_context_menu(ctx);

        // Handle file drag-and-drop.
        let dropped_paths: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });

        if let Some(path) = dropped_paths.into_iter().next() {
            if self.file_path.is_some() || !self.text.is_empty() {
                self.open_in_new_window(path);
            } else {
                self.maybe_defer_or_run(PendingAction::OpenPath(path));
            }
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
fn config_path() -> Option<PathBuf> {
    eframe::storage_dir(APP_ID).map(|dir| dir.join("config.ron"))
}

fn load_config_settings() -> Option<AppSettings> {
    let path = config_path()?;
    let contents = std::fs::read_to_string(path).ok()?;
    ron::from_str(&contents).ok()
}

fn save_config_settings(settings: &AppSettings) -> std::io::Result<()> {
    let path = config_path().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "Missing config path"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let pretty = PrettyConfig::new();
    let contents = ron::ser::to_string_pretty(settings, pretty)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    std::fs::write(path, contents)
}

struct UpdateCheckResult {
    available: Option<String>,
    url: Option<String>,
    error: Option<String>,
}

fn check_for_update() -> UpdateCheckResult {
    let url = "https://api.github.com/repos/samseyller/scratch-pad/releases/latest";
    let response = ureq::get(url)
        .set("User-Agent", "scratchpad")
        .call();

    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(code, response)) => {
            let status = response.status_text().to_string();
            return UpdateCheckResult {
                available: None,
                url: None,
                error: Some(format!("Update check failed: {code} {status}")),
            };
        }
        Err(ureq::Error::Transport(err)) => {
            return UpdateCheckResult {
                available: None,
                url: None,
                error: Some(format!(
                    "Update check failed: {}",
                    short_transport_error(&err)
                )),
            };
        }
    };

    let value = match response.into_json::<Value>() {
        Ok(value) => value,
        Err(err) => {
            return UpdateCheckResult {
                available: None,
                url: None,
                error: Some(format!("Update check failed: {err}")),
            };
        }
    };

    let tag = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();

    let Ok(latest) = Version::parse(&tag) else {
        return UpdateCheckResult {
            available: None,
            url: None,
            error: Some("Update check failed.".to_string()),
        };
    };
    let Ok(current) = Version::parse(env!("CARGO_PKG_VERSION")) else {
        return UpdateCheckResult {
            available: None,
            url: None,
            error: None,
        };
    };

    if latest > current {
        UpdateCheckResult {
            available: Some(latest.to_string()),
            url: value.get("html_url").and_then(|v| v.as_str()).map(|s| s.to_string()),
            error: None,
        }
    } else {
        UpdateCheckResult {
            available: None,
            url: None,
            error: None,
        }
    }
}

fn short_transport_error(err: &ureq::Transport) -> String {
    let mut message = err.kind().to_string();
    if let Some(detail) = err.message() {
        message = format!("{message}: {detail}");
    }
    message
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

#![windows_subsystem = "windows"]

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
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
        Box::new(move |cc| Ok(Box::new(ScratchpadApp::new(cc, initial_path.clone())))),
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum ThemeSetting {
    /// Follow the operating system.
    #[default]
    Auto,
    Light,
    Dark,
}

impl ThemeSetting {
    fn preference(self) -> egui::ThemePreference {
        match self {
            ThemeSetting::Auto => egui::ThemePreference::System,
            ThemeSetting::Light => egui::ThemePreference::Light,
            ThemeSetting::Dark => egui::ThemePreference::Dark,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenFilter {
    Text,
    ConfigData,
    SourceCode,
    WebFiles,
    StructuredData,
    AllFiles,
}

impl OpenFilter {
    fn all() -> [(OpenFilter, &'static str, &'static [&'static str]); 6] {
        [
            (OpenFilter::Text, "Text Files", &["txt", "log", "md", "rst", "adoc"]),
            (
                OpenFilter::ConfigData,
                "Config & Data",
                &["json", "yaml", "yml", "toml", "ini", "cfg", "ron"],
            ),
            (
                OpenFilter::SourceCode,
                "Source Code",
                &["rs", "c", "cpp", "cs", "java", "py", "js", "ts", "go"],
            ),
            (OpenFilter::WebFiles, "Web Files", &["html", "css", "js", "ts"]),
            (OpenFilter::StructuredData, "Structured Data", &["csv", "xml"]),
            (OpenFilter::AllFiles, "All Files", &["*"]),
        ]
    }

    fn from_extension(ext: &str) -> Option<Self> {
        Self::all()
            .into_iter()
            .find(|(_, _, exts)| exts.iter().any(|e| e.eq_ignore_ascii_case(ext)))
            .map(|(filter, _, _)| filter)
    }
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

/// Edits offered by the right-click menu and the Edit menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorAction {
    Copy,
    Cut,
    Delete,
    Paste,
    SelectAll,
    Uppercase,
    Lowercase,
    Propercase,
}

const EDITOR_MENU_ITEMS: [(&str, EditorAction); 8] = [
    ("Copy (Ctrl+C)", EditorAction::Copy),
    ("Cut (Ctrl+X)", EditorAction::Cut),
    ("Delete (Del)", EditorAction::Delete),
    ("Paste (Ctrl+V)", EditorAction::Paste),
    ("Select All (Ctrl+A)", EditorAction::SelectAll),
    ("UPPERCASE", EditorAction::Uppercase),
    ("lowercase", EditorAction::Lowercase),
    ("Propercase", EditorAction::Propercase),
];

fn decode_bytes(bytes: &[u8], encoding: TextEncoding) -> Result<String, String> {
    let utf16 = |from: fn([u8; 2]) -> u16| -> Result<String, String> {
        if bytes.len() % 2 != 0 {
            return Err(format!("{} data has an odd number of bytes.", encoding.label()));
        }
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| from([c[0], c[1]])).collect();
        String::from_utf16(&units).map_err(|e| e.to_string())
    };
    match encoding {
        TextEncoding::Utf8 => std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|e| e.to_string()),
        TextEncoding::Utf16Le => utf16(u16::from_le_bytes),
        TextEncoding::Utf16Be => utf16(u16::from_be_bytes),
    }
}

/// Detect the encoding from a byte-order mark or, failing that, from where the zero
/// bytes of mostly-ASCII UTF-16 text fall. Returns the encoding and the BOM length.
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
    let (mut even_zeros, mut odd_zeros) = (0usize, 0usize);
    for (i, b) in bytes.iter().enumerate().take(64 * 1024) {
        if *b == 0 {
            if i % 2 == 0 {
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

/// Encode text for saving. UTF-16 files get a byte-order mark.
fn encode_text(contents: &str, encoding: TextEncoding) -> Vec<u8> {
    match encoding {
        TextEncoding::Utf8 => contents.as_bytes().to_vec(),
        TextEncoding::Utf16Le => {
            let mut out = vec![0xFF, 0xFE];
            out.extend(contents.encode_utf16().flat_map(|u| u.to_le_bytes()));
            out
        }
        TextEncoding::Utf16Be => {
            let mut out = vec![0xFE, 0xFF];
            out.extend(contents.encode_utf16().flat_map(|u| u.to_be_bytes()));
            out
        }
    }
}

fn to_proper_case(text: &str) -> String {
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
}

fn char_index_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
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
    always_on_top: bool,
    check_updates: bool,
    watch_file_changes: bool,
    theme: ThemeSetting,
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
            always_on_top: false,
            check_updates: true,
            watch_file_changes: true,
            theme: ThemeSetting::Auto,
        }
    }
}

/// Above this size the editor keeps far fewer undo points. egui stores a full copy of
/// the text per undo point (up to 100 by default), which would cost gigabytes.
const LARGE_DOC_UNDO_BYTES: usize = 1024 * 1024;
/// Above this size the status bar warns that editing will be slow and memory-hungry.
const HUGE_DOC_BYTES: usize = 5 * 1024 * 1024;
/// Longest line the large-file view draws; the rest is cut off with an ellipsis.
const VIEW_MAX_COLS: usize = 2000;
const LARGE_DOC_MAX_UNDOS: usize = 10;
const DEFAULT_MAX_UNDOS: usize = 100;

/// Above this size the highlighter is skipped (it is O(n) work per edit).
const MAX_HIGHLIGHT_BYTES: usize = 2 * 1024 * 1024;

/// Accent colors for highlighting, chosen per theme.
///
/// The accents are based on the Okabe-Ito colour-blind-safe palette, adjusted so
/// each one has at least WCAG AA contrast (4.5:1) against the editor background in
/// its theme and stays distinguishable under protan/deutan/tritan simulation
/// (both are enforced by unit tests). Meaning is also carried by style (bold,
/// italic, underline), never by colour alone.
#[derive(Clone, Copy, Debug)]
struct HighlightPalette {
    /// De-emphasised text (comments).
    muted: egui::Color32,
    a: egui::Color32,
    b: egui::Color32,
    c: egui::Color32,
    d: egui::Color32,
}

const DARK_PALETTE: HighlightPalette = HighlightPalette {
    muted: egui::Color32::from_rgb(150, 150, 150),
    a: egui::Color32::from_rgb(85, 180, 255),
    b: egui::Color32::from_rgb(230, 118, 0),
    c: egui::Color32::from_rgb(189, 78, 176),
    d: egui::Color32::from_rgb(240, 236, 67),
};

const LIGHT_PALETTE: HighlightPalette = HighlightPalette {
    muted: egui::Color32::from_rgb(96, 96, 96),
    a: egui::Color32::from_rgb(0, 78, 174),
    b: egui::Color32::from_rgb(166, 40, 0),
    c: egui::Color32::from_rgb(124, 38, 115),
    d: egui::Color32::from_rgb(0, 135, 72),
};

impl HighlightPalette {
    fn for_visuals(visuals: &egui::Visuals) -> Self {
        if visuals.dark_mode { DARK_PALETTE } else { LIGHT_PALETTE }
    }
}

/// Append `text[start..end]` (if non-empty) as a single plain-formatted section.
/// Coalescing plain runs keeps the LayoutJob small (it used to get one section
/// per punctuation/whitespace character).
fn flush_plain(
    job: &mut egui::text::LayoutJob,
    text: &str,
    start: usize,
    end: usize,
    format: &egui::TextFormat,
) {
    if start < end {
        job.append(&text[start..end], 0.0, format.clone());
    }
}

/// 1-based (line, column) for a cursor at `char_idx`, without allocating.
fn cursor_line_col(text: &str, char_idx: usize) -> (usize, usize) {
    let byte = text
        .char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let prefix = &text[..byte];
    let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
    let line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
    (line, prefix[line_start..].chars().count() + 1)
}

/// Find all regex matches with byte and char offsets in a single pass
/// (char offsets are accumulated incrementally instead of rescanning the prefix).
/// Byte offset of the start of every line, plus the longest line (in bytes, capped at
/// `VIEW_MAX_COLS`). Used by the large-file view to draw only the visible lines.
struct LineIndex {
    rev: u64,
    starts: Vec<usize>,
    max_cols: usize,
}

fn build_line_starts(text: &str) -> (Vec<usize>, usize) {
    let mut starts = vec![0usize];
    let mut max_len = 0usize;
    let mut prev = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            max_len = max_len.max(i - prev);
            prev = i + 1;
            starts.push(prev);
        }
    }
    max_len = max_len.max(text.len() - prev);
    (starts, max_len.min(VIEW_MAX_COLS))
}

/// Shade `matches` (sorted char ranges) in the visible rows of the editor. This works
/// from the row list (binary search plus a running char count) because
/// Galley::cursor_from_pos / pos_from_cursor walk the whole galley, which costs tens of
/// milliseconds per frame on huge documents.
fn paint_matches(
    ui: &egui::Ui,
    output: &TextEditOutput,
    matches: &[FindMatch],
    color: egui::Color32,
) {
    let clip = ui.clip_rect();
    let origin = output.galley_pos.to_vec2();
    let rows = &output.galley.rows;
    let first_row = rows.partition_point(|r| r.rect().max.y + origin.y < clip.min.y);
    let end_row = rows.partition_point(|r| r.rect().min.y + origin.y <= clip.max.y);
    if first_row >= end_row {
        return;
    }
    let mut row_start: usize = rows[..first_row]
        .iter()
        .map(|r| usize::from(r.char_count_including_newline()))
        .sum();

    let painter = ui.painter_at(output.text_clip_rect);
    for row in &rows[first_row..end_row] {
        let row_end = row_start + usize::from(row.char_count_excluding_newline());
        let next_start = row_start + usize::from(row.char_count_including_newline());
        let first_match = matches.partition_point(|m| m.end_char <= row_start);
        for m in matches[first_match..]
            .iter()
            .take_while(|m| m.start_char < next_start)
        {
            // Clip the match to this row's text (a match can span several rows).
            let s = m.start_char.max(row_start);
            let e = m.end_char.min(row_end);
            if e <= s {
                continue;
            }
            let x0 = row.x_offset((s - row_start).into());
            let x1 = row.x_offset((e - row_start).into());
            let r = row.rect();
            let rect = egui::Rect::from_min_max(
                egui::pos2(r.min.x + x0, r.min.y),
                egui::pos2(r.min.x + x1, r.max.y),
            );
            painter.rect_filled(rect.translate(origin), 0.0, color);
        }
        row_start = next_start;
    }
}

fn find_matches_in(text: &str, regex: &Regex) -> Vec<FindMatch> {
    let mut out = Vec::new();
    let mut last_byte = 0usize;
    let mut last_char = 0usize;
    for m in regex.find_iter(text) {
        let start_char = last_char + text[last_byte..m.start()].chars().count();
        let end_char = start_char + text[m.start()..m.end()].chars().count();
        last_byte = m.end();
        last_char = end_char;
        out.push(FindMatch {
            start_byte: m.start(),
            end_byte: m.end(),
            start_char,
            end_char,
        });
    }
    out
}

/// How often the open file is checked for external modification.
const FILE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

fn should_check_file(last: Option<Instant>, now: Instant, interval: Duration) -> bool {
    match last {
        None => true,
        Some(t) => now.duration_since(t) >= interval,
    }
}

/// Newline and character counts, computed in a single pass over the bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TextStats {
    newlines: usize,
    chars: usize,
}

fn text_stats(text: &str) -> TextStats {
    let mut newlines = 0usize;
    let mut chars = 0usize;
    for &b in text.as_bytes() {
        newlines += (b == b'\n') as usize;
        // Every byte that isn't a UTF-8 continuation byte starts a char.
        chars += ((b & 0xC0) != 0x80) as usize;
    }
    TextStats { newlines, chars }
}

/// Values derived from the text, valid for one `text_rev`.
#[derive(Clone, Copy, Debug)]
struct DerivedText {
    rev: u64,
    dirty: bool,
    stats: TextStats,
}

/// The last galley produced by the editor layouter. Reused while the text and
/// layout inputs are unchanged, so idle frames skip re-highlighting entirely.
struct LayoutCache {
    text: String,
    mode: HighlightMode,
    font: egui::FontId,
    wrap_bits: u32,
    dark: bool,
    ppp_bits: u32,
    galley: Arc<egui::Galley>,
}

/// Cached match list for "highlight all", so it isn't recomputed every frame.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FindCacheKey {
    rev: u64,
    query: String,
    mode: FindMode,
    case_sensitive: bool,
}

struct FindCache {
    key: FindCacheKey,
    matches: Vec<FindMatch>,
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
    /// Char range of the match most recently selected by Find.
    last_found: Option<(usize, usize)>,
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
            last_found: None,
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

    /// Last known modified timestamp for the opened file.
    last_file_mtime: Option<SystemTime>,

    /// If set, show the "Unsaved changes" dialog and then run this action.
    pending_action: Option<PendingAction>,

    /// Request focus for the editor on the next frame (after New/Open).
    request_editor_focus: bool,

    /// Clear editor selection/state on the next frame (after New/Open).
    reset_editor_state: bool,

    /// Current encoding for load/save.
    encoding: TextEncoding,

    /// Last selected filter in the Open dialog (per session).
    last_open_filter: OpenFilter,

    applied_always_on_top: bool,

    /// Selection (possibly just a cursor) as of the last frame the editor had focus.
    /// Menus take focus away from the editor, so actions use this.
    last_selection: Option<egui::text::CCursorRange>,
    /// Selection captured when the right-click menu was opened.
    context_selection: Option<egui::text::CCursorRange>,
    /// Action chosen in the right-click menu, applied at the start of the next frame.
    pending_editor_action: Option<(EditorAction, Option<egui::text::CCursorRange>)>,

    /// Result of the background file-modification check.
    file_mtime_rx: Option<Receiver<(PathBuf, Option<SystemTime>)>>,

    /// Large files open in a read-only view that lays out only the visible lines.
    view_only: bool,
    /// Byte range of the match Find last selected, shown in the large-file view.
    view_match: Option<(usize, usize)>,
    line_index: Option<LineIndex>,
    confirm_edit_open: bool,

    /// Whether the editor's undo history is currently limited for a large document.
    undo_capped: bool,

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

    find_cache: Option<FindCache>,

    /// Bumped whenever `text` or `saved_snapshot` changes. Everything derived from
    /// the text (dirty flag, counts, cursor position, find matches) is cached
    /// against this instead of being recomputed every frame.
    text_rev: u64,
    derived: Cell<Option<DerivedText>>,
    cursor_cache: Cell<Option<(u64, usize, (usize, usize))>>,
    layout_cache: RefCell<Option<LayoutCache>>,

    last_file_check: Option<Instant>,
}

impl Default for ScratchpadApp {
    fn default() -> Self {
        Self {
            text: String::new(),
            file_path: None,
            saved_snapshot: String::new(),
            line_ending: LineEnding::Lf,
            last_file_mtime: None,
            pending_action: None,
            request_editor_focus: true,
            reset_editor_state: false,
            view_only: false,
            encoding: TextEncoding::Utf8,
            last_open_filter: OpenFilter::Text,
            applied_always_on_top: false,
            last_selection: None,
            context_selection: None,
            pending_editor_action: None,
            file_mtime_rx: None,
            view_match: None,
            line_index: None,
            confirm_edit_open: false,
            undo_capped: false,
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
            find_cache: None,
            text_rev: 0,
            derived: Cell::new(None),
            cursor_cache: Cell::new(None),
            layout_cache: RefCell::new(None),
            last_file_check: None,
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
        self.derived().dirty
    }

    /// Record that `text` or `saved_snapshot` changed.
    fn touch(&mut self) {
        self.text_rev = self.text_rev.wrapping_add(1);
    }

    /// Derived text values, recomputed only when the text revision changed.
    fn derived(&self) -> DerivedText {
        if let Some(d) = self.derived.get() {
            if d.rev == self.text_rev {
                return d;
            }
        }
        let d = DerivedText {
            rev: self.text_rev,
            dirty: self.text != self.saved_snapshot,
            stats: text_stats(&self.text),
        };
        self.derived.set(Some(d));
        d
    }

    fn cached_cursor_line_col(&self, char_idx: usize) -> (usize, usize) {
        if let Some((rev, idx, result)) = self.cursor_cache.get() {
            if rev == self.text_rev && idx == char_idx {
                return result;
            }
        }
        let result = cursor_line_col(&self.text, char_idx);
        self.cursor_cache.set(Some((self.text_rev, char_idx, result)));
        result
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

    /// Read a file, detecting its encoding (BOM or UTF-16 heuristics) and line endings.
    fn read_text_with_encoding(path: &Path) -> Result<(String, LineEnding, TextEncoding), String> {
        let bytes = fs::read(path).map_err(|e| e.to_string())?;
        let (encoding, bom_len) = detect_encoding(&bytes);
        let text = decode_bytes(&bytes[bom_len..], encoding)?;
        let (normalized, line_ending) = Self::normalize_line_endings(&text);
        Ok((normalized, line_ending, encoding))
    }

    fn write_text_with_encoding(&self, path: &Path, contents: &str) -> std::io::Result<()> {
        let contents = self.apply_line_ending(contents);
        fs::write(path, encode_text(&contents, self.encoding))
    }

    /// Load a file into the buffer; shared by Open, Open Recent, drop and reload.
    fn load_file_into_buffer(&mut self, path: &Path) -> Result<(), String> {
        let (normalized, line_ending, encoding) = Self::read_text_with_encoding(path)?;
        self.text = normalized;
        self.saved_snapshot = self.text.clone();
        self.touch();
        self.line_ending = line_ending;
        self.encoding = encoding;
        Ok(())
    }

    /// Start a new, empty document.
    fn do_new_file(&mut self) {
        self.text.clear();
        self.saved_snapshot.clear();
        self.touch();
        self.file_path = None;
        self.line_ending = LineEnding::Lf;
        self.encoding = TextEncoding::Utf8;
        self.last_file_mtime = None;
        self.clear_error();
        self.request_editor_focus = true;
        self.reset_editor_state = true;
    }

    /// Open a file chosen by the user and load it into the buffer.
    fn do_open_file(&mut self) {
        self.clear_error();

        let Some(path) = self.pick_open_file() else {
            // User canceled.
            return;
        };

        self.update_last_open_filter(&path);

        match self.load_file_into_buffer(&path) {
            Ok(()) => {
                self.push_recent(&path);
                self.file_path = Some(path);
                self.last_file_mtime = Self::read_file_mtime(&self.file_path);
                self.request_editor_focus = true;
                self.reset_editor_state = true;
            }
            Err(e) => self.set_error(format!("Failed to open file: {e}")),
        }
    }

    /// The Open dialog, with the filter used last time listed first.
    fn pick_open_file(&self) -> Option<PathBuf> {
        let mut dialog = rfd::FileDialog::new();
        for (filter, name, extensions) in OpenFilter::all() {
            if filter == self.last_open_filter {
                dialog = dialog.add_filter(name, extensions);
            }
        }
        for (filter, name, extensions) in OpenFilter::all() {
            if filter != self.last_open_filter {
                dialog = dialog.add_filter(name, extensions);
            }
        }
        dialog.pick_file()
    }

    fn update_last_open_filter(&mut self, path: &Path) {
        self.last_open_filter = path
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(OpenFilter::from_extension)
            .unwrap_or(OpenFilter::AllFiles);
    }

    /// Launch another copy of the app showing `path`.
    fn open_in_new_window(&mut self, path: PathBuf) {
        self.update_last_open_filter(&path);
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

    /// Open a specific file path (e.g. from drag-and-drop).
    fn do_open_path(&mut self, path: PathBuf) {
        self.clear_error();

        self.update_last_open_filter(&path);

        match self.load_file_into_buffer(&path) {
            Ok(()) => {
                self.push_recent(&path);
                self.file_path = Some(path);
                self.last_file_mtime = Self::read_file_mtime(&self.file_path);
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

        if let Err(e) = self.write_text_with_encoding(&path, &self.text) {
            self.set_error(format!("Failed to save file: {e}"));
            return;
        }

        // Only mark "clean" after a successful write.
        self.saved_snapshot = self.text.clone();
        self.touch();
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

        if let Err(e) = self.write_text_with_encoding(&path, &self.text) {
            self.set_error(format!("Failed to save file: {e}"));
            return;
        }

        self.file_path = Some(path);
        self.saved_snapshot = self.text.clone();
        self.touch();
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

    fn check_external_change(&mut self, ctx: &egui::Context) {
        if self.file_change_disabled || !self.settings.watch_file_changes {
            return;
        }
        if self.file_path.is_none() {
            return;
        }
        // Collect the result of the background check started on an earlier frame.
        if let Some(rx) = &self.file_mtime_rx {
            match rx.try_recv() {
                Ok((checked_path, modified)) => {
                    self.file_mtime_rx = None;
                    if Some(&checked_path) == self.file_path.as_ref() {
                        if let Some(modified) = modified {
                            match self.last_file_mtime {
                                Some(last) if modified > last => {
                                    self.file_change_prompt_open = true;
                                }
                                Some(_) => {}
                                None => self.last_file_mtime = Some(modified),
                            }
                        }
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.file_mtime_rx = None,
            }
        }

        // Wake an otherwise idle window periodically so external edits are noticed,
        // but only look at the file once per interval, and off the UI thread so a slow
        // or network drive can't freeze the window.
        ctx.request_repaint_after(FILE_CHECK_INTERVAL);
        if self.file_change_prompt_open || self.file_mtime_rx.is_some() {
            return;
        }
        let now = Instant::now();
        if !should_check_file(self.last_file_check, now, FILE_CHECK_INTERVAL) {
            return;
        }
        self.last_file_check = Some(now);
        let Some(path) = self.file_path.clone() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.file_mtime_rx = Some(rx);
        let repaint = ctx.clone();
        thread::spawn(move || {
            let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();
            let _ = tx.send((path, modified));
            repaint.request_repaint();
        });
    }

    fn reload_from_disk(&mut self) {
        let Some(path) = self.file_path.clone() else {
            return;
        };
        match self.load_file_into_buffer(&path) {
            Ok(()) => {
                self.last_file_mtime = Self::read_file_mtime(&self.file_path);
                self.request_editor_focus = true;
                self.reset_editor_state = true;
            }
            Err(e) => self.set_error(format!("Failed to reload file: {e}")),
        }
    }

    /// Switch the encoding. With a clean file this re-reads it as the chosen encoding
    /// (to fix a wrong guess); with unsaved changes it only sets the encoding for the
    /// next save.
    fn apply_encoding_selection(&mut self, encoding: TextEncoding) {
        if self.is_dirty() || self.file_path.is_none() {
            self.encoding = encoding;
            if self.is_dirty() {
                self.set_error("Unsaved changes: encoding set for next save.");
            }
            return;
        }
        let Some(path) = self.file_path.clone() else {
            return;
        };
        let mut bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.set_error(format!("Failed to reload file: {e}"));
                return;
            }
        };
        let (_, bom_len) = detect_encoding(&bytes);
        let bom_matches = match encoding {
            TextEncoding::Utf8 => bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
            TextEncoding::Utf16Le => bytes.starts_with(&[0xFF, 0xFE]),
            TextEncoding::Utf16Be => bytes.starts_with(&[0xFE, 0xFF]),
        };
        if bom_matches {
            bytes.drain(0..bom_len);
        }
        let text = match decode_bytes(&bytes, encoding) {
            Ok(text) => text,
            Err(e) => {
                self.set_error(format!("Failed to reload file: {e}"));
                return;
            }
        };
        let (normalized, line_ending) = Self::normalize_line_endings(&text);
        self.text = normalized;
        self.saved_snapshot = self.text.clone();
        self.touch();
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

        let stats = self.derived().stats;
        let newlines = stats.newlines;
        let lines = newlines + 1;
        let chars = stats.chars;

        let mut line = 1usize;
        let mut col = 1usize;
        let mut selection_len = 0usize;
        if let Some(state) = egui::TextEdit::load_state(ctx, egui::Id::unique("editor")) {
            if let Some(range) = state.cursor.char_range() {
                (line, col) = self.cached_cursor_line_col(usize::from(range.primary.index));
                let [min, max] = range.sorted_cursors();
                selection_len = usize::from(max.index).saturating_sub(usize::from(min.index));
            }
        }

        // Size on disk: LF text expands by one byte per newline when saved as CRLF.
        let size_bytes = self.text.len()
            + if self.line_ending == LineEnding::CrLf { newlines } else { 0 };
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

        let huge_text = if self.text.len() > HUGE_DOC_BYTES {
            "  |  Large file: editing may be slow and use a lot of memory"
        } else {
            ""
        };

        let update_text = if let Some(version) = &self.update_available {
            format!("  |  New Version Available: v{version}")
        } else {
            String::new()
        };

        format!(
            "{name}{dirty}  |  Ln {line}, Col {col}  |  Lines: {lines}  Chars: {chars}  |  Size: {size_text}  |  {line_ending}  |  {encoding}{selection_text}"
        )
        + huge_text
        + &update_text
    }

    /// Read-only view for very large files: lays out only the visible lines, so opening
    /// and scrolling stay fast and memory stays small. Supports Find (next/previous/all).
    fn show_large_file_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "Large file ({}): read-only view to keep things fast.",
                    format_size(self.text.len())
                ))
                .strong(),
            );
            if ui.button("Edit anyway…").clicked() {
                self.confirm_edit_open = true;
            }
        });
        ui.separator();

        if self.confirm_edit_open {
            let mut open = true;
            egui::Window::new("Edit large file?")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ui.ctx(), |ui| {
                    ui.label(format!(
                        "Editing a {} file loads all of it into the editor. Opening it can \
                         take several seconds, every keystroke may lag, and memory use can \
                         reach several gigabytes.",
                        format_size(self.text.len())
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Edit anyway").clicked() {
                            self.view_only = false;
                            self.confirm_edit_open = false;
                            self.request_editor_focus = true;
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_edit_open = false;
                        }
                    });
                });
            if !open {
                self.confirm_edit_open = false;
            }
            if !self.view_only {
                return;
            }
        }

        // Rebuild the line index and Find matches only when the text changed.
        if self.line_index.as_ref().map(|l| l.rev) != Some(self.text_rev) {
            let (starts, max_cols) = build_line_starts(&self.text);
            self.line_index = Some(LineIndex { rev: self.text_rev, starts, max_cols });
        }
        let show_all = self.find_state.highlight_all && self.ensure_find_cache();

        let font_id = egui::FontId::monospace(self.settings.font_size);
        let text_color = ui.visuals().text_color();
        let number_color = ui.visuals().weak_text_color();
        let dark = ui.visuals().dark_mode;
        let all_color = if dark {
            egui::Color32::from_rgba_unmultiplied(255, 200, 40, 70)
        } else {
            egui::Color32::from_rgba_unmultiplied(255, 190, 0, 90)
        };
        let current_color = if dark {
            egui::Color32::from_rgba_unmultiplied(255, 140, 0, 150)
        } else {
            egui::Color32::from_rgba_unmultiplied(255, 120, 0, 140)
        };
        let (char_w, row_h) = {
            let g = ui.fonts_mut(|f| f.layout_no_wrap("0".to_string(), font_id.clone(), text_color));
            (g.size().x, g.size().y)
        };

        let text = self.text.as_str();
        let Some(index) = self.line_index.as_ref() else {
            return;
        };
        let starts = &index.starts;
        let total_lines = starts.len();
        let gutter = if self.settings.show_line_numbers {
            (total_lines.to_string().len() as f32 + 1.0) * char_w + 8.0
        } else {
            4.0
        };
        let content_w = gutter + (index.max_cols + 1) as f32 * char_w + 16.0;
        let matches: &[FindMatch] = if show_all {
            self.find_cache.as_ref().map_or(&[], |c| &c.matches[..])
        } else {
            &[]
        };
        let current = self.view_match;
        let show_line_numbers = self.settings.show_line_numbers;

        ui.spacing_mut().item_spacing.y = 0.0;
        let mut area = egui::ScrollArea::both().auto_shrink([false, false]);
        let mut scroll_done = false;
        if self.find_state.scroll_to_match {
            if let Some((b, _)) = current {
                let line = starts.partition_point(|&s| s <= b).saturating_sub(1);
                let y = line as f32 * row_h - ui.available_height() / 2.0;
                area = area.vertical_scroll_offset(y.max(0.0));
            }
            scroll_done = true;
        }

        area.show_rows(ui, row_h, total_lines, |ui, range| {
            ui.set_min_width(content_w);
            for line in range {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(content_w, row_h), egui::Sense::hover());
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let start = starts[line];
                let raw_end = starts.get(line + 1).copied().unwrap_or(text.len());
                let full = text[start..raw_end].trim_end_matches(['\n', '\r']);
                let (shown, cut) = match full.char_indices().nth(VIEW_MAX_COLS) {
                    Some((i, _)) => (&full[..i], true),
                    None => (full, false),
                };
                let line_end = start + shown.len();
                let painter = ui.painter();
                let x0 = rect.min.x + gutter;

                let shade = |a: usize, b: usize, color: egui::Color32| {
                    let (a, b) = (a.max(start), b.min(line_end));
                    if b <= a {
                        return;
                    }
                    let c0 = text[start..a].chars().count() as f32;
                    let c1 = text[start..b].chars().count() as f32;
                    painter.rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(x0 + c0 * char_w, rect.min.y),
                            egui::pos2(x0 + c1 * char_w, rect.max.y),
                        ),
                        0.0,
                        color,
                    );
                };
                let first = matches.partition_point(|m| m.end_byte <= start);
                for m in matches[first..].iter().take_while(|m| m.start_byte < line_end) {
                    shade(m.start_byte, m.end_byte, all_color);
                }
                if let Some((a, b)) = current {
                    shade(a, b, current_color);
                }

                if show_line_numbers {
                    painter.text(
                        egui::pos2(rect.min.x + gutter - 6.0, rect.min.y),
                        egui::Align2::RIGHT_TOP,
                        (line + 1).to_string(),
                        font_id.clone(),
                        number_color,
                    );
                }
                let label = if cut { format!("{shown}…") } else { shown.to_string() };
                painter.text(
                    egui::pos2(x0, rect.min.y),
                    egui::Align2::LEFT_TOP,
                    label,
                    font_id.clone(),
                    text_color,
                );
            }
        });
        if scroll_done {
            self.find_state.scroll_to_match = false;
        }
    }

    /// Limit the editor's undo history while the document is large (see
    /// `LARGE_DOC_UNDO_BYTES`). Switching the limit restarts the history.
    fn update_undo_limit(&mut self, ctx: &egui::Context, editor_id: egui::Id) {
        let large = self.text.len() > LARGE_DOC_UNDO_BYTES;
        if large == self.undo_capped {
            return;
        }
        let mut state = egui::TextEdit::load_state(ctx, editor_id).unwrap_or_default();
        state.set_undoer(egui::util::undoer::Undoer::with_settings(
            egui::util::undoer::Settings {
                max_undos: if large { LARGE_DOC_MAX_UNDOS } else { DEFAULT_MAX_UNDOS },
                ..Default::default()
            },
        ));
        state.store(ctx, editor_id);
        self.undo_capped = large;
    }

    fn apply_window_level(&mut self, ctx: &egui::Context) {
        let level = if self.settings.always_on_top {
            egui::viewport::WindowLevel::AlwaysOnTop
        } else {
            egui::viewport::WindowLevel::Normal
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
        self.applied_always_on_top = self.settings.always_on_top;
    }

    fn apply_font_settings(&self, ctx: &egui::Context) {
        let pref = self.settings.theme.preference();
        if ctx.options(|o| o.theme_preference) != pref {
            ctx.set_theme(pref);
        }
        // Called every frame, so bail out cheaply when nothing needs changing.
        if ctx.global_style().text_styles.values().all(|f| f.size == 14.0) {
            return;
        }
        let mut style = (*ctx.global_style()).clone();
        for (_text_style, font_id) in style.text_styles.iter_mut() {
            font_id.size = 14.0;
        }
        ctx.set_global_style(style);
    }

    fn make_editor_layouter<'a>(
        cache: &'a RefCell<Option<LayoutCache>>,
        wrap: bool,
        wrap_width: f32,
        font_id: egui::FontId,
        highlight_mode: HighlightMode,
    ) -> impl FnMut(&egui::Ui, &dyn egui::TextBuffer, f32) -> Arc<egui::Galley> + 'a {
        move |ui, text, _wrap_width| {
            let text = text.as_str();
            let max_width = if wrap { wrap_width } else { f32::INFINITY };
            let dark = ui.visuals().dark_mode;
            let ppp_bits = ui.ctx().pixels_per_point().to_bits();

            // The editor may lay out the text several times per frame (before and
            // after an edit), so the cache is keyed on the full text, not a revision.
            if let Some(c) = cache.borrow().as_ref() {
                if c.mode == highlight_mode
                    && c.font == font_id
                    && c.wrap_bits == max_width.to_bits()
                    && c.dark == dark
                    && c.ppp_bits == ppp_bits
                    && c.text == text
                {
                    return c.galley.clone();
                }
            }

            let job = match highlight_mode {
                HighlightMode::Markdown => {
                    Self::build_markdown_highlight_job(text, &font_id, max_width, ui.visuals())
                }
                HighlightMode::Syntax => {
                    Self::build_syntax_highlight_job(text, &font_id, max_width, ui.visuals())
                }
                HighlightMode::Off => {
                    let mut job = egui::text::LayoutJob::simple(
                        text.to_owned(),
                        font_id.clone(),
                        ui.visuals().text_color(),
                        max_width,
                    );
                    job.wrap.max_width = max_width;
                    job
                }
            };
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            *cache.borrow_mut() = Some(LayoutCache {
                text: text.to_owned(),
                mode: highlight_mode,
                font: font_id.clone(),
                wrap_bits: max_width.to_bits(),
                dark,
                ppp_bits,
                galley: galley.clone(),
            });
            galley
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

        let palette = HighlightPalette::for_visuals(visuals);
        let fmt = |color: egui::Color32| egui::TextFormat {
            font_id: font_id.clone(),
            color,
            ..Default::default()
        };
        let normal = fmt(visuals.text_color());
        let mut comment = fmt(palette.muted);
        comment.italics = true;
        let string = fmt(palette.b);
        let keyword = fmt(palette.a);
        let number = fmt(palette.c);

        let mut plain_start = 0usize;
        let mut idx = 0usize;
        while idx < text.len() {
            let rest = &text[idx..];
            if rest.starts_with("//") || rest.starts_with('#') {
                flush_plain(&mut job, text, plain_start, idx, &normal);
                let end = rest.find('\n').map(|p| idx + p).unwrap_or(text.len());
                job.append(&text[idx..end], 0.0, comment.clone());
                idx = end;
                plain_start = end;
                continue;
            }

            if rest.starts_with('"') {
                flush_plain(&mut job, text, plain_start, idx, &normal);
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
                plain_start = end;
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
                if Self::is_keyword(&text[idx..end]) {
                    flush_plain(&mut job, text, plain_start, idx, &normal);
                    job.append(&text[idx..end], 0.0, keyword.clone());
                    plain_start = end;
                }
                idx = end;
                continue;
            }

            if ch.is_ascii_digit() {
                flush_plain(&mut job, text, plain_start, idx, &normal);
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
                plain_start = end;
                continue;
            }

            idx += ch.len_utf8();
        }
        flush_plain(&mut job, text, plain_start, text.len(), &normal);

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

        let palette = HighlightPalette::for_visuals(visuals);
        let normal = egui::TextFormat {
            font_id: font_id.clone(),
            color: visuals.text_color(),
            ..Default::default()
        };

        // Bold uses the theme's "strong" text colour (white-ish in dark mode,
        // black-ish in light mode) plus wider letter spacing.
        let mut bold = normal.clone();
        bold.extra_letter_spacing = 1.0;
        bold.color = visuals.strong_text_color();
        let mut italics = normal.clone();
        let mut underline = normal.clone();
        italics.italics = true;
        underline.underline = egui::Stroke::new(1.0_f32, underline.color);

        // Headings differ by colour AND style (underline / bold / italic).
        let mut h1 = bold.clone();
        h1.color = palette.a;
        h1.underline = egui::Stroke::new(1.0_f32, h1.color);

        let mut h2 = bold.clone();
        h2.color = palette.b;

        let mut h3 = italics.clone();
        h3.color = palette.c;

        let mut code = normal.clone();
        code.color = palette.d;
        code.background = visuals.code_bg_color;

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
            if (1..=3).contains(&heading_level) {
                let heading_format = match heading_level {
                    1 => &h1,
                    2 => &h2,
                    _ => &h3,
                };
                job.append(content, 0.0, heading_format.clone());
                if line_ends_with_newline {
                    job.append("\n", 0.0, normal.clone());
                }
                continue;
            }

            let mut plain_start = 0usize;
            let mut idx = 0usize;
            while idx < content.len() {
                let rest = &content[idx..];

                let styled: Option<(usize, &egui::TextFormat)> = if rest.starts_with('`') {
                    rest[1..].find('`').map(|p| (p + 2, &code))
                } else if rest.starts_with("**") {
                    rest[2..].find("**").map(|p| (p + 4, &bold))
                } else if rest.starts_with("__") {
                    rest[2..].find("__").map(|p| (p + 4, &underline))
                } else if rest.starts_with('*') {
                    rest[1..].find('*').map(|p| (p + 2, &italics))
                } else if rest.starts_with('_') {
                    rest[1..].find('_').map(|p| (p + 2, &italics))
                } else {
                    None
                };

                if let Some((len, format)) = styled {
                    flush_plain(&mut job, line, plain_start, idx, &normal);
                    job.append(&content[idx..idx + len], 0.0, format.clone());
                    idx += len;
                    plain_start = idx;
                    continue;
                }

                idx += rest.chars().next().unwrap().len_utf8();
            }
            // Plain tail, including the trailing newline (if any).
            flush_plain(&mut job, line, plain_start, line.len(), &normal);
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

    fn current_selection_char_range(&self, ctx: &egui::Context) -> Option<(usize, usize)> {
        if self.view_only {
            return self.find_state.last_found;
        }
        let editor_id = egui::Id::unique("editor");
        let current = egui::TextEdit::load_state(ctx, editor_id)
            .and_then(|state| state.cursor.char_range())
            .map(|range| {
                let [min, max] = range.sorted_cursors();
                (usize::from(min.index), usize::from(max.index))
            });
        // When the editor loses focus (e.g. a Find button was clicked) egui collapses
        // the selection to its end. Treat that as still selecting the last found match,
        // otherwise Find Previous would just re-find the same match.
        match (current, self.find_state.last_found) {
            (Some((a, b)), Some(found)) if a == b && a == found.1 => Some(found),
            (cur, _) => cur,
        }
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
            start.into()..end.into(),
        );
        self.find_state.query = selected.to_string();
        self.find_state.highlight_all = false;
    }

    fn select_match(&mut self, ctx: &egui::Context, start_char: usize, end_char: usize) {
        self.find_state.last_found = Some((start_char, end_char));
        if self.view_only {
            // No text editor to select in: remember the match so the view can show it.
            let start = self
                .text
                .char_indices()
                .nth(start_char)
                .map_or(self.text.len(), |(i, _)| i);
            let end = self.text[start..]
                .char_indices()
                .nth(end_char - start_char)
                .map_or(self.text.len(), |(i, _)| start + i);
            self.view_match = Some((start, end));
            return;
        }
        let editor_id = egui::Id::unique("editor");
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
        find_matches_in(&self.text, regex)
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
        if self.view_only {
            self.set_find_result("Read-only large-file view: choose Edit anyway to replace");
            return;
        }
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
        let replacement = expand_replacement(
            &regex,
            &self.text,
            m.start_byte,
            &self.find_state.replace,
            self.find_state.mode == FindMode::Literal,
        );
        self.text.replace_range(m.start_byte..m.end_byte, &replacement);
        self.touch();
        let new_end = m.start_char + replacement.chars().count();
        self.select_match(ctx, m.start_char, new_end);
        self.find_state.scroll_to_match = true;
        self.set_find_result(format!("Replaced {}/{}", idx + 1, matches.len()));
    }

    fn replace_all(&mut self) {
        if self.view_only {
            self.set_find_result("Read-only large-file view: choose Edit anyway to replace");
            return;
        }
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
        self.text = replace_all_text(
            &regex,
            &self.text,
            &self.find_state.replace,
            self.find_state.mode == FindMode::Literal,
        );
        self.touch();
        self.request_editor_focus = true;
        self.set_find_result(format!("Replaced {}", matches.len()));
    }

    /// Make sure `find_cache` holds the matches for the current text and search
    /// settings. Returns false (and turns Find All off) if the pattern is invalid.
    fn ensure_find_cache(&mut self) -> bool {
        let regex = match self.build_find_regex() {
            Ok(r) => r,
            Err(e) => {
                self.set_find_result(e);
                self.find_state.highlight_all = false;
                return false;
            }
        };
        // Recompute matches only when the text or the search settings changed.
        let key = FindCacheKey {
            rev: self.text_rev,
            query: self.find_state.query.clone(),
            mode: self.find_state.mode,
            case_sensitive: self.find_state.case_sensitive,
        };
        if self.find_cache.as_ref().map(|c| &c.key) != Some(&key) {
            self.find_cache = Some(FindCache {
                matches: find_matches_in(&self.text, &regex),
                key,
            });
        }
        true
    }

    fn paint_find_highlights(&mut self, ui: &egui::Ui, output: &TextEditOutput) {
        if !self.find_state.highlight_all || !self.ensure_find_cache() {
            return;
        }
        let Some(cache) = &self.find_cache else {
            return;
        };
        let matches = &cache.matches;
        if matches.is_empty() {
            return;
        }

        // Translucent so the text underneath stays readable in both themes.
        let color = if ui.visuals().dark_mode {
            egui::Color32::from_rgba_unmultiplied(255, 200, 40, 70)
        } else {
            egui::Color32::from_rgba_unmultiplied(255, 190, 0, 90)
        };
        paint_matches(ui, output, matches, color);
    }

    /// Put the selection `range` into the editor and give it focus.
    fn set_editor_selection(&mut self, ctx: &egui::Context, range: egui::text::CCursorRange) {
        let editor_id = egui::Id::unique("editor");
        let mut state = egui::TextEdit::load_state(ctx, editor_id).unwrap_or_default();
        state.cursor.set_char_range(Some(range));
        state.store(ctx, editor_id);
        ctx.memory_mut(|mem| mem.request_focus(editor_id));
        self.request_editor_focus = true;
    }

    /// Apply a menu action to `selection` (the editor's selection when the menu opened;
    /// menus take focus, so it can't be read from the editor itself).
    fn apply_editor_action(
        &mut self,
        ctx: &egui::Context,
        action: EditorAction,
        selection: Option<egui::text::CCursorRange>,
    ) {
        use egui::text::{CCursor, CCursorRange};
        if action == EditorAction::SelectAll {
            let len = self.text.chars().count();
            self.set_editor_selection(ctx, CCursorRange::two(CCursor::new(0), CCursor::new(len)));
            return;
        }
        let Some(range) = selection else {
            if action == EditorAction::Paste {
                self.paste_from_clipboard(ctx);
            }
            return;
        };
        let [min, max] = range.sorted_cursors();
        let (start, end) = (usize::from(min.index), usize::from(max.index));
        let start_byte = char_index_to_byte(&self.text, start);
        let end_byte = char_index_to_byte(&self.text, end);
        let has_selection = end > start;

        // Replace the selected text, then select `new_len` chars (0 = just a cursor).
        let replace = |app: &mut Self, with: &str, select: bool| {
            app.text.replace_range(start_byte..end_byte, with);
            app.touch();
            let len = with.chars().count();
            let range = if select {
                CCursorRange::two(CCursor::new(start), CCursor::new(start + len))
            } else {
                CCursorRange::one(CCursor::new(start + len))
            };
            app.set_editor_selection(ctx, range);
        };

        match action {
            EditorAction::Copy if has_selection => {
                let selected = self.text[start_byte..end_byte].to_string();
                self.copy_to_clipboard(&selected);
            }
            EditorAction::Cut if has_selection => {
                let selected = self.text[start_byte..end_byte].to_string();
                self.copy_to_clipboard(&selected);
                replace(self, "", false);
            }
            EditorAction::Delete if has_selection => replace(self, "", false),
            EditorAction::Paste => {
                if let Some(insert) = self.read_clipboard_text() {
                    replace(self, &insert.replace("\r\n", "\n"), false);
                }
            }
            EditorAction::Uppercase if has_selection => {
                let out = self.text[start_byte..end_byte].to_uppercase();
                replace(self, &out, true);
            }
            EditorAction::Lowercase if has_selection => {
                let out = self.text[start_byte..end_byte].to_lowercase();
                replace(self, &out, true);
            }
            EditorAction::Propercase if has_selection => {
                let out = to_proper_case(&self.text[start_byte..end_byte]);
                replace(self, &out, true);
            }
            _ => {}
        }
    }

    fn copy_to_clipboard(&mut self, text: &str) {
        if let Err(e) =
            Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text.to_string()))
        {
            self.set_error(format!("Clipboard copy failed: {e}"));
        }
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

    fn send_editor_event(&mut self, ctx: &egui::Context, event: egui::Event) {
        let editor_id = egui::Id::unique("editor");
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
    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        let _ = save_config_settings(&self.settings);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }
}

impl ScratchpadApp {
    /// One UI frame. Separate from `App::ui` so it can be driven without an
    /// `eframe::Frame` (see `bench.rs`).
    fn frame(&mut self, root_ui: &mut egui::Ui) {
        let ctx = root_ui.ctx().clone();
        let ctx = &ctx;
        self.apply_font_settings(ctx);
        if self.applied_always_on_top != self.settings.always_on_top {
            self.apply_window_level(ctx);
        }
        self.poll_update_check();
        let mut open_find = false;
        let mut open_replace = false;
        let mut hotkey_find_next = false;
        let mut hotkey_find_prev = false;
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::COMMAND, egui::Key::F) {
                open_find = true;
            }
            if i.consume_key(egui::Modifiers::SHIFT, egui::Key::F3) {
                hotkey_find_prev = true;
            } else if i.consume_key(egui::Modifiers::NONE, egui::Key::F3) {
                hotkey_find_next = true;
            }
            if i.consume_key(egui::Modifiers::COMMAND, egui::Key::R) {
                open_replace = true;
            }
        });
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
        if hotkey_find_next || hotkey_find_prev {
            if self.find_state.query.is_empty() {
                // Nothing to search for yet: open the dialog instead.
                self.find_state.open = true;
                self.find_state.request_focus = true;
            } else if hotkey_find_prev {
                self.find_previous(ctx);
            } else {
                self.find_next(ctx);
            }
        }
        self.check_external_change(ctx);
        let mut hotkey_new = false;
        let mut hotkey_open = false;
        let mut hotkey_save = false;
        let mut hotkey_save_as = false;
        let mut hotkey_cycle_highlighting = false;
        let mut font_step = 0.0;
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
        egui::Panel::top("menu_bar").show(root_ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New (Ctrl+N)").clicked() {
                        ui.close();
                        self.handle_command(PendingAction::NewFile);
                    }

                    if ui.button("Open... (Ctrl+O)").clicked() {
                        ui.close();
                        self.handle_command(PendingAction::OpenFile);
                    }

                    if ui.button("Open in New Window...").clicked() {
                        ui.close();
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
                                    ui.close();
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
                        ui.close();
                        self.do_save();
                    }

                    if ui.button("Save As…").clicked() {
                        ui.close();
                        self.do_save_as();
                    }

                    ui.separator();

                    if ui.button("Exit").clicked() {
                        ui.close();
                        self.maybe_defer_or_run(PendingAction::Exit);
                    }
                });

                ui.menu_button("Edit", |ui| {
                    if ui.button("Copy (Ctrl+C)").clicked() {
                        ui.close();
                        self.send_editor_event(ctx, egui::Event::Copy);
                    }

                    if ui.button("Cut (Ctrl+X)").clicked() {
                        ui.close();
                        self.send_editor_event(ctx, egui::Event::Cut);
                    }

                    if ui.button("Delete (Del)").clicked() {
                        ui.close();
                        self.apply_editor_action(ctx, EditorAction::Delete, self.last_selection);
                    }

                    if ui.button("Paste (Ctrl+V)").clicked() {
                        ui.close();
                        self.paste_from_clipboard(ctx);
                    }

                    if ui.button("Select All (Ctrl+A)").clicked() {
                        ui.close();
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
                    for (label, action) in &EDITOR_MENU_ITEMS[5..] {
                        if ui.button(*label).clicked() {
                            ui.close();
                            self.apply_editor_action(ctx, *action, self.last_selection);
                        }
                    }

                    ui.separator();
                    if ui.button("Find (Ctrl+F)").clicked() {
                        ui.close();
                        self.find_state.open = true;
                        self.find_state.show_replace = false;
                        self.find_state.request_focus = true;
                    }
                    if ui.button("Find and Replace (Ctrl+R)").clicked() {
                        ui.close();
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
                    if ui
                        .checkbox(&mut self.settings.always_on_top, "Always on top")
                        .changed()
                    {
                        self.apply_window_level(ctx);
                    }
                    ui.separator();
                    ui.label("Theme");
                    changed |= ui
                        .radio_value(&mut self.settings.theme, ThemeSetting::Auto, "Auto")
                        .changed();
                    changed |= ui
                        .radio_value(&mut self.settings.theme, ThemeSetting::Light, "Light")
                        .changed();
                    changed |= ui
                        .radio_value(&mut self.settings.theme, ThemeSetting::Dark, "Dark")
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

                ui.menu_button("Encoding", |ui| {
                    for encoding in [
                        TextEncoding::Utf8,
                        TextEncoding::Utf16Le,
                        TextEncoding::Utf16Be,
                    ] {
                        let mut selected = self.encoding;
                        if ui
                            .radio_value(&mut selected, encoding, encoding.label())
                            .clicked()
                        {
                            ui.close();
                            self.apply_encoding_selection(encoding);
                        }
                    }
                });

                ui.menu_button("Help", |ui| {
                    if let Some(version) = &self.update_available {
                        if ui.button(format!("Update to v{version}")).clicked() {
                            ui.close();
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
                        ui.close();
                        ctx.open_url(egui::OpenUrl::new_tab("https://github.com/samseyller/scratch-pad/releases"));
                    }

                                        if ui.button("About").clicked() {
                        ui.close();
                        self.about_open = true;
                    }
                });

                // Right-side: show current full path (or Untitled)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(path) = &self.file_path {
                        ui.add(egui::Label::new(path.display().to_string()).truncate());
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
            let dirty = self.is_dirty();
            egui::Window::new("File changed on disk")
                .collapsible(false)
                .resizable(false)
                .open(&mut prompt_open)
                .show(ctx, |ui| {
                    ui.label("The file has changed outside of Scratchpad.");
                    if dirty {
                        ui.label("Reloading will discard your unsaved changes.");
                    }
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
            // Closing the window with its X button counts as "Ignore"; otherwise the
            // stale mtime would immediately re-open the prompt.
            if !prompt_open && action.is_none() {
                action = Some("ignore");
            }
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
                                .id(egui::Id::unique("find_input")),
                        );
                        if self.find_state.request_focus {
                            find_response.request_focus();
                            self.find_state.request_focus = false;
                        }
                        find_changed |= find_response.changed();
                        // Enter in the box finds next, Shift+Enter finds previous; keep
                        // the focus in the box so the user can keep pressing Enter.
                        if find_response.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            && !self.find_state.query.is_empty()
                        {
                            if ui.input(|i| i.modifiers.shift) {
                                self.find_previous(ctx);
                            } else {
                                self.find_next(ctx);
                            }
                            self.request_editor_focus = false;
                            find_response.request_focus();
                        }
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
        egui::Panel::bottom("status_bar").show(root_ui, |ui| {
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
        egui::CentralPanel::default_margins().show(root_ui, |ui| {
            // Stable ID helps keep focus/state consistent.
            let editor_id = egui::Id::unique("editor");
            if self.reset_editor_state {
                egui::TextEdit::store_state(ctx, editor_id, Default::default());
                self.reset_editor_state = false;
                self.undo_capped = false;
                self.view_only = self.text.len() > HUGE_DOC_BYTES;
                self.view_match = None;
                self.find_state.last_found = None;
            }
            if self.view_only {
                self.show_large_file_view(ui);
                self.request_editor_focus = false;
                return;
            }
            self.update_undo_limit(ctx, editor_id);
            if let Some((action, selection)) = self.pending_editor_action.take() {
                self.apply_editor_action(ctx, action, selection);
            }
            let selection_before = self.last_selection;
            let show_line_numbers = self.settings.show_line_numbers;

            let font_id =
                egui::FontId::new(self.settings.font_size, self.settings.font_family.to_egui());
            let line_number_color = ui.visuals().weak_text_color();

            let total_lines = self.derived().stats.newlines + 1;
            let highlight_mode = if self.text.len() > MAX_HIGHLIGHT_BYTES {
                HighlightMode::Off
            } else {
                self.settings.highlight_mode
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
                        let digits = total_lines.to_string().len();
                        let sample = "9".repeat(digits);
                        let width = ui
                            .fonts_mut(|f| {
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
                    let wrap_width = if self.settings.word_wrap {
                        (text_width - 2.0 * text_margin_x).max(1.0)
                    } else {
                        f32::INFINITY
                    };

                    ui.horizontal(|ui| {
                        // Reserve the gutter; the numbers themselves are painted below,
                        // from the editor's own galley, so they always line up with the
                        // text (including wrapped lines) and only visible rows are drawn.
                        let gutter_x = ui.available_rect_before_wrap().min.x;
                        if show_line_numbers {
                            ui.allocate_space(egui::vec2(gutter_width, 0.0));
                        }

                        let mut layouter = Self::make_editor_layouter(
                            &self.layout_cache,
                            self.settings.word_wrap,
                            wrap_width,
                            font_id.clone(),
                            highlight_mode,
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

                        // Release the layouter's borrow of `layout_cache` before mutating self.
                        drop(layouter);

                        if output.response.changed() {
                            self.touch();
                        }

                        if self.find_state.scroll_to_match {
                            // The editor only reports its cursor on frames where it has
                            // focus, which arrives a frame after the Find click, so fall
                            // back to the stored state.
                            let cursor_range = output.cursor_range.or_else(|| {
                                egui::TextEdit::load_state(ui.ctx(), editor_id)
                                    .and_then(|st| st.cursor.range(&output.galley))
                            });
                            if let Some(cursor_range) = cursor_range {
                                let row_height = output
                                    .galley
                                    .rows
                                    .first()
                                    .map(|row| row.height())
                                    .unwrap_or(self.settings.font_size);
                                let cursor_rect = egui::text_selection::text_cursor_state::cursor_rect(
                                    &output.galley,
                                    &cursor_range.primary,
                                    row_height,
                                )
                                .translate(output.galley_pos.to_vec2());
                                ui.scroll_to_rect(cursor_rect, None);
                            }
                            self.find_state.scroll_to_match = false;
                            ui.ctx().request_repaint();
                        }

                        if show_line_numbers {
                            let clip = ui.clip_rect();
                            let painter = ui.painter();
                            let right = gutter_x + gutter_width - 4.0;
                            let mut line_no = 1usize;
                            let mut line_start = true;
                            for row in &output.galley.rows {
                                let y = output.galley_pos.y + row.rect().min.y;
                                if y > clip.max.y {
                                    break;
                                }
                                if line_start && y + row.rect().height() >= clip.min.y {
                                    painter.text(
                                        egui::pos2(right, y),
                                        egui::Align2::RIGHT_TOP,
                                        line_no.to_string(),
                                        font_id.clone(),
                                        line_number_color,
                                    );
                                }
                                line_start = row.ends_with_newline;
                                if line_start {
                                    line_no += 1;
                                }
                            }
                        }

                        if self.find_state.highlight_all {
                            self.paint_find_highlights(ui, &output);
                        }

                        // Menus take focus away from the editor, so remember the
                        // selection as it was the last time the editor had focus.
                        if output.response.has_focus() {
                            self.last_selection = output.cursor_range;
                        }

                        // While the Find window has focus the editor shows no selection;
                        // keep showing the current match.
                        if self.find_state.open && !output.response.has_focus() {
                            if let Some((start, end)) = self.find_state.last_found {
                                if self.current_selection_char_range(ctx) == Some((start, end)) {
                                    let m = FindMatch {
                                        start_byte: 0,
                                        end_byte: 0,
                                        start_char: start,
                                        end_char: end,
                                    };
                                    let color = egui::Color32::from_rgba_unmultiplied(255, 140, 0, 110);
                                    paint_matches(ui, &output, &[m], color);
                                }
                            }
                        }

                        // Right-click menu. The selection is captured when the button goes
                        // down, before the press can move the cursor.
                        if output.response.hovered()
                            && ctx.input(|i| i.pointer.secondary_pressed())
                        {
                            self.context_selection = selection_before;
                        }
                        let mut chosen = None;
                        output.response.context_menu(|ui| {
                            ui.set_min_width(170.0);
                            for (idx, (label, action)) in EDITOR_MENU_ITEMS.iter().enumerate() {
                                if idx == 5 {
                                    ui.separator();
                                }
                                if ui.button(*label).clicked() {
                                    chosen = Some(*action);
                                    ui.close();
                                }
                            }
                        });
                        if let Some(action) = chosen {
                            self.pending_editor_action = Some((action, self.context_selection));
                        }
                        if !output.response.context_menu_opened()
                            && !ctx.input(|i| i.pointer.secondary_down())
                        {
                            self.context_selection = None;
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
                .map(|f| f.path().to_path_buf())
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

                            if is_exit {
                                // Revert so the close-request guard doesn't re-block the exit.
                                self.text = self.saved_snapshot.clone();
                                self.touch();
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            } else {
                                // New/Open replace the buffer themselves on success; if the
                                // user cancels the file dialog, their edits are preserved.
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

/// Compute the replacement for the match starting at `start_byte`.
/// Literal mode inserts the replacement verbatim (no `$1` expansion); regex mode
/// expands capture groups, matching against the full text so anchors and `\b` work.
fn expand_replacement(
    regex: &Regex,
    text: &str,
    start_byte: usize,
    replacement: &str,
    literal: bool,
) -> String {
    if literal {
        return replacement.to_string();
    }
    match regex.captures_at(text, start_byte) {
        Some(caps) => {
            let mut out = String::new();
            caps.expand(replacement, &mut out);
            out
        }
        None => replacement.to_string(),
    }
}

fn replace_all_text(regex: &Regex, text: &str, replacement: &str, literal: bool) -> String {
    if literal {
        regex.replace_all(text, regex::NoExpand(replacement)).into_owned()
    } else {
        regex.replace_all(text, replacement).into_owned()
    }
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


#[cfg(test)]
mod bench;

#[cfg(test)]
mod tests {
    use super::*;

    fn rx(p: &str) -> Regex {
        Regex::new(p).unwrap()
    }

    #[test]
    fn normalize_detects_crlf() {
        let (t, le) = ScratchpadApp::normalize_line_endings("a\r\nb\r\n");
        assert_eq!(t, "a\nb\n");
        assert_eq!(le, LineEnding::CrLf);
        let (t, le) = ScratchpadApp::normalize_line_endings("a\nb");
        assert_eq!(t, "a\nb");
        assert_eq!(le, LineEnding::Lf);
    }

    #[test]
    fn apply_line_ending_roundtrip() {
        let mut app = ScratchpadApp::default();
        app.line_ending = LineEnding::CrLf;
        assert_eq!(app.apply_line_ending("a\nb\n"), "a\r\nb\r\n");
        app.line_ending = LineEnding::Lf;
        assert_eq!(app.apply_line_ending("a\nb\n"), "a\nb\n");
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.00 KiB");
        assert_eq!(format_size(1024 * 1024), "1.00 MiB");
        assert_eq!(format_size(150 * 1024), "150 KiB");
    }

    #[test]
    fn literal_replace_all_does_not_expand_dollars() {
        let r = rx(&regex::escape("price"));
        assert_eq!(replace_all_text(&r, "price price", "$5 $1", true), "$5 $1 $5 $1");
    }

    #[test]
    fn regex_replace_all_expands_groups() {
        let r = rx(r"(\w+)@(\w+)");
        assert_eq!(replace_all_text(&r, "me@host", "$2/$1", false), "host/me");
    }

    #[test]
    fn regex_replace_current_respects_context() {
        // Anchors only match with the full text as context.
        let r = rx(r"(?m)^foo");
        assert_eq!(expand_replacement(&r, "x\nfoo bar", 2, "baz", false), "baz");

        let r = rx(r"\bcat");
        let text = "concat cat";
        let m = r.find(text).unwrap();
        assert_eq!(m.start(), 7);
        assert_eq!(expand_replacement(&r, text, m.start(), "dog", false), "dog");
    }

    #[test]
    fn regex_replace_current_expands_captures() {
        let r = rx(r"(a)(b)");
        assert_eq!(expand_replacement(&r, "xab", 1, "$2$1", false), "ba");
    }

    #[test]
    fn literal_replace_current_is_verbatim() {
        let r = rx(&regex::escape("a"));
        assert_eq!(expand_replacement(&r, "a", 0, "$1", true), "$1");
    }

    const SAMPLES: &[&str] = &[
        "",
        "plain text",
        "# Heading\n## Two\n### Three\n#### Four\n",
        "**bold** and *italic* and __under__ and _it_ and `code`\n",
        "snake_case_name and 2*3*4 and unmatched ** and ` tick\n",
        "```\nfn main() {}\n```\nafter\n",
        "héllo **wörld** ünïcode `ç`\n",
        "// comment\nlet s = \"str\\\"q\"; 12.5 fn\n#include <x>\n",
    ];

    #[test]
    fn markdown_highlight_preserves_text() {
        let font = egui::FontId::monospace(14.0);
        let visuals = egui::Visuals::dark();
        for s in SAMPLES {
            let job = ScratchpadApp::build_markdown_highlight_job(s, &font, f32::INFINITY, &visuals);
            assert_eq!(&job.text, s, "markdown job altered text");
        }
    }

    #[test]
    fn syntax_highlight_preserves_text() {
        let font = egui::FontId::monospace(14.0);
        let visuals = egui::Visuals::dark();
        for s in SAMPLES {
            let job = ScratchpadApp::build_syntax_highlight_job(s, &font, f32::INFINITY, &visuals);
            assert_eq!(&job.text, s, "syntax job altered text");
        }
    }

    #[test]
    fn push_recent_dedupes_and_caps() {
        let mut app = ScratchpadApp::default();
        for i in 0..12 {
            app.push_recent(Path::new(&format!("f{i}.txt")));
        }
        app.push_recent(Path::new("f5.txt"));
        assert_eq!(app.settings.recent_files.len(), 10);
        assert_eq!(app.settings.recent_files[0], "f5.txt");
        assert_eq!(app.settings.recent_files.iter().filter(|p| *p == "f5.txt").count(), 1);
    }

    // ---- colour accessibility -------------------------------------------------

    fn lin(c: u8) -> f32 {
        let c = c as f32 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    }

    fn luminance(c: egui::Color32) -> f32 {
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }

    fn contrast(a: egui::Color32, b: egui::Color32) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Machado et al. (2009) colour-vision-deficiency matrices, severity 1.0,
    /// applied in linear RGB. `None` = normal vision.
    const CVD: [(&str, Option<[[f32; 3]; 3]>); 4] = [
        ("normal", None),
        ("protan", Some([[0.152286, 1.052583, -0.204868], [0.114503, 0.786281, 0.099216], [-0.003882, -0.048116, 1.051998]])),
        ("deutan", Some([[0.367322, 0.860646, -0.227968], [0.280085, 0.672501, 0.047413], [-0.011820, 0.042940, 0.968881]])),
        ("tritan", Some([[1.255528, -0.076749, -0.178779], [-0.078411, 0.930809, 0.147602], [0.004733, 0.691367, 0.303900]])),
    ];

    fn lab(c: egui::Color32, cvd: &Option<[[f32; 3]; 3]>) -> [f32; 3] {
        let mut rgb = [lin(c.r()), lin(c.g()), lin(c.b())];
        if let Some(m) = cvd {
            let o = rgb;
            for i in 0..3 {
                rgb[i] = (m[i][0] * o[0] + m[i][1] * o[1] + m[i][2] * o[2]).clamp(0.0, 1.0);
            }
        }
        let x = 0.4124 * rgb[0] + 0.3576 * rgb[1] + 0.1805 * rgb[2];
        let y = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
        let z = 0.0193 * rgb[0] + 0.1192 * rgb[1] + 0.9505 * rgb[2];
        let f = |t: f32| if t > 0.008856 { t.cbrt() } else { 7.787 * t + 16.0 / 116.0 };
        let (fx, fy, fz) = (f(x / 0.95047), f(y), f(z / 1.08883));
        [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
    }

    fn delta_e(a: egui::Color32, b: egui::Color32, cvd: &Option<[[f32; 3]; 3]>) -> f32 {
        let (la, lb) = (lab(a, cvd), lab(b, cvd));
        ((la[0] - lb[0]).powi(2) + (la[1] - lb[1]).powi(2) + (la[2] - lb[2]).powi(2)).sqrt()
    }

    fn themes() -> [(&'static str, egui::Visuals); 2] {
        [("dark", egui::Visuals::dark()), ("light", egui::Visuals::light())]
    }

    #[test]
    fn highlight_colors_meet_wcag_aa_contrast() {
        for (name, v) in themes() {
            let p = HighlightPalette::for_visuals(&v);
            let bg = v.extreme_bg_color;
            for (label, c) in [
                ("text", v.text_color()),
                ("strong", v.strong_text_color()),
                ("muted", p.muted),
                ("a", p.a),
                ("b", p.b),
                ("c", p.c),
                ("d", p.d),
            ] {
                let ratio = contrast(c, bg);
                assert!(ratio >= 4.5, "{name} {label}: contrast {ratio:.2}:1 < 4.5:1");
            }
        }
    }

    #[test]
    fn highlight_colors_distinguishable_for_color_blindness() {
        const MIN_DELTA_E: f32 = 20.0;
        let mut failures = Vec::new();
        for (name, v) in themes() {
            let p = HighlightPalette::for_visuals(&v);
            let colors = [("muted", p.muted), ("a", p.a), ("b", p.b), ("c", p.c), ("d", p.d)];
            for (vision, cvd) in CVD.iter() {
                for i in 0..colors.len() {
                    for j in (i + 1)..colors.len() {
                        let d = delta_e(colors[i].1, colors[j].1, cvd);
                        if d < MIN_DELTA_E {
                            failures.push(format!(
                                "{name}/{vision}: {} vs {} too similar (dE {d:.1})",
                                colors[i].0, colors[j].0
                            ));
                        }
                    }
                }
            }
        }
        // The accents must also stand apart from ordinary body text. (Muted comment
        // text is excluded: it is close to body text by design and is italic.)
        for (name, v) in themes() {
            let p = HighlightPalette::for_visuals(&v);
            for (vision, cvd) in CVD.iter() {
                for (label, c) in [("a", p.a), ("b", p.b), ("c", p.c), ("d", p.d)] {
                    let d = delta_e(c, v.text_color(), cvd);
                    if d < MIN_DELTA_E {
                        failures.push(format!("{name}/{vision}: {label} vs body text too similar (dE {d:.1})"));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "
{}", failures.join("
"));
    }

    // ---- helpers --------------------------------------------------------------

    #[test]
    fn cursor_line_col_handles_unicode_and_edges() {
        assert_eq!(cursor_line_col("", 0), (1, 1));
        assert_eq!(cursor_line_col("abc", 2), (1, 3));
        assert_eq!(cursor_line_col("ab\ncd", 3), (2, 1));
        assert_eq!(cursor_line_col("ab\ncd", 5), (2, 3));
        assert_eq!(cursor_line_col("héé\nxü", 6), (2, 3));
        assert_eq!(cursor_line_col("ab", 99), (1, 3));
    }

    #[test]
    fn text_stats_counts_newlines_and_chars() {
        assert_eq!(text_stats(""), TextStats { newlines: 0, chars: 0 });
        assert_eq!(text_stats("a\nb\n"), TextStats { newlines: 2, chars: 4 });
        let t = "héé\nü€😀";
        assert_eq!(text_stats(t).chars, t.chars().count());
        assert_eq!(text_stats(t).newlines, 1);
    }

    #[test]
    fn file_check_is_throttled() {
        let t0 = Instant::now();
        let iv = Duration::from_secs(1);
        assert!(should_check_file(None, t0, iv));
        assert!(!should_check_file(Some(t0), t0 + Duration::from_millis(999), iv));
        assert!(should_check_file(Some(t0), t0 + iv, iv));
    }

    #[test]
    fn derived_values_follow_text_revision() {
        let mut app = ScratchpadApp::default();
        assert!(!app.is_dirty());
        app.text = "hello\nworld".to_string();
        app.touch();
        assert!(app.is_dirty());
        assert_eq!(app.derived().stats.newlines, 1);
        app.saved_snapshot = app.text.clone();
        app.touch();
        assert!(!app.is_dirty());
        // Cursor lookups are cached per (revision, index) and stay correct after edits.
        assert_eq!(app.cached_cursor_line_col(8), (2, 3));
        app.text.insert_str(0, "x\n");
        app.touch();
        assert_eq!(app.cached_cursor_line_col(8), (3, 1));
    }

    #[test]
    fn find_matches_char_offsets_match_naive() {
        let text = "héllo wörld héllo\nünï héllo";
        let r = Regex::new("héllo").unwrap();
        let got = find_matches_in(text, &r);
        assert_eq!(got.len(), 3);
        for m in got {
            assert_eq!(m.start_char, text[..m.start_byte].chars().count());
            assert_eq!(m.end_char, text[..m.end_byte].chars().count());
        }
        // Empty matches shouldn't break the incremental offsets.
        let got = find_matches_in("aé", &Regex::new("").unwrap());
        assert_eq!(got.iter().map(|m| m.start_char).collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn highlight_jobs_coalesce_plain_text() {
        let font = egui::FontId::monospace(14.0);
        let visuals = egui::Visuals::dark();
        let text = "a + b - c * d / e; (f, g) [h] {i}\n".repeat(50);
        let job = ScratchpadApp::build_syntax_highlight_job(&text, &font, f32::INFINITY, &visuals);
        assert_eq!(job.text, text);
        assert!(job.sections.len() < 10, "sections: {}", job.sections.len());
    }
}

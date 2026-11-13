//! Headless frame-time benchmark. Run with:
//!
//! ```text
//! cargo test --release bench_ -- --ignored --nocapture
//! ```
//!
//! Set `BENCH_BYTES` to change the document size (default 10 MiB). It drives full
//! egui frames (layout, UI, tessellation) without a window or GPU, so numbers are
//! CPU cost per frame and exclude GPU upload and presentation.

use super::*;
use std::time::Instant;

fn make_text(size: usize) -> String {
    let mut text = String::with_capacity(size + 128);
    let mut i = 0usize;
    while text.len() < size {
        text.push_str(&format!(
            "line {i}: let value = compute({i}, \"text\"); // a comment about item {i}\n"
        ));
        i += 1;
    }
    text
}

struct Harness {
    ctx: egui::Context,
    app: ScratchpadApp,
    start: Instant,
}

impl Harness {
    fn new(text: String) -> Self {
        let mut app = ScratchpadApp::default();
        app.saved_snapshot = text.clone();
        app.text = text;
        Self { ctx: egui::Context::default(), app, start: Instant::now() }
    }

    /// Run one frame (including tessellation) and return its CPU time in ms.
    fn frame(&mut self, events: Vec<egui::Event>) -> f64 {
        let t = Instant::now();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))),
            time: Some(self.start.elapsed().as_secs_f64()),
            events,
            ..Default::default()
        };
        let app = &mut self.app;
        let mut out = self.ctx.run_ui(raw, |ui| app.frame(ui));
        // Debug builds panic if texture deltas are dropped unapplied (no renderer here).
        out.textures_delta.clear();
        let _ = self.ctx.tessellate(out.shapes, self.ctx.pixels_per_point());
        t.elapsed().as_secs_f64() * 1000.0
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn run_scenarios(label: &str, size: usize) {
    let text = make_text(size);
    let mb = text.len() as f64 / (1024.0 * 1024.0);
    let mut h = Harness::new(text);

    let cold = h.frame(vec![]);
    for _ in 0..3 {
        h.frame(vec![]);
    }
    let idle = median((0..15).map(|_| h.frame(vec![])).collect());
    let typing = median(
        (0..5)
            .map(|_| h.frame(vec![egui::Event::Text("x".to_string())]))
            .collect(),
    );

    h.app.find_state.query = "value".to_string();
    h.app.find_state.highlight_all = true;
    h.frame(vec![]); // builds the match list
    let find_idle = median((0..10).map(|_| h.frame(vec![])).collect());

    println!(
        "{label:<28} {mb:>5.1} MiB | first frame {cold:>8.1} ms | idle {idle:>7.2} ms | \
         keystroke {typing:>8.1} ms | idle+FindAll {find_idle:>7.2} ms"
    );
}

#[test]
#[ignore]
fn bench_large_document_frames() {
    let size = std::env::var("BENCH_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10 * 1024 * 1024);
    run_scenarios("large (highlighting off)", size);
    // Below MAX_HIGHLIGHT_BYTES, so the syntax highlighter is active.
    run_scenarios("medium (syntax highlighting)", 1024 * 1024);
}

#[test]
fn find_previous_walks_backwards() {
    let mut h = Harness::new("aa x aa x aa x aa\n".repeat(3));
    h.app.find_state.query = "x".to_string();
    let ctx = h.ctx.clone();
    h.frame(vec![]);
    h.frame(vec![]);
    let mut seq = vec![];
    for step in ["n", "n", "n", "p", "p", "p"] {
        if step == "n" { h.app.find_next(&ctx) } else { h.app.find_previous(&ctx) }
        h.frame(vec![]);
        h.frame(vec![]);
        // Simulate the editor losing focus (user clicked a button / the find field).
        ctx.memory_mut(|m| m.request_focus(egui::Id::unique("find_input")));
        h.frame(vec![]);
        h.frame(vec![]);
        seq.push(h.app.current_selection_char_range(&ctx));
    }
    println!("{seq:?}");
    assert_eq!(seq[3], seq[1]);
    assert_eq!(seq[4], seq[0]);
}

/// Working-set size of this process in MiB (Windows `tasklist`).
fn working_set_mib() -> f64 {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", std::process::id()), "/FO", "CSV", "/NH"])
        .output()
        .expect("tasklist");
    let s = String::from_utf8_lossy(&out.stdout);
    let field = s.trim().rsplit("\",\"").next().unwrap_or("");
    let digits: String = field.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse::<f64>().unwrap_or(0.0) / 1024.0
}

/// Measures how process memory grows with each undo point. Each undo point holds a
/// full copy of the text, so memory should grow by ~document size per edit.
/// Run: `cargo test --release bench_undo -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_undo_memory() {
    let size = std::env::var("BENCH_BYTES").ok().and_then(|v| v.parse().ok()).unwrap_or(10 * 1024 * 1024);
    let mut h = Harness::new(make_text(size));
    h.frame(vec![]);
    h.frame(vec![]);
    let base = working_set_mib();
    println!("doc {:.1} MiB, baseline working set {base:.0} MiB", size as f64 / 1048576.0);
    let mut t = 0.0;
    for n in 1..=40 {
        // One edit, then let the text sit for >1 s so the undoer records a point.
        t += 1.5;
        h.start = Instant::now() - std::time::Duration::from_secs_f64(t);
        h.frame(vec![egui::Event::Text("x".to_string())]);
        t += 1.5;
        h.start = Instant::now() - std::time::Duration::from_secs_f64(t);
        h.frame(vec![]);
        if n % 10 == 0 {
            let ws = working_set_mib();
            println!("after {n:>3} undo points: {ws:>7.0} MiB (+{:.0})", ws - base);
        }
    }
}

fn undo_count(h: &Harness) -> usize {
    let st = egui::TextEdit::load_state(&h.ctx, egui::Id::unique("editor")).unwrap();
    let dbg = format!("{:?}", st.undoer());
    dbg.split("undo count: ").nth(1).unwrap().split(',').next().unwrap().trim().parse().unwrap()
}

fn type_with_pauses(h: &mut Harness, n: usize) {
    let mut t = 0.0;
    for _ in 0..n {
        t += 1.5;
        h.start = Instant::now() - std::time::Duration::from_secs_f64(t);
        h.frame(vec![egui::Event::Text("x".to_string())]);
        t += 1.5;
        h.start = Instant::now() - std::time::Duration::from_secs_f64(t);
        h.frame(vec![]);
    }
}

#[test]
fn undo_history_is_capped_for_large_documents() {
    let mut big = Harness::new(make_text(LARGE_DOC_UNDO_BYTES + 1024));
    big.frame(vec![]);
    type_with_pauses(&mut big, 30);
    assert!(undo_count(&big) <= LARGE_DOC_MAX_UNDOS, "big doc kept {}", undo_count(&big));

    let mut small = Harness::new(make_text(10 * 1024));
    small.frame(vec![]);
    type_with_pauses(&mut small, 30);
    assert!(undo_count(&small) > LARGE_DOC_MAX_UNDOS, "small doc kept {}", undo_count(&small));
}

#[test]
fn line_starts_and_longest_line() {
    assert_eq!(build_line_starts(""), (vec![0], 0));
    assert_eq!(build_line_starts("ab\ncde\n"), (vec![0, 3, 7], 3));
    assert_eq!(build_line_starts("x"), (vec![0], 1));
    let long = "y".repeat(VIEW_MAX_COLS * 2);
    assert_eq!(build_line_starts(&long).1, VIEW_MAX_COLS);
}

#[test]
fn huge_file_opens_in_fast_read_only_view() {
    let text = make_text(HUGE_DOC_BYTES + 1024);
    let mut h = Harness::new(text.clone());
    h.app.reset_editor_state = true; // what loading a file does
    let cold = h.frame(vec![]);
    assert!(h.app.view_only, "large file should open in the view");
    let idle = median((0..10).map(|_| h.frame(vec![])).collect());
    println!("view: first frame {cold:.1} ms, idle {idle:.2} ms");
    assert!(cold < 1000.0 && idle < 50.0);

    // Typing does nothing in the read-only view.
    h.frame(vec![egui::Event::Text("x".to_string())]);
    assert_eq!(h.app.text, text);

    // Find works and records the match for the view to show and scroll to.
    let ctx = h.ctx.clone();
    h.app.find_state.query = "line 4000:".to_string();
    h.app.find_next(&ctx);
    let (a, b) = h.app.view_match.expect("match recorded");
    assert_eq!(&h.app.text[a..b], "line 4000:");
    assert!(h.app.find_state.scroll_to_match);
    h.frame(vec![]);
    assert!(!h.app.find_state.scroll_to_match, "scroll request consumed");
    h.app.find_state.highlight_all = true;
    h.frame(vec![]);

    // Replace is refused while read-only.
    h.app.find_state.replace = "zzz".to_string();
    h.app.replace_all();
    assert_eq!(h.app.text, text);

    // Overriding switches to the editor.
    h.app.view_only = false;
    h.frame(vec![]);
    assert!(!h.app.view_only);
}

#[test]
fn encodings_detect_and_roundtrip() {
    let text = "héllo wörld\nline two ✓\n";
    for enc in [TextEncoding::Utf8, TextEncoding::Utf16Le, TextEncoding::Utf16Be] {
        let bytes = encode_text(text, enc);
        let (detected, bom) = detect_encoding(&bytes);
        assert_eq!(detected, if enc == TextEncoding::Utf8 { TextEncoding::Utf8 } else { enc });
        assert_eq!(decode_bytes(&bytes[bom..], detected).unwrap(), text, "{enc:?}");
    }
    // UTF-8 BOM is recognised and stripped.
    assert_eq!(detect_encoding(&[0xEF, 0xBB, 0xBF, b'a']), (TextEncoding::Utf8, 3));
    // BOM-less ASCII UTF-16 is detected from where the zero bytes fall.
    assert_eq!(detect_encoding(b"a\0b\0c\0").0, TextEncoding::Utf16Le);
    assert_eq!(detect_encoding(b"\0a\0b\0c").0, TextEncoding::Utf16Be);
    assert_eq!(detect_encoding(b"plain ascii").0, TextEncoding::Utf8);
    // Bad data is an error, not a panic.
    assert!(decode_bytes(&[0x00], TextEncoding::Utf16Le).is_err());
    assert!(decode_bytes(&[0xFF, 0xFE, 0xFD], TextEncoding::Utf8).is_err());
}

#[test]
fn utf16_file_roundtrip_on_disk() {
    let dir = std::env::temp_dir().join("scratchpad-test-utf16");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("u16.txt");
    let mut app = ScratchpadApp::default();
    app.encoding = TextEncoding::Utf16Le;
    app.line_ending = LineEnding::CrLf;
    app.write_text_with_encoding(&path, "a\nb\n").unwrap();
    let (text, le, enc) = ScratchpadApp::read_text_with_encoding(&path).unwrap();
    assert_eq!((text.as_str(), le, enc), ("a\nb\n", LineEnding::CrLf, TextEncoding::Utf16Le));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_filter_from_extension() {
    assert_eq!(OpenFilter::from_extension("RS"), Some(OpenFilter::SourceCode));
    assert_eq!(OpenFilter::from_extension("md"), Some(OpenFilter::Text));
    assert_eq!(OpenFilter::from_extension("json"), Some(OpenFilter::ConfigData));
    assert_eq!(OpenFilter::from_extension("zzz"), None);
    let mut app = ScratchpadApp::default();
    app.update_last_open_filter(Path::new("a.csv"));
    assert_eq!(app.last_open_filter, OpenFilter::StructuredData);
    app.update_last_open_filter(Path::new("noext"));
    assert_eq!(app.last_open_filter, OpenFilter::AllFiles);
}

#[test]
fn editor_actions_edit_the_selection() {
    use egui::text::{CCursor, CCursorRange};
    let sel = |a, b| Some(CCursorRange::two(CCursor::new(a), CCursor::new(b)));
    let mut h = Harness::new("héllo wORLD foo".to_string());
    let ctx = h.ctx.clone();
    h.frame(vec![]);
    let run = |h: &mut Harness, action, s| {
        h.app.apply_editor_action(&ctx, action, s);
        h.frame(vec![]);
        h.app.text.clone()
    };
    assert_eq!(run(&mut h, EditorAction::Uppercase, sel(0, 5)), "HÉLLO wORLD foo");
    assert_eq!(run(&mut h, EditorAction::Lowercase, sel(0, 5)), "héllo wORLD foo");
    assert_eq!(run(&mut h, EditorAction::Propercase, sel(6, 11)), "héllo World foo");
    assert_eq!(run(&mut h, EditorAction::Delete, sel(5, 11)), "héllo foo");
    assert_eq!(run(&mut h, EditorAction::Cut, sel(0, 6)), "foo");
    // A bare cursor (empty selection) changes nothing.
    assert_eq!(run(&mut h, EditorAction::Delete, sel(1, 1)), "foo");
    assert_eq!(h.app.derived().stats.chars, 3, "revision counter updated");
    assert_eq!(to_proper_case("hELLO wORLD-foo_bar"), "Hello World-Foo_Bar");
}

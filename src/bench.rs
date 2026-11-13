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
        let out = self.ctx.run_ui(raw, |ui| app.frame(ui));
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

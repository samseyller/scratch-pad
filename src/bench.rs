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

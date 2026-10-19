//! Every widget with sample data. QH_SNAPSHOT=/path.png renders the window to PNG after ~3 s and exits.

use adw::prelude::*;
use qh_thread::*;
use std::time::Duration;

const REPLY: &str = r#"## What changed

The relay now keeps **one** connection per peer and retries with *backoff*. The fix is in `src/relay.rs`, with the config read from `qh.toml`.

1. Open the socket once and reuse it.
2. Retry on `ECONNRESET` with jitter.
3. Log the endpoint, see [the docs](https://example.com/qh).

- quiet by default
- loud only on failure
  - nested detail

> A dropped peer is not an error: it reconnects on the next message.

```rust
fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_millis(200);
    base * 2u32.pow(attempt.min(6)) // capped at 12.8 s
}
```

Run `cargo test relay` to check it."#;

fn main() {
    let app = adw::Application::builder().application_id("dev.qh.ThreadDemo").flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
    app.connect_startup(|_| {
        let fonts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apple/qh/Fonts");
        let n = register_fonts(&fonts);
        eprintln!("fonts registered: {n}");
        install_css();
        if std::env::var_os("QH_SNAPSHOT").is_some() {
            // entrance animations need frame ticks a hidden window never gets
            gtk::Settings::default().unwrap().set_gtk_enable_animations(false);
        }
    });
    app.connect_activate(build);
    app.run();
}

fn build(app: &adw::Application) {
    let thread = Thread::new();
    thread.set_links(Links::new(|s| s.contains('/') || s.ends_with(".toml"), |p| eprintln!("open {p}")));
    thread.set_on_review(|p| eprintln!("review {p}"));

    thread.push_user("Why does the relay drop peers after a minute? Fix it and add a test.");
    let t = thread.push_tool("read", "src/relay.rs");
    t.finish(true, "412 lines", "");
    let t = thread.push_tool("bash", "cargo test relay");
    t.finish(false, "1 failed", "running 4 tests\ntest relay::reconnect ... FAILED\n\nthread 'relay::reconnect' panicked at src/relay.rs:88:9\nassertion failed: peer.alive()");
    t.set_expanded(true);
    thread.push_tool("edit", "src/relay.rs");
    thread.push_diff("src/relay.rs", 24, 7);
    let r = thread.push_reply();
    r.set_text(REPLY);
    thread.push_verify("cargo test relay", false, 3.4, "test relay::timeout ... FAILED\nexpected 200ms, got 0ms", Some(Box::new(|| eprintln!("fix"))));
    thread.push_verify("cargo build", true, 12.1, "", None);
    thread.push_notice("context compacted · 41k → 9k tokens");
    thread.push_error("provider error: 429 rate limited, retrying in 5s");

    let plan = PlanCard::new(
        "Plan",
        &[
            ("Read the relay and its tests".into(), Step::Done),
            ("Keep one connection per peer".into(), Step::Done),
            ("Retry with jittered backoff".into(), Step::Active),
            ("Add a reconnect test".into(), Step::Pending),
        ],
    )
    .with_approve(|e| eprintln!("approve {e:?}"));
    thread.push_widget(plan.widget());

    let ask = AskCard::new(
        &Ask {
            tool: "run bash".into(),
            detail: "rm -rf target/debug/incremental && cargo test relay -- --nocapture".into(),
            warn: "deletes files outside the session's changes".into(),
            always: "cargo test *".into(),
            guarded: false,
        },
        |a| eprintln!("answer {a:?}"),
    );
    let budget = AskCard::budget(4.82, 5.0, |k| eprintln!("keep {k}"));

    let bottom = gtk::Box::new(gtk::Orientation::Vertical, 10);
    bottom.set_margin_start(24);
    bottom.set_margin_end(24);
    bottom.set_margin_bottom(14);
    bottom.append(ask.widget());
    bottom.append(budget.widget());
    let clamp = adw::Clamp::builder().maximum_size(712).child(&bottom).build();

    let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
    col.append(thread.widget());
    col.append(&clamp);
    col.add_css_class("qh-thread");
    let win = adw::ApplicationWindow::builder().application(app).default_width(900).default_height(1500).content(&col).build();
    win.present();

    // stream a second reply to exercise append()
    let r2 = thread.push_reply();
    let words: Vec<String> = "Streaming works: each delta is buffered and the **markdown** redraws at most every 60 ms, so `src/relay.rs` links stay live.".split_inclusive(' ').map(String::from).collect();
    for (i, w) in words.into_iter().enumerate() {
        let r2 = r2.clone();
        gtk::glib::timeout_add_local_once(Duration::from_millis(30 * i as u64), move || r2.append(&w));
    }

    if let Ok(path) = std::env::var("QH_SNAPSHOT") {
        let w = win.clone();
        gtk::glib::timeout_add_local_once(Duration::from_secs(3), move || {
            snapshot(&w, &path);
            w.application().unwrap().quit();
        });
    }
}

fn snapshot(win: &adw::ApplicationWindow, path: &str) {
    let content = win.content().unwrap();
    // a window on a hidden workspace gets no frame clock ticks, so lay it out by hand
    let (w, h) = (900.0f32, 2000.0f32);
    content.measure(gtk::Orientation::Vertical, w as i32);
    content.allocate(w as i32, h as i32, -1, None);
    let snap = gtk::Snapshot::new();
    // snapshot_child renders the tree directly, without waiting for a frame that never comes
    content.parent().unwrap().snapshot_child(&content, &snap);
    let Some(node) = snap.to_node() else { eprintln!("empty snapshot"); return };
    let renderer = win.native().unwrap().renderer().unwrap();
    let tex = renderer.render_texture(&node, Some(&gtk::graphene::Rect::new(0.0, 0.0, w, h)));
    match tex.save_to_png(path) {
        Ok(_) => eprintln!("saved {path}"),
        Err(e) => eprintln!("save failed: {e}"),
    }
}

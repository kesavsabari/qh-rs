//! Off-screen timings: history fill, a long streamed reply, layout of a big thread.
use adw::prelude::*;
use qh_thread::*;
use std::time::{Duration, Instant};

const REPLY: &str = include_str!("bench_reply.md");

fn lay(w: &gtk::Widget) -> f64 {
    let t = Instant::now();
    let _ = w.measure(gtk::Orientation::Vertical, 900);
    t.elapsed().as_secs_f64() * 1e3
}

fn main() {
    let app = adw::Application::builder().application_id("dev.qh.Bench").flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
    app.connect_startup(|_| install_css());
    app.connect_activate(|app| {
        let thread = Thread::new();
        let win = adw::ApplicationWindow::builder().application(app).default_width(1000).default_height(800).content(thread.widget()).build();
        win.present();
        let w: gtk::Widget = thread.widget().clone().upcast();
        // history: 100 turns = 500 items
        let t = Instant::now();
        for i in 0..100 {
            thread.push_user(&format!("question {i}: why does the relay drop peers?"));
            for n in ["read", "bash", "edit"] { thread.push_tool(n, "src/relay.rs").finish(true, "ok", ""); }
            thread.push_reply().set_text(REPLY);
        }
        eprintln!("history 500 items build: {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
        eprintln!("history first layout: {:.0} ms", lay(&w));
        eprintln!("history relayout: {:.1} ms", lay(&w));
        // streaming: a ~12 KB reply in 12-char deltas, redraw each ~60 ms worth (8 deltas)
        let r = thread.push_reply();
        let big = REPLY.repeat(std::env::var("REP").ok().and_then(|s| s.parse().ok()).unwrap_or(6));
        let (mut redraw, mut layout, mut n, mut worst) = (0.0, 0.0, 0, 0.0f64);
        let chars: Vec<char> = big.chars().collect();
        for (k, c) in chars.chunks(12).enumerate() {
            r.append(&c.iter().collect::<String>());
            if k % 8 == 7 {
                let t = Instant::now(); r.flush(); let d = t.elapsed().as_secs_f64() * 1e3;
                let l = lay(&w); redraw += d; layout += l; n += 1; worst = worst.max(d + l);
            }
        }
        eprintln!("stream {} chars: {n} redraws, avg {:.1} ms render + {:.1} ms layout, worst {:.1} ms (budget 16.7)", big.len(), redraw / n as f64, layout / n as f64, worst);
        let rss = std::fs::read_to_string("/proc/self/status").unwrap().lines().find(|l| l.starts_with("VmRSS")).unwrap().to_string();
        eprintln!("{rss}");
        let a = app.clone();
        gtk::glib::timeout_add_local_once(Duration::from_millis(300), move || a.quit());
    });
    app.run();
}

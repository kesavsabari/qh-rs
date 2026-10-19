//! All qh-code widgets with sample data. QH_SNAPSHOT=/x.png renders to PNG after ~3 s and exits;
//! QH_SNAPSHOT_PAGE=review|code|checkpoints picks the page.

use adw::prelude::*;
use gtk::{gdk, glib};
use qh_code::*;

const BEFORE: &str = "use std::io;\n\nfn main() {\n    let name = read();\n    println!(\"hello {}\", name);\n}\n\nfn read() -> String {\n    let mut s = String::new();\n    io::stdin().read_line(&mut s).unwrap();\n    s\n}\n\nfn unused() {}\n\n// end\n";
const AFTER: &str = "use std::io;\n\nfn main() {\n    let name = read().trim().to_string();\n    if name.is_empty() {\n        return;\n    }\n    println!(\"hello, {name}\");\n}\n\nfn read() -> String {\n    let mut s = String::new();\n    io::stdin().read_line(&mut s).unwrap();\n    s\n}\n\n// end\n";

fn main() -> glib::ExitCode {
    let fonts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apple/qh/Fonts");
    register_fonts(&fonts);
    let app = adw::Application::builder().application_id("dev.qh.CodeDemo").flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
    app.connect_activate(build);
    app.run_with_args::<&str>(&[])
}

fn build(app: &adw::Application) {
    load_css();
    let page = std::env::var("QH_SNAPSHOT_PAGE").unwrap_or_else(|_| "review".into());
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);

    let review = Review::new();
    review.set_changes(vec![
        ("src/main.rs".into(), BEFORE.into(), AFTER.into()),
        ("src/lib/config.rs".into(), "a\nb\n".into(), "a\nb\nc\n".into()),
        ("README.md".into(), "# qh\n".into(), "# qh\n\nTeam chat for agents.\n".into()),
    ]);
    review.on_accept(|p| println!("accept {p}"));
    review.on_reject(|p| println!("reject {p}"));
    stack.add_named(review.widget(), Some("review"));

    let cps = Checkpoints::new();
    cps.set_checkpoints(vec![
        (0, "Add a greeting to main and read the name from stdin".into(), vec!["src/main.rs".into()]),
        (1, "".into(), vec![]),
        (2, "Trim the name and skip empty input, then tidy README and config".into(), vec!["src/main.rs".into(), "README.md".into(), "src/lib/config.rs".into()]),
    ]);
    cps.set_busy(std::env::var("QH_BUSY").is_ok());
    cps.on_rewind(|t, c| println!("rewind {t} chat={c}"));
    stack.add_named(cps.widget(), Some("checkpoints"));

    let tree = FileTree::new();
    let mut files: Vec<String> = vec!["Cargo.toml".into(), "README.md".into(), "src/main.rs".into(), "src/lib/config.rs".into(), "src/lib/mod.rs".into(), "docs/guide.md".into()];
    files.extend((0..6000).map(|i| format!("vendor/pkg{}/file{}.rs", i / 100, i)));
    tree.set_files(files);
    tree.widget().set_size_request(240, -1);
    let code = CodeView::new();
    code.set_file("src/main.rs", AFTER, Some(4));
    let c2 = code.clone();
    tree.on_open(move |p| c2.set_file(&p, &format!("// {p}\n"), None));
    code.on_open_editor(|p, l| println!("editor {p}:{l}"));
    let c2 = code.clone();
    code.on_inline(move |a, b, s| {
        println!("inline {a}-{b}: {s}");
        c2.show_inline_preview("let name = read()…");
    });
    code.on_inline_apply(|| println!("apply"));
    code.on_inline_discard(|| println!("discard"));
    let split = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    split.append(tree.widget());
    code.widget().set_hexpand(true);
    split.append(code.widget());
    stack.add_named(&split, Some("code"));
    if page == "code" {
        // show a picked range with a finished inline edit
        code.open_bar();
        code.show_inline_result("    let name = read();\n", "    let name = read().trim().to_string();\n    if name.is_empty() { return; }\n");
    }
    stack.set_visible_child_name(&page);

    let sw = LayoutSwitch::new();
    sw.set_layout(Layout::Code);
    sw.on_change(|l| println!("layout {l:?}"));
    let head = adw::HeaderBar::new();
    head.set_title_widget(Some(sw.widget()));
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&head);
    tv.set_content(Some(&stack));
    let win = adw::ApplicationWindow::builder().application(app).default_width(1100).default_height(700).content(&tv).build();
    win.present();

    if let Ok(out) = std::env::var("QH_SNAPSHOT") {
        let w = win.clone();
        glib::timeout_add_seconds_local_once(3, move || {
            let p = gtk::WidgetPaintable::new(Some(&w));
            let (wd, ht) = (w.width() as f64, w.height() as f64);
            let snap = gtk::Snapshot::new();
            p.snapshot(&snap, wd, ht);
            if let Some(node) = snap.to_node() {
                let r = w.native().unwrap().renderer().unwrap();
                let tex = r.render_texture(node, Some(&gtk::graphene::Rect::new(0.0, 0.0, wd as f32, ht as f32)));
                tex.save_to_png(&out).expect("save png");
            }
            w.application().unwrap().quit();
        });
    }
    let _ = gdk::Display::default();
}

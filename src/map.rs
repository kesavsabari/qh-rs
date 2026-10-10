//! Live map of the hub: sessions as nodes, spawns and messages as lines, and optionally
//! the folders each session wrote to. A small force-directed layout keeps it readable;
//! drawn with Braille dots so lines can run at any angle.
use crate::hub::{lock, Hub};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::Line;
use ratatui::widgets::canvas::{Canvas, Line as Seg, Points};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const FADE: Duration = Duration::from_secs(25); // how long a message line stays lit
const TRAVEL: f64 = 1.6; // seconds for the dot to run along a fresh message line

#[derive(Default)]
pub struct Map {
    pos: HashMap<String, (f64, f64, f64, f64)>, // x, y, vx, vy
    pub sel: usize,
    pub files: bool,
    pub hits: Vec<(String, f64, f64)>, // node id and position, for clicks
}

pub struct Node {
    pub id: String, // session name, or "dir:<path>" for a folder
    pub label: String,
    pub color: Color,
    pub glyph: &'static str,
}

fn status_color(s: &str) -> Color {
    match s {
        "working" => Color::Green,
        "waiting-model" => Color::Yellow,
        "asking" => Color::Cyan,
        "confirm" => Color::Magenta,
        "held" => Color::Red,
        _ => Color::DarkGray,
    }
}

/// Nodes and edges (a, b, color, age for travelling dots) for the current hub state.
#[allow(clippy::type_complexity)]
fn graph(hub: &Hub, files: bool) -> (Vec<Node>, Vec<(String, String, Color, Option<f64>)>) {
    let members = lock(&hub.members).clone();
    let mut nodes = vec![];
    let mut edges = vec![];
    for m in &members {
        let (ctx, _) = hub.ctx_for(m, false);
        let fill = m.live_tokens() as f64 / ctx.max(1) as f64;
        let role = lock(&m.purpose).clone();
        let model = { let s = lock(&m.set); crate::providers::display(&s.provider, &s.model) };
        let mut label = format!("{} · {}", m.name, m.status());
        if !role.is_empty() {
            label += &format!(" · {}", role.chars().take(24).collect::<String>());
        }
        label += &format!(" · {}", model.chars().take(22).collect::<String>());
        nodes.push(Node {
            id: m.name.clone(), label, color: status_color(m.status()),
            glyph: if fill > 0.75 { "◉" } else if fill > 0.4 { "●" } else { "•" },
        });
        if !m.spawner.is_empty() {
            edges.push((m.spawner.clone(), m.name.clone(), Color::DarkGray, None));
        }
    }
    let now = Instant::now();
    // newest message per pair, so a busy pair doesn't stack lines
    let mut recent: BTreeMap<(String, String), (Color, f64)> = BTreeMap::new();
    for l in lock(&hub.links).iter() {
        let age = now.duration_since(l.at);
        if age > FADE || l.from == l.to {
            continue;
        }
        let c = match l.kind { "ask" => Color::Yellow, "report" => Color::Green, _ => Color::Cyan };
        recent.insert((l.from.clone(), l.to.clone()), (c, age.as_secs_f64()));
    }
    for ((a, b), (c, age)) in recent {
        edges.push((a, b, c, Some(age)));
    }
    if files {
        // folders (relative to the writer's working folder), each linked to the sessions writing there
        let mut dirs: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
        for (f, who) in lock(&hub.written).iter() {
            let d = f.parent().map(|p| p.to_path_buf()).unwrap_or_default();
            let v = dirs.entry(d).or_default();
            if !v.contains(who) {
                v.push(who.clone());
            }
        }
        let home = crate::config::home();
        for (d, who) in dirs.into_iter().take(30) {
            let id = format!("dir:{}", d.display());
            let short = d.strip_prefix(&home).map(|r| format!("~/{}", r.display())).unwrap_or_else(|_| d.display().to_string());
            let short = if short.chars().count() > 30 {
                let parts: Vec<&str> = short.rsplit(['/', '\\']).take(2).collect();
                format!("…/{}/{}", parts.get(1).unwrap_or(&""), parts[0])
            } else { short };
            let shared = who.len() > 1;
            nodes.push(Node {
                id: id.clone(),
                label: format!("{short}/{}", if shared { "  ⚠ shared" } else { "" }),
                color: if shared { Color::Red } else { Color::Blue },
                glyph: "▪",
            });
            for w in who {
                edges.push((w, id.clone(), if shared { Color::Red } else { Color::DarkGray }, None));
            }
        }
    }
    (nodes, edges)
}

impl Map {
    /// Advance the layout: nodes push apart, lines pull together, everything drifts to the middle.
    fn step(&mut self, nodes: &[Node], edges: &[(String, String, Color, Option<f64>)]) {
        let n = nodes.len().max(1) as f64;
        for (i, node) in nodes.iter().enumerate() {
            self.pos.entry(node.id.clone()).or_insert_with(|| {
                let a = i as f64 / n * std::f64::consts::TAU;
                (50.0 + 30.0 * a.cos(), 50.0 + 30.0 * a.sin(), 0.0, 0.0)
            });
        }
        self.pos.retain(|k, _| nodes.iter().any(|n| n.id == *k));
        let ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
        for _ in 0..3 {
            let mut force: HashMap<&str, (f64, f64)> = ids.iter().map(|i| (i.as_str(), (0.0, 0.0))).collect();
            for a in &ids {
                for b in &ids {
                    if a == b {
                        continue;
                    }
                    let (pa, pb) = (self.pos[a], self.pos[b]);
                    let (dx, dy) = (pa.0 - pb.0, pa.1 - pb.1);
                    let d2 = (dx * dx + dy * dy).max(4.0);
                    let f = 900.0 / d2;
                    let d = d2.sqrt();
                    let e = force.get_mut(a.as_str()).unwrap();
                    e.0 += f * dx / d;
                    e.1 += f * dy / d;
                }
            }
            for (a, b, _, _) in edges {
                let (Some(pa), Some(pb)) = (self.pos.get(a), self.pos.get(b)) else { continue };
                let (dx, dy) = (pb.0 - pa.0, pb.1 - pa.1);
                let d = (dx * dx + dy * dy).sqrt().max(0.1);
                let f = 0.06 * (d - 28.0);
                if let Some(e) = force.get_mut(a.as_str()) {
                    e.0 += f * dx / d;
                    e.1 += f * dy / d;
                }
                if let Some(e) = force.get_mut(b.as_str()) {
                    e.0 -= f * dx / d;
                    e.1 -= f * dy / d;
                }
            }
            for id in &ids {
                let p = self.pos.get_mut(id).unwrap();
                let (fx, fy) = force[id.as_str()];
                p.2 = (p.2 + fx + 0.02 * (50.0 - p.0)) * 0.8;
                p.3 = (p.3 + fy + 0.02 * (50.0 - p.1)) * 0.8;
                p.0 = (p.0 + p.2.clamp(-3.0, 3.0)).clamp(4.0, 80.0);
                p.1 = (p.1 + p.3.clamp(-3.0, 3.0)).clamp(6.0, 94.0);
            }
        }
    }

    pub fn ids(&self, hub: &Hub) -> Vec<String> {
        graph(hub, self.files).0.into_iter().map(|n| n.id).collect()
    }

    /// The node nearest a click (terminal cell), if close enough.
    pub fn hit(&self, area: Rect, col: u16, row: u16) -> Option<String> {
        let (fx, fy) = ((col.saturating_sub(area.x)) as f64 / area.width.max(1) as f64 * 100.0,
                        100.0 - (row.saturating_sub(area.y)) as f64 / area.height.max(1) as f64 * 100.0);
        self.hits.iter().map(|(id, x, y)| (id, (x - fx).powi(2) + ((y - fy) * 0.5).powi(2)))
            .filter(|(_, d)| *d < 40.0).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(id, _)| id.clone())
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, hub: &Hub) {
        let (nodes, edges) = graph(hub, self.files);
        self.step(&nodes, &edges);
        self.sel = self.sel.min(nodes.len().saturating_sub(1));
        let sel_id = nodes.get(self.sel).map(|n| n.id.clone()).unwrap_or_default();
        let info_h = 3;
        let canvas_area = Rect { height: area.height.saturating_sub(info_h), ..area };
        let pos = self.pos.clone();
        // canvas units per terminal column, so labels can be kept inside the frame
        let unit = 100.0 / canvas_area.width.saturating_sub(2).max(1) as f64;
        self.hits = nodes.iter().filter_map(|n| pos.get(&n.id).map(|p| (n.id.clone(), p.0, p.1))).collect();
        let canvas = Canvas::default()
            .block(Block::bordered().title(" map · ↑↓/Tab select · Enter open · f files · Esc close ")
                .border_style(Style::new().fg(Color::Cyan)))
            .marker(Marker::Braille)
            .x_bounds([0.0, 100.0])
            .y_bounds([0.0, 100.0])
            .paint(|ctx| {
                for (a, b, color, age) in &edges {
                    let (Some(pa), Some(pb)) = (pos.get(a), pos.get(b)) else { continue };
                    ctx.draw(&Seg { x1: pa.0, y1: pa.1, x2: pb.0, y2: pb.1, color: *color });
                    // a fresh message: a dot runs from sender to receiver
                    if let Some(t) = age.filter(|t| *t < TRAVEL) {
                        let k = t / TRAVEL;
                        let (x, y) = (pa.0 + (pb.0 - pa.0) * k, pa.1 + (pb.1 - pa.1) * k);
                        ctx.draw(&Points { coords: &[(x, y), (x + 0.4, y), (x, y + 0.6)], color: Color::White });
                    }
                }
                ctx.layer();
                for n in &nodes {
                    let Some(p) = pos.get(&n.id) else { continue };
                    let mut st = Style::new().fg(n.color);
                    if n.id == sel_id {
                        st = st.add_modifier(Modifier::REVERSED | Modifier::BOLD);
                    }
                    let text = format!("{} {}", n.glyph, n.label);
                    let x = p.0.min(100.0 - (text.chars().count() as f64 + 1.0) * unit).max(0.0);
                    ctx.print(x, p.1, Line::styled(text, st));
                }
            });
        f.render_widget(canvas, canvas_area);
        let info = Rect { y: area.y + canvas_area.height, height: info_h.min(area.height), ..area };
        f.render_widget(Paragraph::new(details(hub, &sel_id)).block(Block::bordered().border_style(Style::new().fg(Color::DarkGray))), info);
    }
}

/// One line about the selected node.
fn details(hub: &Hub, id: &str) -> String {
    if let Some(d) = id.strip_prefix("dir:") {
        let who: Vec<String> = lock(&hub.written).iter().filter(|(f, _)| f.parent().is_some_and(|p| p.display().to_string() == d))
            .map(|(f, w)| format!("{w}: {}", f.file_name().unwrap_or_default().to_string_lossy())).collect();
        return format!("{d}  ·  {}", who.join(", "));
    }
    let Some(m) = hub.get(id) else { return String::new() };
    let (ctx, _) = hub.ctx_for(&m, false);
    let model = { let s = lock(&m.set); crate::providers::display(&s.provider, &s.model) };
    let last = lock(&m.msgs).iter().rev().find(|x| x["role"] == "assistant" && x["content"].as_str().is_some_and(|c| !c.is_empty()))
        .and_then(|x| x["content"].as_str().map(|c| c.split_whitespace().collect::<Vec<_>>().join(" ")))
        .unwrap_or_default();
    format!("{} · {} · {} · {}/{} tokens · {}  │ {}", m.name, m.status(), model, m.live_tokens(), ctx,
            m.cwd().display(), last.chars().take(160).collect::<String>())
}

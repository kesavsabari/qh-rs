//! Ask notifications over org.freedesktop.Notifications (D-Bus, via GIO).
//!
//! Chosen over gio::Notification because it needs no .desktop file / app id
//! to route action clicks back, and the Omarchy (Quickshell) daemon advertises
//! the `actions` capability. Signals are delivered on the GLib main loop, so
//! the caller must be running one (a gtk::Application does).
use gtk::gio;
use gtk::glib::{self, prelude::ToVariant, Variant};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const BUS: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

type OnAnswer = Rc<dyn Fn(&str, &str)>;

struct Inner {
    conn: gio::DBusConnection,
    ids: RefCell<HashMap<u32, String>>, // notification id -> key
    on_answer: OnAnswer,
    _subs: RefCell<Vec<gio::SignalSubscription>>,
}

/// Clone-cheap handle; lives on the GTK main thread.
#[derive(Clone)]
pub struct Notifier(Rc<Inner>);

impl Notifier {
    /// `on_answer(key, "yes" | "no" | "open")`. "open" = body clicked.
    pub fn new(on_answer: impl Fn(&str, &str) + 'static) -> Result<Self, glib::Error> {
        let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
        let inner = Rc::new(Inner { conn: conn.clone(), ids: Default::default(), on_answer: Rc::new(on_answer), _subs: Default::default() });
        let w = Rc::downgrade(&inner);
        let sub = conn.subscribe_to_signal(Some(BUS), Some(BUS), Some("ActionInvoked"), Some(PATH), None, gio::DBusSignalFlags::NONE, move |s| {
            let (Some(i), Some((id, action))) = (w.upgrade(), s.parameters.get::<(u32, String)>()) else { return };
            let Some(key) = i.ids.borrow_mut().remove(&id) else { return };
            let ans = match action.as_str() { "yes" => "yes", "no" => "no", _ => "open" };
            (i.on_answer)(&key, ans);
        });
        let w = Rc::downgrade(&inner);
        let sub2 = conn.subscribe_to_signal(Some(BUS), Some(BUS), Some("NotificationClosed"), Some(PATH), None, gio::DBusSignalFlags::NONE, move |s| {
            if let (Some(i), Some((id, _reason))) = (w.upgrade(), s.parameters.get::<(u32, u32)>()) {
                i.ids.borrow_mut().remove(&id);
            }
        });
        inner._subs.borrow_mut().extend([sub, sub2]);
        Ok(Notifier(inner))
    }

    /// Post a permission ask with Allow once / Deny buttons. Re-asking with the
    /// same key replaces the earlier notification.
    pub fn notify_ask(&self, project: &str, tool: &str, detail: &str, key: &str) {
        let title = format!("{project} needs you");
        let body = if detail.is_empty() { format!("qh wants to run {tool}") } else { format!("{tool}: {}", truncate(detail, 160)) };
        let replaces = self.find(key).unwrap_or(0);
        let actions: Vec<String> = ["default", "Open", "yes", "Allow once", "no", "Deny"].iter().map(|s| s.to_string()).collect();
        let mut hints: HashMap<String, Variant> = HashMap::new();
        hints.insert("urgency".into(), 2u8.to_variant());
        hints.insert("category".into(), "x-qh.ask".to_variant());
        hints.insert("resident".into(), true.to_variant());
        let args = ("qh", replaces, "", title.as_str(), glib::markup_escape_text(&body).as_str(), actions, hints, 0i32).to_variant();
        let key = key.to_string();
        let me = Rc::downgrade(&self.0);
        self.0.conn.call(Some(BUS), PATH, BUS, "Notify", Some(&args), Some(glib::VariantTy::new("(u)").unwrap()), gio::DBusCallFlags::NONE, -1, gio::Cancellable::NONE, move |r| {
            match r {
                Ok(v) => {
                    if let (Some(i), Some((id,))) = (me.upgrade(), v.get::<(u32,)>()) {
                        let mut ids = i.ids.borrow_mut();
                        ids.retain(|_, k| *k != key);
                        ids.insert(id, key);
                    }
                }
                Err(e) => eprintln!("qh-desktop: notify failed: {e}"),
            }
        });
    }

    /// Withdraw the notification for `key` (answered elsewhere).
    pub fn clear(&self, key: &str) {
        if let Some(id) = self.find(key) {
            self.0.ids.borrow_mut().remove(&id);
            self.0.conn.call(Some(BUS), PATH, BUS, "CloseNotification", Some(&(id,).to_variant()), None, gio::DBusCallFlags::NONE, -1, gio::Cancellable::NONE, |_| {});
        }
    }

    fn find(&self, key: &str) -> Option<u32> {
        self.0.ids.borrow().iter().find(|(_, k)| *k == key).map(|(id, _)| *id)
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect::<String>() + "…" }
}

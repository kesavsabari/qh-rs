//! Tell the user when a tab finishes a long turn or waits on a permission, while they look elsewhere.

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

/// turns shorter than this end without a notification
pub const LONG_TURN: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode { Auto, Desktop, Off }

pub fn mode(s: &str) -> Mode {
    match s {
        "off" | "false" | "no" | "none" => Mode::Off,
        "desktop" => Mode::Desktop,
        _ => Mode::Auto,
    }
}

#[derive(Default)]
pub struct Notifier {
    pub focused: Option<bool>, // None until the terminal reports focus
    start: HashMap<String, Instant>, // tab → when its current turn began
    sent: HashMap<String, bool>,     // tab → already notified this turn
}

impl Notifier {
    /// a tab showed activity; the first activity starts its turn
    pub fn activity(&mut self, tab: &str, now: Instant) {
        self.start.entry(tab.to_string()).or_insert(now);
    }

    /// the turn ended: notify if it ran long. Resets the tab for its next turn.
    pub fn done(&mut self, mode: Mode, tab: &str, now: Instant) -> bool {
        let start = self.start.remove(tab);
        let sent = self.sent.remove(tab).unwrap_or(false);
        let long = start.is_some_and(|s| now.duration_since(s) >= LONG_TURN);
        long && !sent && self.wanted(mode)
    }

    /// a permission prompt appeared: notify once per turn
    pub fn confirm(&mut self, mode: Mode, tab: &str, now: Instant) -> bool {
        self.activity(tab, now);
        if !self.wanted(mode) || self.sent.get(tab).copied().unwrap_or(false) {
            return false;
        }
        self.sent.insert(tab.to_string(), true);
        true
    }

    /// A team event (a request, a ping, a direct message, an answer): one notification each,
    /// under the same rule as the rest.
    pub fn team_event(&self, mode: Mode) -> bool {
        self.wanted(mode)
    }

    fn wanted(&self, mode: Mode) -> bool {
        mode != Mode::Off && self.focused != Some(true)
    }
}

/// The bell, then the OS notifier when there is one (notify-send, osascript): many terminals
/// (Alacritty, foot without setup) drop OSC notifications. With no notifier, OSC 9 and 777 instead
/// (terminals ignore the one they don't know). `mode` only matters for Off, checked by the caller.
pub fn send(_mode: Mode, text: &str) {
    let t: String = text.chars().filter(|c| !c.is_control() && *c != ';').collect();
    let mut out = std::io::stdout();
    let os = !cfg!(windows) && crate::platform::on_path(if cfg!(target_os = "macos") { "osascript" } else { "notify-send" });
    let _ = if os { write!(out, "\x07") } else { write!(out, "\x07\x1b]9;{t}\x07\x1b]777;notify;qh;{t}\x07") };
    let _ = out.flush();
    if !os {
        return;
    }
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("osascript");
        c.arg("-e").arg(format!("display notification \"{}\" with title \"qh\"", t.replace(['"', '\\'], "")));
        c
    } else {
        let mut c = std::process::Command::new("notify-send");
        c.arg("qh").arg(&t);
        c
    };
    // not on PATH: spawn fails and nothing happens
    let _ = cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_turns_stay_quiet_long_ones_notify() {
        let mut n = Notifier::default();
        let t0 = Instant::now();
        n.activity("a", t0);
        assert!(!n.done(Mode::Auto, "a", t0 + Duration::from_secs(3)));
        n.activity("a", t0);
        assert!(n.done(Mode::Auto, "a", t0 + Duration::from_secs(11)));
    }

    #[test]
    fn focused_terminal_or_off_stays_quiet() {
        let t0 = Instant::now();
        let mut n = Notifier { focused: Some(true), ..Default::default() };
        n.activity("a", t0);
        assert!(!n.done(Mode::Auto, "a", t0 + Duration::from_secs(20)));
        assert!(!n.confirm(Mode::Auto, "a", t0));
        let mut n = Notifier { focused: Some(false), ..Default::default() };
        n.activity("a", t0);
        assert!(!n.done(Mode::Off, "a", t0 + Duration::from_secs(20)));
        assert!(!n.confirm(Mode::Off, "a", t0));
        // unknown focus notifies
        let mut n = Notifier::default();
        assert!(n.confirm(Mode::Desktop, "a", t0));
    }

    #[test]
    fn once_per_tab_per_turn() {
        let t0 = Instant::now();
        let mut n = Notifier::default();
        assert!(n.confirm(Mode::Auto, "a", t0));
        assert!(!n.confirm(Mode::Auto, "a", t0));
        assert!(n.confirm(Mode::Auto, "b", t0));
        // the turn's end doesn't notify again
        assert!(!n.done(Mode::Auto, "a", t0 + Duration::from_secs(30)));
        // the next turn may notify again
        assert!(n.confirm(Mode::Auto, "a", t0));
    }

    #[test]
    fn team_events_follow_focus_and_off() {
        assert!(Notifier::default().team_event(Mode::Auto), "focus unknown: notify");
        assert!(Notifier { focused: Some(false), ..Default::default() }.team_event(Mode::Desktop));
        assert!(!Notifier { focused: Some(true), ..Default::default() }.team_event(Mode::Auto), "looking at qh: quiet");
        assert!(!Notifier { focused: Some(false), ..Default::default() }.team_event(Mode::Off), "notify = off");
    }

    #[test]
    fn modes_parse() {
        assert_eq!(mode("off"), Mode::Off);
        assert_eq!(mode("desktop"), Mode::Desktop);
        assert_eq!(mode(""), Mode::Auto);
    }
}

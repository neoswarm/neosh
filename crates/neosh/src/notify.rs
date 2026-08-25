//! Deciding whether something is worth interrupting a person for, and reaching them if it is.
//!
//! There used to be one channel and everything was on it: a hundred and seventy call sites, all of
//! them a six-second toast in the same corner, sorted by nothing. `MessageLevel` says how bad a
//! thing is and never whether you need to know about it, so the corner was mostly echoing facts
//! already on screen — `favourited ~/proj` next to a row that had just grown a pin — while the two
//! events that actually deserve a person's attention, a turn finishing and a question waiting, had
//! no notification at all.
//!
//! # The rule
//!
//! **A notification is for something you did not ask for and cannot see.** [`NoticeKind`] carries
//! the first half — a reply to a key you pressed is not news, whatever else it is — and this module
//! answers the second, because it is the only place that knows which conversation is on screen,
//! which terminals are attached and whether any of them has focus.
//!
//! # Where it comes out
//!
//! Nothing leaves the terminal about a thing you are looking at. Past that:
//!
//! - **Terminal not focused** — an escape sequence, written by the view. The workspace is a process
//!   and the terminal is a viewer of it, and the person is at the terminal. A
//!   `notify-send` from here would be the same bug OSC 52 exists to avoid: neosh is used over SSH,
//!   and the machine the process is on is not the machine the eyes are at.
//! - **Nothing attached** — then there is no view, no stream and no wrong machine to be on, so the
//!   workspace shells out to whatever the platform has.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use neosh_proto::{MessageLevel, SessionId};

/// What kind of event earned an alert, so the user can turn each of them off by name.
///
/// A list in the config rather than one boolean per event: they are the same decision asked five
/// times, and five booleans is five settings to find.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertReason {
    /// A turn finished in a conversation you were not watching.
    TurnDone,
    /// An agent asked you a question and is blocked on the answer.
    Question,
    /// A tool wants permission and is blocked on the answer.
    Permission,
    /// A turn ended on an error, a refusal or a driver failure.
    Failure,
    /// A plan limit went critical — the next request is the one that gets refused.
    Quota,
    /// A plugin raised one itself, through `ApiCall::Alert`.
    Plugin,
}

impl AlertReason {
    /// The name this is switched off by in `notify.when`.
    pub fn key(self) -> &'static str {
        match self {
            Self::TurnDone => "turn.done",
            Self::Question => "question",
            Self::Permission => "permission",
            Self::Failure => "failure",
            Self::Quota => "quota",
            Self::Plugin => "plugin",
        }
    }

    pub fn all() -> [Self; 6] {
        [Self::TurnDone, Self::Question, Self::Permission, Self::Failure, Self::Quota, Self::Plugin]
    }
}

/// When the workspace may raise something outside the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AwayPolicy {
    /// Only when no attached terminal has focus. The default, and the only one that is about where
    /// the person is rather than about what we would like to be true.
    #[default]
    Focus,
    /// Whenever the thing is not the conversation on screen, focused or not. For someone who keeps
    /// neosh visible in a tiled layout and is not looking at it.
    Offscreen,
    /// Never. Marks and the corner only, which is what the workspace did before alerts existed.
    Never,
}

impl AwayPolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "focus" => Some(Self::Focus),
            "offscreen" => Some(Self::Offscreen),
            "never" | "off" => Some(Self::Never),
            _ => None,
        }
    }
}

/// When to shell out to the platform's own notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DesktopPolicy {
    /// Only when nothing is attached, because then there is no terminal to write an escape to and
    /// the workspace's own machine is the only one there is.
    #[default]
    WhenDetached,
    /// Every time. For a terminal that raises none of the escapes — at the cost of firing on the
    /// wrong machine over SSH, which is the user's call and not ours to make for them.
    Always,
    Never,
}

impl DesktopPolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "when-detached" | "when_detached" => Some(Self::WhenDetached),
            "always" => Some(Self::Always),
            "never" | "off" => Some(Self::Never),
            _ => None,
        }
    }
}

/// Everything `[notify]` can say.
#[derive(Debug, Clone)]
pub struct NotifyConfig {
    pub enabled: bool,
    pub when: Vec<String>,
    pub away: AwayPolicy,
    pub desktop: DesktopPolicy,
    /// How long without a keystroke counts as away, on a terminal that cannot report focus.
    pub idle_after: Duration,
    /// The shortest turn worth mentioning. A turn that took three seconds finished while you were
    /// still looking at the key you pressed to start it.
    pub min_turn: Duration,
    pub bell: bool,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            when: AlertReason::all().iter().map(|r| r.key().to_string()).collect(),
            away: AwayPolicy::default(),
            desktop: DesktopPolicy::default(),
            idle_after: Duration::from_secs(60),
            min_turn: Duration::from_secs(10),
            bell: false,
        }
    }
}

/// One thing worth telling somebody about.
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub title: String,
    pub body: String,
    pub level: MessageLevel,
}

/// How long a burst is gathered before it goes out as one.
///
/// Three conversations finishing while you are at lunch is one notification that says three. Long
/// enough to catch a genuine burst, short enough that a single event does not feel delayed — and
/// nothing here is on a critical path, so the delay costs nothing but the delay.
const BURST: Duration = Duration::from_millis(1500);

/// The gatekeeper. Holds what has already been said, so it is not said twice.
#[derive(Debug, Default)]
pub struct Notifier {
    cfg: NotifyConfig,
    /// Alerts gathered but not yet sent, and when the first of them arrived.
    pending: Vec<Alert>,
    since: Option<Instant>,
    /// The last turn each conversation was alerted about, so a turn cannot produce two.
    ///
    /// A turn ends once, but the things that make it newsworthy do not: a question, then a
    /// permission, then the ending. Keyed by conversation *and* reason so a question and a finish
    /// in one conversation are still two notifications — they are two different things to go and
    /// do — while a driver that reports its ending twice is one.
    said: HashMap<(SessionId, AlertReason), Instant>,
}

/// How long one conversation-and-reason stays spent.
///
/// Long enough that a driver double-reporting an ending is one notification, short enough that the
/// next real turn in the same conversation is not swallowed. Turns are minutes; this is seconds.
const REPEAT_AFTER: Duration = Duration::from_secs(20);

impl Notifier {
    pub fn new(cfg: NotifyConfig) -> Self {
        Self { cfg, ..Default::default() }
    }

    pub fn configure(&mut self, cfg: NotifyConfig) {
        self.cfg = cfg;
    }

    pub fn config(&self) -> &NotifyConfig {
        &self.cfg
    }

    /// Whether this kind of event is one the user asked to hear about.
    pub fn wants(&self, reason: AlertReason) -> bool {
        self.cfg.enabled && self.cfg.when.iter().any(|w| w == reason.key())
    }

    /// Offer an alert. `false` if it was dropped — switched off, or already said.
    ///
    /// Nothing here decides *whether the user can see it*; that is the caller's, because only the
    /// host knows which conversation is on screen. This decides whether it is worth saying at all
    /// and whether it has been said already.
    pub fn offer(&mut self, reason: AlertReason, session: Option<&SessionId>, alert: Alert) -> bool {
        if !self.wants(reason) {
            return false;
        }
        let now = Instant::now();
        if let Some(s) = session {
            let key = (s.clone(), reason);
            if self.said.get(&key).is_some_and(|at| now.duration_since(*at) < REPEAT_AFTER) {
                return false;
            }
            self.said.insert(key, now);
            // Bounded without a sweep task: the map is one entry per conversation per reason, and
            // a workspace with enough conversations to matter is one where most are long stale.
            if self.said.len() > 256 {
                self.said.retain(|_, at| now.duration_since(*at) < REPEAT_AFTER);
            }
        }
        self.since.get_or_insert(now);
        self.pending.push(alert);
        true
    }

    /// When the gathered burst is due, if anything is waiting.
    pub fn due_in(&self) -> Option<Duration> {
        let since = self.since?;
        Some(BURST.saturating_sub(since.elapsed()))
    }

    /// Take the burst, if it has gathered long enough. Several become one.
    pub fn take_due(&mut self) -> Option<Alert> {
        let since = self.since?;
        if since.elapsed() < BURST {
            return None;
        }
        self.since = None;
        let pending = std::mem::take(&mut self.pending);
        self.collapse(pending)
    }

    /// Send what is waiting right now, whether the burst window has closed or not.
    ///
    /// For shutdown, where waiting another second means never.
    pub fn flush(&mut self) -> Option<Alert> {
        self.since = None;
        let pending = std::mem::take(&mut self.pending);
        self.collapse(pending)
    }

    /// Several alerts into the one notification a person actually reads.
    ///
    /// One stays exactly as it was: the common case must not be made worse by the burst machinery
    /// existing. More than one becomes a count and the most severe level, because a list of five
    /// titles in a desktop notification is a list nobody finishes.
    fn collapse(&self, mut alerts: Vec<Alert>) -> Option<Alert> {
        match alerts.len() {
            0 => None,
            1 => alerts.pop(),
            n => {
                let level = alerts
                    .iter()
                    .map(|a| a.level)
                    .max_by_key(|l| match l {
                        MessageLevel::Error => 2,
                        MessageLevel::Warn => 1,
                        MessageLevel::Info => 0,
                    })
                    .unwrap_or(MessageLevel::Info);
                // The first is named and the rest are counted. Which one is named matters less
                // than that one of them is: "3 conversations need you" with no name is a
                // notification you have to open neosh to act on, which is most of the way back to
                // not having sent it.
                let first = alerts.remove(0);
                Some(Alert {
                    title: first.title,
                    body: format!("{} — and {} more", first.body, n - 1),
                    level,
                })
            }
        }
    }
}

/// Raise a notification through the platform's own machinery.
///
/// Only for a workspace with nothing attached: with a terminal there, the escape sequence is
/// correct and this is not, because the process may be on another machine entirely.
///
/// Spawned and not awaited. `notify-send` on a machine with no notification daemon blocks on D-Bus
/// until it times out, and the host loop is the one thing in the program that must never wait on
/// somebody else's desktop being configured.
pub fn desktop(alert: &Alert) {
    let title = alert.title.clone();
    let body = alert.body.clone();
    let urgent = matches!(alert.level, MessageLevel::Error);
    tokio::spawn(async move {
        let _ = raise(&title, &body, urgent).await;
    });
}

/// Try each of the platform's notifiers in turn, stopping at the first that works.
///
/// The same shape `images.rs` uses for the clipboard, and for the same reason: which of these
/// exists is a property of the machine rather than of the operating system, and a Linux box may
/// have any of them or none.
async fn raise(title: &str, body: &str, urgent: bool) -> std::io::Result<()> {
    use tokio::process::Command;

    // Nothing here is a shell, so nothing here needs quoting — arguments go across as argv. The
    // exception is `osascript`, which takes a script, and that one is escaped where it is built.
    let mac = format!(
        "display notification {} with title {}",
        applescript_string(body),
        applescript_string(title)
    );
    let ps = format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, \
         ContentType = WindowsRuntime] > $null; \
         $t = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent(0); \
         $t.GetElementsByTagName('text').Item(0).AppendChild($t.CreateTextNode({})) > $null; \
         $t.GetElementsByTagName('text').Item(1).AppendChild($t.CreateTextNode({})) > $null; \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('neosh')\
         .Show([Windows.UI.Notifications.ToastNotification]::new($t))",
        powershell_string(title),
        powershell_string(body)
    );

    let urgency = if urgent { "critical" } else { "normal" };
    let candidates: Vec<(&str, Vec<&str>)> = vec![
        ("notify-send", vec!["-a", "neosh", "-u", urgency, title, body]),
        // macOS. Deliberately after `notify-send`, because a Mac has no `notify-send` and a Linux
        // box has no `osascript`, so the order between them never matters and this one is slower.
        ("osascript", vec!["-e", &mac]),
        ("terminal-notifier", vec!["-title", title, "-message", body]),
        ("powershell", vec!["-NoProfile", "-NonInteractive", "-Command", &ps]),
    ];

    for (program, args) in candidates {
        match Command::new(program).args(&args).output().await {
            Ok(out) if out.status.success() => return Ok(()),
            // A program that exists and failed is worth knowing about; one that is not installed is
            // the ordinary case on every platform but one, and is not.
            Ok(out) => tracing::debug!(
                program,
                stderr = %String::from_utf8_lossy(&out.stderr),
                "notifier failed"
            ),
            Err(_) => continue,
        }
    }
    tracing::debug!("no desktop notifier on this machine");
    Ok(())
}

/// A string literal AppleScript will accept.
fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A single-quoted PowerShell literal, where the only escape is a doubled quote.
fn powershell_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(body: &str) -> Alert {
        Alert { title: "neosh".into(), body: body.into(), level: MessageLevel::Info }
    }

    fn session(s: &str) -> SessionId {
        SessionId(s.to_string())
    }

    #[test]
    fn a_reason_switched_off_is_not_offered() {
        let mut n = Notifier::new(NotifyConfig {
            when: vec!["question".into()],
            ..Default::default()
        });
        assert!(!n.offer(AlertReason::TurnDone, None, alert("done")));
        assert!(n.offer(AlertReason::Question, None, alert("asking")));
    }

    #[test]
    fn one_turn_is_one_notification_however_many_times_it_ends() {
        let mut n = Notifier::new(NotifyConfig::default());
        let s = session("a");
        assert!(n.offer(AlertReason::TurnDone, Some(&s), alert("done")));
        assert!(!n.offer(AlertReason::TurnDone, Some(&s), alert("done")));
        // A different conversation is different news, and so is a different reason in the same
        // one: a question and a finish are two things to go and do.
        assert!(n.offer(AlertReason::TurnDone, Some(&session("b")), alert("done")));
        assert!(n.offer(AlertReason::Question, Some(&s), alert("asking")));
    }

    #[test]
    fn a_burst_is_one_notification_that_says_how_many() {
        let mut n = Notifier::new(NotifyConfig::default());
        for i in 0..3 {
            n.offer(AlertReason::TurnDone, Some(&session(&i.to_string())), alert("finished"));
        }
        let out = n.flush().expect("something to say");
        assert!(out.body.contains("and 2 more"), "{}", out.body);
    }

    #[test]
    fn one_alert_is_left_exactly_as_it_was() {
        let mut n = Notifier::new(NotifyConfig::default());
        n.offer(AlertReason::TurnDone, Some(&session("a")), alert("finished"));
        assert_eq!(n.flush().expect("something to say"), alert("finished"));
    }

    #[test]
    fn the_loudest_level_in_a_burst_is_the_one_that_goes_out() {
        let mut n = Notifier::new(NotifyConfig::default());
        n.offer(AlertReason::TurnDone, Some(&session("a")), alert("finished"));
        n.offer(AlertReason::Failure, Some(&session("b")), Alert {
            title: "neosh".into(),
            body: "failed".into(),
            level: MessageLevel::Error,
        });
        assert_eq!(n.flush().expect("something to say").level, MessageLevel::Error);
    }

    #[test]
    fn nothing_waiting_is_nothing_due() {
        let mut n = Notifier::new(NotifyConfig::default());
        assert!(n.due_in().is_none());
        assert!(n.take_due().is_none());
        assert!(n.flush().is_none());
    }

    #[test]
    fn a_burst_is_not_due_the_moment_it_starts() {
        let mut n = Notifier::new(NotifyConfig::default());
        n.offer(AlertReason::TurnDone, Some(&session("a")), alert("finished"));
        assert!(n.due_in().is_some_and(|d| d > Duration::ZERO));
        assert!(n.take_due().is_none(), "the window has not closed yet");
    }

    #[test]
    fn quoting_survives_a_message_that_contains_quotes() {
        assert_eq!(applescript_string(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(powershell_string("it's"), "'it''s'");
    }
}

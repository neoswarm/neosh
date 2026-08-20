//! The JavaScript runtime thread.
//!
//! `JsRuntime` is `!Send`, so it lives on a dedicated thread with its own current-thread tokio
//! runtime and talks to the rest of the host over channels. That is not a workaround — it is the
//! same shape an out-of-process plugin has, which is why the in-process and out-of-process paths
//! can share one protocol.
//!
//! Only two ops exist. Everything a plugin can do goes through
//! [`neosh_proto::ApiCall`](neosh_proto::ApiCall) inside `op_neosh_send`, and everything the host
//! can ask of a plugin arrives through `op_neosh_next`. Keeping the v8 boundary this narrow means
//! the part most likely to break across deno_core releases is two functions rather than fifty.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::rc::Rc;
use std::time::Duration;

use deno_core::{Extension, JsRuntime, OpDecl, OpState, PollEventLoopOptions, RuntimeOptions, op2};
use neosh_proto::{MessageLevel, PluginId, PluginInbound, PluginOutbound};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::loader::NeoshModuleLoader;

const BOOTSTRAP: &str = include_str!("bootstrap.js");
const BOOTSTRAP_URL: &str = "neosh:bootstrap";

/// Host -> runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScriptInbound {
    /// Import a plugin's entry module and call its `activate`.
    Load {
        plugin: PluginId,
        /// A `file://` URL for the entry module.
        url: String,
        config: serde_json::Value,
        version: u32,
    },
    Plugin {
        plugin: PluginId,
        msg: PluginInbound,
    },
    Unload {
        plugin: PluginId,
    },
}

/// Runtime -> host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScriptOutbound {
    /// Activation finished. `error` is `None` on success.
    Loaded { plugin: PluginId, error: Option<String> },
    Plugin { plugin: PluginId, msg: PluginOutbound },
    Log { level: MessageLevel, message: String },
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

/// A scheduled callback, identified by the id JS holds.
#[derive(PartialEq, Eq)]
struct Scheduled {
    at: Instant,
    id: u64,
}

// Ordered by deadline only; `Reverse` in the heap turns it into a min-heap. The id breaks ties so
// two timers armed in the same instant fire in the order they were created, which is what every
// other JavaScript runtime does.
impl Ord for Scheduled {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.at.cmp(&other.at).then_with(|| self.id.cmp(&other.id))
    }
}
impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Pending `setTimeout`/`setInterval` deadlines, owned by Rust.
///
/// The obvious alternative is one suspended async op per timer. It is worse in two ways that
/// matter: shutdown would have to outlive the longest outstanding timer before the event loop
/// drained, and `clearTimeout` would mean reaching into a future the host cannot name. A deadline
/// heap is cancellable, inspectable by the drive loop, and thrown away in one line at teardown.
#[derive(Default)]
struct Timers {
    next_id: u64,
    pending: BinaryHeap<Reverse<Scheduled>>,
    /// Live ids mapped to their repeat interval, `None` for a one-shot.
    ///
    /// Cancelling removes from here rather than from the heap: a heap has no cheap removal, and a
    /// stale entry costs one skipped pop.
    live: HashMap<u64, Option<Duration>>,
}

impl Timers {
    /// `now` is passed in rather than read here, so every deadline in a test is exact rather than
    /// "a few microseconds after whenever the test happened to call this".
    fn start(&mut self, now: Instant, delay: Duration, repeat: bool) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        // Clamped at registration rather than at reschedule, so the stored period and the one that
        // actually elapses are the same number.
        let period = if repeat { delay.max(MIN_INTERVAL) } else { delay };
        self.live.insert(id, repeat.then_some(period));
        self.pending.push(Reverse(Scheduled { at: now + delay, id }));
        id
    }

    fn clear(&mut self, id: u64) {
        self.live.remove(&id);
    }

    fn clear_all(&mut self) {
        self.live.clear();
        self.pending.clear();
    }

    /// Every timer due at `now`, rescheduling the repeating ones.
    fn take_expired(&mut self, now: Instant) -> Vec<u64> {
        let mut due = Vec::new();
        while let Some(Reverse(next)) = self.pending.peek() {
            if next.at > now {
                break;
            }
            let Some(Reverse(fired)) = self.pending.pop() else { break };
            match self.live.get(&fired.id) {
                // Cancelled while queued: drop it without calling back into JS.
                None => continue,
                Some(&repeat) => {
                    due.push(fired.id);
                    if let Some(every) = repeat {
                        // From now rather than from the missed deadline, so a slow callback cannot
                        // build a backlog of interval firings that all land at once.
                        //
                        // `every` is at least `MIN_INTERVAL`, so this deadline is strictly after
                        // `now` and the loop above is guaranteed to terminate.
                        self.pending.push(Reverse(Scheduled { at: now + every, id: fired.id }));
                    } else {
                        self.live.remove(&fired.id);
                    }
                }
            }
        }
        due
    }

    /// When the earliest live timer is due.
    fn next_deadline(&mut self) -> Option<Instant> {
        // Discard cancelled heads so the answer is a deadline something is actually waiting for.
        while let Some(Reverse(next)) = self.pending.peek() {
            if self.live.contains_key(&next.id) {
                return Some(next.at);
            }
            self.pending.pop();
        }
        None
    }
}

type TimerState = Rc<RefCell<Timers>>;

/// Floor for a *repeating* timer's period.
///
/// Without it, `setInterval(f, 0)` reschedules at `now + 0`, which is immediately due again, and
/// `take_expired` never leaves its loop — one line of ordinary plugin JavaScript wedges the whole
/// runtime. Browsers clamp for the same reason (to 4 ms, after nesting); 1 ms is imperceptible and
/// makes `at > now` true by construction, which is what actually terminates the loop.
const MIN_INTERVAL: Duration = Duration::from_millis(1);

/// What the dispatch loop is woken with.
///
/// A superset of [`ScriptInbound`]: the runtime produces timer wakeups itself, and the host neither
/// sends nor sees them. Keeping them out of `ScriptInbound` means that type stays exactly the set
/// of things a host is allowed to say.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum Wake {
    Host(ScriptInbound),
    Timer(TimerWake),
}

#[derive(Debug, Clone, Serialize)]
struct TimerWake {
    r#type: &'static str,
    /// `f64` rather than `u64`: these cross into JS as timer handles, and a `bigint` handle would
    /// make `clearTimeout` reject the value `setTimeout` returned.
    ids: Vec<f64>,
}

/// Messages waiting for the JS dispatch loop to pick up.
///
/// The runtime does not hand the channel receiver to the op directly. `run_event_loop` returns as
/// soon as JS goes idle — a suspended op does not keep it alive — so the *host* has to own the
/// waiting and re-enter the event loop when something arrives. See [`run`].
type Queue = Rc<RefCell<std::collections::VecDeque<ScriptInbound>>>;
type Closed = Rc<std::cell::Cell<bool>>;
type Waker = Rc<tokio::sync::Notify>;

/// Await the next host message. Resolves to `null` once the host closes the channel and the queue
/// is drained, which ends the dispatch loop and lets the runtime shut down cleanly.
#[op2]
#[serde]
async fn op_neosh_next(state: Rc<RefCell<OpState>>) -> Option<Wake> {
    // Clone the handles and drop the borrow before awaiting: holding an `OpState` borrow across an
    // await point panics the moment any other op runs.
    let (queue, wake, closed, timers) = {
        let s = state.borrow();
        (
            s.borrow::<Queue>().clone(),
            s.borrow::<Waker>().clone(),
            s.borrow::<Closed>().clone(),
            s.borrow::<TimerState>().clone(),
        )
    };
    loop {
        if let Some(next) = queue.borrow_mut().pop_front() {
            return Some(Wake::Host(next));
        }
        // Checked before timers so shutdown is prompt: a one-hour `setInterval` must not be able to
        // hold the runtime open.
        if closed.get() {
            return None;
        }
        let now = Instant::now();
        let due = timers.borrow_mut().take_expired(now);
        if !due.is_empty() {
            return Some(Wake::Timer(TimerWake {
                r#type: "timer",
                ids: due.into_iter().map(|id| id as f64).collect(),
            }));
        }

        // Compute the deadline and drop the borrow before awaiting — a `RefCell` held across an
        // await point is a panic waiting for the next op to run.
        let deadline = timers.borrow_mut().next_deadline();
        match deadline {
            // `notify_one` stores a permit when nobody is waiting, so a message that arrives while
            // the event loop is between polls cannot be missed.
            None => wake.notified().await,
            Some(at) => {
                tokio::select! {
                    () = wake.notified() => {}
                    () = tokio::time::sleep_until(at) => {}
                }
            }
        }
    }
}

/// Arm a timer, returning the handle JS hands back to `clearTimeout`.
#[op2(fast)]
fn op_neosh_timer_start(state: &mut OpState, delay_ms: f64, repeat: bool) -> f64 {
    // A negative or non-finite delay is "as soon as possible", matching every browser and Node.
    let ms = if delay_ms.is_finite() && delay_ms > 0.0 { delay_ms } else { 0.0 };
    // Clamped so a plugin cannot overflow the deadline arithmetic with `setTimeout(f, 1e300)`.
    let delay = Duration::from_secs_f64((ms / 1000.0).min(60.0 * 60.0 * 24.0 * 365.0));
    let timers = state.borrow::<TimerState>().clone();
    let id = timers.borrow_mut().start(Instant::now(), delay, repeat);
    // The waker matters: the dispatch op may already be parked on a later deadline, or on no
    // deadline at all, and would otherwise sleep straight through this one.
    state.borrow::<Waker>().notify_one();
    id as f64
}

#[op2(fast)]
fn op_neosh_timer_clear(state: &mut OpState, id: f64) {
    if id.is_finite() && id > 0.0 {
        state.borrow::<TimerState>().borrow_mut().clear(id as u64);
    }
}

/// How many terminal columns a string occupies.
///
/// Synchronous, because it is called per row of a list being laid out and a round trip to the host
/// per string would make a picker feel like a network service.
///
/// It exists because a plugin composing a two-column layout has to align the column, and the only
/// tools JavaScript gives it are `String.length` (UTF-16 units) and `Array.from().length` (code
/// points). Both are wrong for the text agents actually produce: `"日本"` is 2 code points and 4
/// columns, `"👋🏽"` is 2 code points and 2 columns, `"é"` may be 2 code points and 1 column. Every
/// plugin that padded with a character count drew a ragged rule the first time a CJK model name
/// appeared in the list.
///
/// The same `unicode-width` + `unicode-segmentation` pair the frontend measures with, so a plugin
/// and the renderer agree by construction rather than by coincidence.
#[op2(fast)]
fn op_neosh_width(#[string] text: &str) -> u32 {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    // By grapheme cluster: a combining mark belongs to the character it sits on, and measuring the
    // pieces separately would count the accent as its own column.
    text.graphemes(true).map(|g| g.width() as u32).sum()
}

/// Truncate to at most `columns` terminal columns, on a grapheme boundary.
///
/// Paired with [`op_neosh_width`] because clipping is the other half of laying a column out, and a
/// plugin doing it by hand would either cut a character in half or walk the string twice through
/// the op boundary.
#[op2]
#[string]
fn op_neosh_clip(#[string] text: &str, columns: u32) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let mut used = 0u32;
    let mut out = String::with_capacity(text.len());
    for g in text.graphemes(true) {
        let w = g.width() as u32;
        if used + w > columns {
            break;
        }
        used += w;
        out.push_str(g);
    }
    out
}

/// Send one message to the host.
#[op2]
fn op_neosh_send(state: &mut OpState, #[serde] msg: ScriptOutbound) {
    let tx = state.borrow::<mpsc::UnboundedSender<ScriptOutbound>>();
    // A closed channel means the host is shutting down; dropping the message is correct.
    let _ = tx.send(msg);
}

fn extension(
    queue: Queue,
    wake: Waker,
    closed: Closed,
    timers: TimerState,
    outbox: mpsc::UnboundedSender<ScriptOutbound>,
) -> Extension {
    const OPS: &[OpDecl] = &[
        op_neosh_next(),
        op_neosh_send(),
        op_neosh_timer_start(),
        op_neosh_timer_clear(),
        op_neosh_width(),
        op_neosh_clip(),
    ];
    Extension {
        name: "neosh",
        ops: std::borrow::Cow::Borrowed(OPS),
        op_state_fn: Some(Box::new(move |state: &mut OpState| {
            state.put::<Queue>(queue.clone());
            state.put::<Waker>(wake.clone());
            state.put::<Closed>(closed.clone());
            state.put::<TimerState>(timers.clone());
            state.put::<mpsc::UnboundedSender<ScriptOutbound>>(outbox.clone());
        })),
        ..Default::default()
    }
}

/// A handle to the plugin runtime thread.
pub struct ScriptRuntime {
    to_js: mpsc::UnboundedSender<ScriptInbound>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ScriptRuntime {
    /// Spawn the runtime.
    ///
    /// Returns the handle and the stream of messages plugins produce. The thread lives until the
    /// handle is dropped.
    pub fn spawn() -> (Self, mpsc::UnboundedReceiver<ScriptOutbound>) {
        let (to_js, js_rx) = mpsc::unbounded_channel::<ScriptInbound>();
        let (js_tx, from_js) = mpsc::unbounded_channel::<ScriptOutbound>();

        let thread = std::thread::Builder::new()
            .name("neosh-script".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = js_tx.send(ScriptOutbound::Log {
                            level: MessageLevel::Error,
                            message: format!("could not start the plugin runtime: {e}"),
                        });
                        return;
                    }
                };
                let local = tokio::task::LocalSet::new();
                let err_tx = js_tx.clone();
                local.block_on(&rt, async move {
                    if let Err(e) = run(js_rx, js_tx).await {
                        let _ = err_tx.send(ScriptOutbound::Log {
                            level: MessageLevel::Error,
                            message: format!("plugin runtime stopped: {e}"),
                        });
                    }
                });
            })
            .expect("spawning the plugin runtime thread");

        (Self { to_js, thread: Some(thread) }, from_js)
    }

    /// Queue a message for the runtime. Fails only once the runtime has stopped.
    pub fn send(&self, msg: ScriptInbound) -> Result<(), String> {
        self.to_js.send(msg).map_err(|_| "the plugin runtime is not running".to_string())
    }

    pub fn is_running(&self) -> bool {
        !self.to_js.is_closed()
    }
}

impl Drop for ScriptRuntime {
    fn drop(&mut self) {
        // Closing the channel resolves `op_neosh_next` to null, which ends the dispatch loop and
        // lets the event loop drain instead of tearing down v8 mid-call.
        let (dead, _) = mpsc::unbounded_channel();
        let _ = std::mem::replace(&mut self.to_js, dead);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

async fn run(
    mut inbox: mpsc::UnboundedReceiver<ScriptInbound>,
    outbox: mpsc::UnboundedSender<ScriptOutbound>,
) -> Result<(), anyhow::Error> {
    let queue: Queue = Default::default();
    let wake: Waker = Default::default();
    let closed: Closed = Default::default();
    let timers: TimerState = Default::default();

    let mut js = JsRuntime::new(RuntimeOptions {
        module_loader: Some(Rc::new(NeoshModuleLoader::new())),
        extensions: vec![extension(
            queue.clone(),
            wake.clone(),
            closed.clone(),
            timers.clone(),
            outbox,
        )],
        ..Default::default()
    });

    let url = deno_core::ModuleSpecifier::parse(BOOTSTRAP_URL)?;
    let id = js.load_main_es_module_from_code(&url, BOOTSTRAP).await?;
    let evaluated = js.mod_evaluate(id);

    // The bootstrap's dispatch loop is a floating promise, so `mod_evaluate` resolves as soon as
    // the module body finishes and this loop is what actually drives plugins.
    //
    // `run_event_loop` returns the moment JS has nothing left to do — an op suspended on a host
    // message does *not* keep it alive. So the shape is: drive JS to idle, then block on the host
    // channel, then drive again. Anything else either exits the runtime between messages or spins.
    // Two facts about `run_event_loop` shape this loop, and getting either wrong is a hang:
    //
    // * While the dispatch loop's op is suspended waiting for a host message, it does **not**
    //   return. So it cannot simply be awaited before feeding the runtime — the loop would wait for
    //   the op, the op would wait for a message, and the message could only arrive afterwards.
    // * When it *does* return, JS is not finished; it means nothing is pending, which for this
    //   runtime means a plugin is awaiting a response only the host can send. Treating that as
    //   shutdown kills the runtime mid-activation.
    //
    // Hence: drive JS and the inbox concurrently, and when JS goes quiet, block on the inbox rather
    // than spinning on an event loop that has nothing to do.
    let opts = PollEventLoopOptions::default();
    let recv_into = |msg: Option<ScriptInbound>,
                         inbox: &mut mpsc::UnboundedReceiver<ScriptInbound>| {
        match msg {
            Some(m) => {
                queue.borrow_mut().push_back(m);
                // Drain whatever else is buffered so a burst costs one wakeup, not N.
                while let Ok(more) = inbox.try_recv() {
                    queue.borrow_mut().push_back(more);
                }
            }
            None => closed.set(true),
        }
        wake.notify_one();
    };

    loop {
        if closed.get() {
            // Drop every pending timer first: a plugin with a repeating interval would otherwise
            // keep handing the event loop work forever and shutdown would never finish.
            timers.borrow_mut().clear_all();
            // Let the dispatch loop observe `null`, unwind, and drain any in-flight work.
            wake.notify_one();
            js.run_event_loop(opts).await?;
            break;
        }
        tokio::select! {
            biased;
            msg = inbox.recv() => recv_into(msg, &mut inbox),
            res = js.run_event_loop(opts) => {
                res?;
                if !closed.get() {
                    // JS has gone quiet. Wait for a host message — but no longer than the next
                    // timer is due, or a `setTimeout` in an otherwise idle session would not fire
                    // until the user happened to press a key.
                    //
                    // The dispatch op parks on the same deadline, so between the two of them the
                    // timer fires whether or not `run_event_loop` returns while an op is pending.
                    // That belt-and-braces is deliberate: the answer differs across deno_core
                    // versions, and a hung timer is a bug nobody reports as a hung timer.
                    let deadline = timers.borrow_mut().next_deadline();
                    match deadline {
                        None => {
                            let msg = inbox.recv().await;
                            recv_into(msg, &mut inbox);
                        }
                        Some(at) => {
                            tokio::select! {
                                msg = inbox.recv() => recv_into(msg, &mut inbox),
                                () = tokio::time::sleep_until(at) => wake.notify_one(),
                            }
                        }
                    }
                }
            }
        }
    }

    evaluated.await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[tokio::test]
    async fn timers_fire_in_deadline_order() {
        let mut t = Timers::default();
        let start = Instant::now();
        let late = t.start(start, ms(50), false);
        let early = t.start(start, ms(10), false);
        assert_eq!(t.take_expired(start), Vec::<u64>::new(), "nothing is due yet");
        assert_eq!(t.take_expired(start + ms(10)), vec![early], "due exactly on its deadline");
        assert_eq!(t.take_expired(start + ms(50)), vec![late]);
    }

    #[tokio::test]
    async fn two_timers_armed_together_fire_in_creation_order() {
        // What every other JavaScript runtime does, and what a plugin will assume.
        let mut t = Timers::default();
        let start = Instant::now();
        let a = t.start(start, ms(10), false);
        let b = t.start(start, ms(10), false);
        assert_eq!(t.take_expired(start + ms(10)), vec![a, b]);
    }

    #[tokio::test]
    async fn a_one_shot_does_not_fire_twice() {
        let mut t = Timers::default();
        let start = Instant::now();
        let id = t.start(start, ms(10), false);
        assert_eq!(t.take_expired(start + ms(10)), vec![id]);
        assert!(t.take_expired(start + ms(100)).is_empty());
    }

    #[tokio::test]
    async fn an_interval_reschedules_itself() {
        let mut t = Timers::default();
        let start = Instant::now();
        let id = t.start(start, ms(10), true);
        assert_eq!(t.take_expired(start + ms(10)), vec![id]);
        assert_eq!(t.take_expired(start + ms(20)), vec![id]);
        assert_eq!(t.take_expired(start + ms(30)), vec![id]);
    }

    #[tokio::test]
    async fn a_slow_callback_does_not_build_a_backlog() {
        // Rescheduling from the missed deadline would deliver a burst of firings the moment a busy
        // plugin came up for air.
        let mut t = Timers::default();
        let start = Instant::now();
        let id = t.start(start, ms(10), true);
        assert_eq!(t.take_expired(start + ms(10)), vec![id]);
        // Nothing polled for a second; exactly one firing is owed, not a hundred.
        assert_eq!(t.take_expired(start + ms(1010)), vec![id]);
        assert_eq!(t.take_expired(start + ms(1015)), Vec::<u64>::new());
    }

    #[tokio::test]
    async fn a_zero_delay_interval_terminates() {
        // Regression: rescheduling at `now + 0` made `take_expired` spin forever, hanging the
        // runtime thread and with it the whole process. One line of plugin JS could do it.
        let mut t = Timers::default();
        let start = Instant::now();
        let id = t.start(start, Duration::ZERO, true);
        assert_eq!(t.take_expired(start), vec![id], "fires immediately");
        assert!(
            t.take_expired(start).is_empty(),
            "and is not due again at the same instant, or the drain loop never ends"
        );
        assert_eq!(t.take_expired(start + MIN_INTERVAL), vec![id], "still repeats");
    }

    #[tokio::test]
    async fn a_zero_delay_one_shot_is_not_clamped() {
        // Only repeats need a floor; `setTimeout(f, 0)` should be as soon as possible.
        let mut t = Timers::default();
        let start = Instant::now();
        let id = t.start(start, Duration::ZERO, false);
        assert_eq!(t.take_expired(start), vec![id]);
    }

    #[tokio::test]
    async fn a_cleared_timer_never_fires() {
        let mut t = Timers::default();
        let start = Instant::now();
        let id = t.start(start, ms(10), false);
        t.clear(id);
        assert!(t.take_expired(start + ms(50)).is_empty());
    }

    #[tokio::test]
    async fn clearing_an_interval_stops_it_between_firings() {
        let mut t = Timers::default();
        let start = Instant::now();
        let id = t.start(start, ms(10), true);
        assert_eq!(t.take_expired(start + ms(10)), vec![id]);
        t.clear(id);
        assert!(t.take_expired(start + ms(100)).is_empty());
    }

    #[tokio::test]
    async fn the_next_deadline_skips_cancelled_timers() {
        // The drive loop sleeps on this. Reporting a cancelled timer's deadline would wake it for
        // nothing; reporting `None` when a live timer exists would let that timer never fire.
        let mut t = Timers::default();
        let start = Instant::now();
        let dead = t.start(start, ms(10), false);
        let live = t.start(start, ms(50), false);
        t.clear(dead);
        assert_eq!(t.next_deadline(), Some(start + ms(50)), "reported the cancelled deadline");
        t.clear(live);
        assert_eq!(t.next_deadline(), None);
    }

    #[tokio::test]
    async fn clearing_everything_leaves_nothing_to_wait_for() {
        // Shutdown depends on this: a repeating interval must not be able to hold the runtime open.
        let mut t = Timers::default();
        let start = Instant::now();
        t.start(start, ms(10), true);
        t.start(start, ms(60 * 60 * 1000), false);
        t.clear_all();
        assert_eq!(t.next_deadline(), None);
        assert!(t.take_expired(Instant::now() + Duration::from_secs(7200)).is_empty());
    }

    #[test]
    fn a_timer_wake_serializes_as_its_own_message_kind() {
        // It shares `op_neosh_next` with host messages, so the dispatch loop tells them apart by
        // `type` exactly as it does for `load`/`plugin`/`unload`.
        let w = Wake::Timer(TimerWake { r#type: "timer", ids: vec![1.0, 2.0] });
        let s = serde_json::to_string(&w).unwrap();
        assert_eq!(s, r#"{"type":"timer","ids":[1.0,2.0]}"#);
    }

    #[test]
    fn a_host_message_is_unchanged_by_sharing_the_wake_channel() {
        let w = Wake::Host(ScriptInbound::Unload { plugin: PluginId::from("p") });
        let s = serde_json::to_string(&w).unwrap();
        assert_eq!(s, r#"{"type":"unload","plugin":"p"}"#);
    }

    #[test]
    fn the_bootstrap_installs_the_timer_globals() {
        // A bare deno_core has none. Without these, `setTimeout` is a runtime `undefined`.
        for name in ["setTimeout", "setInterval", "clearTimeout", "clearInterval"] {
            assert!(BOOTSTRAP.contains(&format!("globalThis.{name}")), "missing {name}");
        }
        assert!(BOOTSTRAP.contains("op_neosh_timer_start"));
    }

    #[test]
    fn inbound_and_outbound_round_trip_as_json() {
        let m = ScriptInbound::Load {
            plugin: PluginId::from("hello"),
            url: "file:///p/main.ts".into(),
            config: serde_json::json!({"a": 1}),
            version: 1,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"type\":\"load\""));
        let back: ScriptInbound = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ScriptInbound::Load { .. }));

        let o = ScriptOutbound::Loaded { plugin: PluginId::from("hello"), error: None };
        let s = serde_json::to_string(&o).unwrap();
        assert!(s.contains("\"type\":\"loaded\""));
    }

    #[test]
    fn the_bootstrap_imports_the_virtual_api_module() {
        assert!(BOOTSTRAP.contains("@neosh/api"));
        assert!(BOOTSTRAP.contains("op_neosh_next"));
        assert!(BOOTSTRAP.contains("op_neosh_send"));
    }
}

//! The frontend's copy of core state.
//!
//! A frontend is a fold over [`UiEvent`] plus a draw. Keeping that fold in its own type — with no
//! ratatui in sight — means the same mirror backs the terminal renderer, the snapshot tests, and
//! any future web or mobile frontend, and that a protocol bug shows up as a failing mirror test
//! rather than as a wrong pixel.

use std::collections::BTreeMap;

use neosh_proto::{
    BufferId, HighlightDef, LineRender, NoticeKind, Rect, SurfaceCell, SurfaceId, UiEvent,
    WindowId, WindowLayout,
};

#[derive(Debug, Clone, Default)]
pub struct MirrorBuffer {
    pub name: String,
    pub lines: Vec<LineRender>,
}

#[derive(Debug, Clone)]
pub struct MirrorWindow {
    pub buf: BufferId,
    pub layout: WindowLayout,
    pub cursor: (u32, u32),
    /// What the caret looks like here — a bar between characters, or a block on one.
    pub cursor_shape: neosh_proto::CursorShape,
    /// `None` is unscrolled: the end of a window that follows its tail, the start of every other.
    pub top_line: Option<u32>,
    /// How this window reads group names — `winhighlight`. Empty for almost every window.
    pub highlights: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct MirrorSurface {
    pub win: WindowId,
    pub rect: Rect,
    pub cells: Vec<SurfaceCell>,
}

/// One thing the corner is saying, and when it started saying it.
#[derive(Debug, Clone)]
pub struct Notice {
    pub level: neosh_proto::MessageLevel,
    pub kind: NoticeKind,
    pub text: String,
    pub at: std::time::Instant,
    /// How many times this same text has arrived in a row.
    ///
    /// A burst of one message repeated is one row with a count, not five rows of the same
    /// sentence. Drawn only above one, so the ordinary case reads exactly as it always did.
    pub repeats: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Mirror {
    pub protocol_version: u32,
    pub buffers: BTreeMap<BufferId, MirrorBuffer>,
    pub windows: BTreeMap<WindowId, MirrorWindow>,
    /// Insertion order, which is the tie-breaker for equal z-index.
    pub window_order: Vec<WindowId>,
    pub surfaces: BTreeMap<SurfaceId, MirrorSurface>,
    pub highlights: BTreeMap<String, HighlightDef>,
    pub focus: Option<WindowId>,
    /// Notifications, with when each arrived.
    ///
    /// The protocol carries no timestamp — a message is a message, and a frontend that never shows
    /// them has no use for one. The clock is stamped here, on receipt, so expiry is the frontend's
    /// business and the wire stays clock-free.
    ///
    /// Only [`NoticeKind::Reply`] and [`NoticeKind::Alert`] are in here; progress lives in its own
    /// map because it is replaced rather than appended.
    pub messages: Vec<Notice>,
    /// Progress rows, by the key that owns each one.
    ///
    /// A map rather than a list because the whole point is that writing the same key twice is one
    /// row that changed, not two rows. Ordered by key so the rows do not shuffle between frames:
    /// two operations running at once must not swap places every time one of them ticks.
    pub progress: BTreeMap<String, (String, std::time::Instant)>,
    /// Alerts the host has decided nobody can see, waiting to be raised outside the terminal.
    ///
    /// Taken by the frontend when it next draws, exactly like [`Mirror::clipboard`] and for the
    /// same reason: the mirror owns no terminal, and writing an escape sequence is the one thing
    /// this type must not do if it is to stay testable without a tty.
    pub alerts: Vec<(neosh_proto::MessageLevel, String, String)>,
    /// Text the core asked us to put on the clipboard, taken by the renderer when it next draws.
    ///
    /// Queued rather than written here because the mirror owns no terminal — this type is pure
    /// state folding, which is what makes it testable without a tty.
    pub clipboard: Option<String>,
    pub shutdown: bool,
    /// Set by `Flush`; the renderer clears it after drawing.
    pub dirty: bool,
}

impl Mirror {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event in. Returns `true` when a redraw is due.
    pub fn apply(&mut self, ev: UiEvent) -> bool {
        match ev {
            UiEvent::Init { protocol_version } => {
                self.protocol_version = protocol_version;
            }
            UiEvent::BufferOpened { buf, name } => {
                self.buffers.entry(buf).or_default().name = name;
            }
            UiEvent::BufferClosed { buf } => {
                self.buffers.remove(&buf);
            }
            UiEvent::BufferLines { buf, start, old_end, lines } => {
                let b = self.buffers.entry(buf).or_default();
                let len = b.lines.len();
                // Exactly the splice the core performed. Clamping rather than trusting the
                // indices keeps a protocol desync from panicking the UI.
                let s = (start as usize).min(len);
                let e = (old_end as usize).clamp(s, len);
                b.lines.splice(s..e, lines);
            }
            UiEvent::WindowOpened { win, buf, layout } => {
                self.windows.insert(win, MirrorWindow {
                    buf,
                    layout,
                    cursor: (0, 0),
                    cursor_shape: Default::default(),
                    top_line: None,
                    highlights: BTreeMap::new(),
                });
                if !self.window_order.contains(&win) {
                    self.window_order.push(win);
                }
            }
            UiEvent::WindowConfigured { win, layout } => {
                if let Some(w) = self.windows.get_mut(&win) {
                    w.layout = layout;
                }
            }
            UiEvent::WindowBuffer { win, buf } => {
                if let Some(w) = self.windows.get_mut(&win) {
                    w.buf = buf;
                }
            }
            UiEvent::WindowClosed { win } => {
                self.windows.remove(&win);
                self.window_order.retain(|w| *w != win);
                self.surfaces.retain(|_, s| s.win != win);
                if self.focus == Some(win) {
                    self.focus = None;
                }
            }
            UiEvent::CursorMoved { win, row, col } => {
                if let Some(w) = self.windows.get_mut(&win) {
                    w.cursor = (row, col);
                }
            }
            UiEvent::CursorShapeChanged { win, shape } => {
                if let Some(w) = self.windows.get_mut(&win) {
                    w.cursor_shape = shape;
                }
            }
            UiEvent::ScrollTo { win, top_line } => {
                if let Some(w) = self.windows.get_mut(&win) {
                    w.top_line = top_line;
                }
            }
            UiEvent::HighlightDefined { name, def } => {
                self.highlights.insert(name, def);
            }
            UiEvent::HighlightCleared { name } => {
                self.highlights.remove(&name);
            }
            UiEvent::WindowHighlights { win, map } => {
                if let Some(w) = self.windows.get_mut(&win) {
                    w.highlights = map;
                }
            }
            UiEvent::SurfaceClaimed { surface, win, rect } => {
                self.surfaces.insert(surface, MirrorSurface { win, rect, cells: Vec::new() });
            }
            UiEvent::SurfaceCells { surface, cells } => {
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    // Cells are sparse updates over a retained grid, so merge by position rather
                    // than replace: a plugin redrawing one column must not blank the rest.
                    for c in cells {
                        match s
                            .cells
                            .iter_mut()
                            .find(|e| e.row == c.row && e.col == c.col)
                        {
                            Some(existing) => *existing = c,
                            None => s.cells.push(c),
                        }
                    }
                }
            }
            UiEvent::SurfaceReleased { surface } => {
                self.surfaces.remove(&surface);
            }
            UiEvent::FocusChanged { win } => {
                self.focus = win;
            }
            UiEvent::Message { level, text, kind, key } => {
                let now = std::time::Instant::now();
                if kind == NoticeKind::Progress {
                    // A row that is replaced in place. No expiry stamped forward: the clock is
                    // reset on every write so that a long operation which keeps reporting stays
                    // on screen, and one that stops reporting eventually falls off by itself.
                    if let Some(key) = key {
                        self.progress.insert(key, (text, now));
                    }
                    return false;
                }
                // The same sentence arriving again is that sentence happening again, not a second
                // thing to read. Only against the newest, so two messages alternating are two
                // rows — which is what they are.
                if let Some(last) = self.messages.last_mut()
                    && last.text == text
                    && last.level == level
                    && last.kind == kind
                {
                    last.repeats += 1;
                    last.at = now;
                    return false;
                }
                // A reply is feedback for the key you just pressed, so the previous reply is
                // feedback for a key you have already moved on from. One at a time. An alert is
                // news and stays: it is not answering anything, so nothing supersedes it.
                if kind == NoticeKind::Reply {
                    self.messages.retain(|m| m.kind != NoticeKind::Reply);
                }
                self.messages.push(Notice { level, kind, text, at: now, repeats: 1 });
                // Keep the log bounded; a long agent session would otherwise grow it forever.
                if self.messages.len() > 200 {
                    self.messages.drain(..self.messages.len() - 200);
                }
            }
            UiEvent::ProgressDone { key } => {
                self.progress.remove(&key);
            }
            UiEvent::Alert { title, body, level } => {
                self.alerts.push((level, title, body));
                return true;
            }
            UiEvent::Clipboard { text } => {
                self.clipboard = Some(text);
                return true;
            }
            UiEvent::Flush => {
                self.dirty = true;
                return true;
            }
            UiEvent::Shutdown => {
                self.shutdown = true;
                return true;
            }
        }
        false
    }

    pub fn buffer_of(&self, win: WindowId) -> Option<&MirrorBuffer> {
        self.windows.get(&win).and_then(|w| self.buffers.get(&w.buf))
    }

    /// Windows in paint order: docks first, then floats by z-index, ties broken by open order.
    pub fn paint_order(&self) -> Vec<WindowId> {
        let mut v: Vec<WindowId> = self.window_order.to_vec();
        v.sort_by_key(|w| {
            let z = match self.windows.get(w).map(|w| &w.layout) {
                Some(WindowLayout::Float { config }) => config.z,
                _ => i32::MIN,
            };
            let order = self.window_order.iter().position(|x| x == w).unwrap_or(0);
            (z, order as i32)
        });
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::{
        Anchor, Dock, ExtmarkId, Gravity, ExtmarkOpts, ExtmarkRender, Extent, FloatConfig, NamespaceId,
        VirtChunk, VirtTextPos,
    };

    fn line(text: &str) -> LineRender {
        LineRender { text: text.into(), marks: vec![] }
    }

    fn marked(text: &str, col: u32, virt: &str) -> LineRender {
        LineRender {
            text: text.into(),
            marks: vec![ExtmarkRender {
                ns: NamespaceId(1),
                id: ExtmarkId(1),
                col,
                opts: ExtmarkOpts {
                    virt_text: vec![VirtChunk {
                        text: virt.into(),
                        hl_group: Some("X".into()),
                    }],
                    virt_text_pos: VirtTextPos::Eol,
                    ..Default::default()
                },
            }],
        }
    }

    fn mirror_with(lines: &[&str]) -> Mirror {
        let mut m = Mirror::new();
        m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "b".into() });
        m.apply(UiEvent::BufferLines {
            buf: BufferId(1),
            start: 0,
            old_end: 0,
            lines: lines.iter().map(|s| line(s)).collect(),
        });
        m
    }

    #[test]
    fn buffer_lines_replay_the_cores_splice_exactly() {
        let mut m = mirror_with(&["a", "b", "c"]);
        m.apply(UiEvent::BufferLines {
            buf: BufferId(1),
            start: 1,
            old_end: 2,
            lines: vec![line("x"), line("y")],
        });
        let texts: Vec<_> =
            m.buffers[&BufferId(1)].lines.iter().map(|l| l.text.clone()).collect();
        assert_eq!(texts, vec!["a", "x", "y", "c"]);
    }

    #[test]
    fn text_and_marks_arrive_together_so_they_cannot_desync() {
        let mut m = mirror_with(&["target"]);
        m.apply(UiEvent::BufferLines {
            buf: BufferId(1),
            start: 0,
            old_end: 1,
            lines: vec![marked("target", 0, "<- here")],
        });
        // Insert a line above; the mark's line moves with it because it is part of the payload.
        m.apply(UiEvent::BufferLines {
            buf: BufferId(1),
            start: 0,
            old_end: 0,
            lines: vec![line("inserted")],
        });
        let b = &m.buffers[&BufferId(1)];
        assert_eq!(b.lines[0].text, "inserted");
        assert_eq!(b.lines[1].marks.len(), 1, "the mark rode along with its line");
        assert_eq!(b.lines[1].marks[0].opts.virt_text[0].text, "<- here");
    }

    #[test]
    fn out_of_range_indices_clamp_rather_than_panic() {
        let mut m = mirror_with(&["a"]);
        m.apply(UiEvent::BufferLines {
            buf: BufferId(1),
            start: 99,
            old_end: 200,
            lines: vec![line("z")],
        });
        assert_eq!(m.buffers[&BufferId(1)].lines.last().unwrap().text, "z");
    }

    #[test]
    fn only_flush_requests_a_redraw() {
        let mut m = mirror_with(&["a"]);
        assert!(!m.apply(UiEvent::CursorMoved { win: WindowId(1), row: 0, col: 0 }));
        assert!(m.apply(UiEvent::Flush), "a batch ends with exactly one redraw");
    }

    #[test]
    fn floats_paint_above_docks_and_higher_z_wins() {
        let mut m = Mirror::new();
        m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "b".into() });
        m.apply(UiEvent::WindowOpened {
            win: WindowId(1),
            buf: BufferId(1),
            layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
        });
        let float = |z: i32| WindowLayout::Float {
            config: FloatConfig {
                anchor: Anchor::Screen,
                width: Extent::Fixed { n: 10 },
                height: Extent::Fixed { n: 3 },
                z,
                ..Default::default()
            },
        };
        m.apply(UiEvent::WindowOpened { win: WindowId(2), buf: BufferId(1), layout: float(200) });
        m.apply(UiEvent::WindowOpened { win: WindowId(3), buf: BufferId(1), layout: float(100) });
        assert_eq!(m.paint_order(), vec![WindowId(1), WindowId(3), WindowId(2)]);
    }

    #[test]
    fn closing_a_window_drops_its_surfaces_and_clears_focus() {
        let mut m = Mirror::new();
        m.apply(UiEvent::WindowOpened {
            win: WindowId(1),
            buf: BufferId(1),
            layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
        });
        m.apply(UiEvent::SurfaceClaimed {
            surface: SurfaceId(1),
            win: WindowId(1),
            rect: Rect { row: 0, col: 0, width: 4, height: 2 },
        });
        m.apply(UiEvent::FocusChanged { win: Some(WindowId(1)) });
        m.apply(UiEvent::WindowClosed { win: WindowId(1) });
        assert!(m.surfaces.is_empty());
        assert_eq!(m.focus, None);
    }

    #[test]
    fn surface_cells_merge_rather_than_replace() {
        let mut m = Mirror::new();
        m.apply(UiEvent::SurfaceClaimed {
            surface: SurfaceId(1),
            win: WindowId(1),
            rect: Rect { row: 0, col: 0, width: 4, height: 2 },
        });
        let cell = |row, col, g: &str| SurfaceCell {
            row,
            col,
            grapheme: g.into(),
            fg: None,
            bg: None,
            attrs: Default::default(),
        };
        m.apply(UiEvent::SurfaceCells {
            surface: SurfaceId(1),
            cells: vec![cell(0, 0, "a"), cell(0, 1, "b")],
        });
        m.apply(UiEvent::SurfaceCells { surface: SurfaceId(1), cells: vec![cell(0, 1, "B")] });
        let s = &m.surfaces[&SurfaceId(1)];
        assert_eq!(s.cells.len(), 2, "the untouched cell must survive");
        assert_eq!(s.cells.iter().find(|c| c.col == 1).unwrap().grapheme, "B");
    }

    /// An alert, which is the kind that accumulates. A reply would replace the one before it,
    /// which is exactly what makes it the wrong thing to test a bound with.
    fn news(text: &str) -> UiEvent {
        UiEvent::Message {
            level: neosh_proto::MessageLevel::Info,
            text: text.into(),
            kind: NoticeKind::Alert,
            key: None,
        }
    }

    fn reply(text: &str) -> UiEvent {
        UiEvent::Message {
            level: neosh_proto::MessageLevel::Info,
            text: text.into(),
            kind: NoticeKind::Reply,
            key: None,
        }
    }

    #[test]
    fn the_message_log_stays_bounded() {
        let mut m = Mirror::new();
        for i in 0..500 {
            m.apply(news(&i.to_string()));
        }
        assert_eq!(m.messages.len(), 200);
        assert_eq!(m.messages.last().unwrap().text, "499");
    }

    /// Two keys pressed quickly are two keys, and the answer you want is the one for the second.
    /// The three-high stack was the corner trying to be a log.
    #[test]
    fn a_reply_replaces_the_reply_before_it() {
        let mut m = Mirror::new();
        m.apply(reply("copied /a"));
        m.apply(reply("copied /b"));
        assert_eq!(m.messages.len(), 1);
        assert_eq!(m.messages[0].text, "copied /b");
    }

    /// News is not answering anything, so nothing supersedes it — and a reply arriving must not
    /// push it off either.
    #[test]
    fn news_is_kept_when_a_reply_lands_on_top_of_it() {
        let mut m = Mirror::new();
        m.apply(news("a node went away"));
        m.apply(reply("copied /a"));
        assert_eq!(m.messages.len(), 2);
        assert_eq!(m.messages[0].text, "a node went away");
    }

    /// One sentence arriving five times is that sentence happening five times, not five things to
    /// read.
    #[test]
    fn the_same_message_twice_is_one_row_with_a_count() {
        let mut m = Mirror::new();
        for _ in 0..5 {
            m.apply(news("could not reach the endpoint"));
        }
        assert_eq!(m.messages.len(), 1);
        assert_eq!(m.messages[0].repeats, 5);
    }

    /// The whole point of `Progress`: writing the same key again is one row that changed, which is
    /// what `pulling…` then `up to date` should always have been.
    #[test]
    fn progress_replaces_in_place_and_is_taken_away_by_name() {
        let mut m = Mirror::new();
        let step = |text: &str| UiEvent::Message {
            level: neosh_proto::MessageLevel::Info,
            text: text.into(),
            kind: NoticeKind::Progress,
            key: Some("git.pull".into()),
        };
        m.apply(step("pulling…"));
        m.apply(step("still pulling…"));
        assert_eq!(m.progress.len(), 1, "one row, not two");
        assert_eq!(m.progress["git.pull"].0, "still pulling…");
        // And it never lands in the message list, where it would expire on a timer.
        assert!(m.messages.is_empty());

        m.apply(UiEvent::ProgressDone { key: "git.pull".into() });
        assert!(m.progress.is_empty());
    }

    /// Two operations at once are two rows, each ended by whichever finishes.
    #[test]
    fn two_operations_are_two_progress_rows() {
        let mut m = Mirror::new();
        let step = |key: &str, text: &str| UiEvent::Message {
            level: neosh_proto::MessageLevel::Info,
            text: text.into(),
            kind: NoticeKind::Progress,
            key: Some(key.into()),
        };
        m.apply(step("git.pull", "pulling…"));
        m.apply(step("image:1", "fetching example.com…"));
        assert_eq!(m.progress.len(), 2);
        m.apply(UiEvent::ProgressDone { key: "git.pull".into() });
        assert_eq!(m.progress.len(), 1);
        assert!(m.progress.contains_key("image:1"));
    }

    /// An alert is the host having already decided nobody can see the thing it is about, so the
    /// frontend takes it and writes an escape. Queued rather than acted on here, exactly like the
    /// clipboard, because this type owns no terminal.
    #[test]
    fn an_alert_is_queued_for_the_frontend_to_raise() {
        let mut m = Mirror::new();
        m.apply(UiEvent::Alert {
            title: "fix/the-login".into(),
            body: "finished".into(),
            level: neosh_proto::MessageLevel::Info,
        });
        assert_eq!(m.alerts.len(), 1);
        assert_eq!(m.alerts[0].1, "fix/the-login");
    }
}

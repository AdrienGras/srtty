//! Live dashboard: an inline Ratatui viewport under the cursor, redrawn on every
//! event and on a ticker, behind the `Reporter` trait.

pub mod recap;
pub mod state;
pub mod view;

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::dashboard::state::DashboardState;
use crate::dashboard::view::{plan_layout, render};
use crate::report::{Event, Reporter};

/// Redraw period of the ticker, fast enough for the spinner and the clock.
pub const TICK: Duration = Duration::from_millis(100);

/// How a transcription run shows its progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// `--quiet`: no progress at all.
    Quiet,
    /// One plain line per event on stderr, without ANSI codes.
    Plain,
    /// The live inline viewport on stderr.
    Dashboard,
}

/// Picks the display mode. The dashboard needs both stdout and stderr on a
/// terminal: crossterm writes its cursor position query to stdout, which would
/// end up in a pipe. `--json`, a non-empty `NO_COLOR` or `TERM=dumb` also fall
/// back to plain lines. See design decision 1 of plan 3.
pub fn display_mode(
    quiet: bool,
    json: bool,
    stdout_tty: bool,
    stderr_tty: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> DisplayMode {
    if quiet {
        return DisplayMode::Quiet;
    }
    let no_color = env("NO_COLOR").is_some_and(|v| !v.is_empty());
    let dumb = env("TERM").as_deref() == Some("dumb");
    if json || !stdout_tty || !stderr_tty || no_color || dumb {
        DisplayMode::Plain
    } else {
        DisplayMode::Dashboard
    }
}

/// How the dashboard reaches the terminal; swapped for a `TestBackend` in tests.
pub struct Screen<B: Backend> {
    pub size: Box<dyn Fn() -> std::io::Result<Size> + Send>,
    /// Opens an inline viewport of `height` rows at the cursor.
    pub open: Box<dyn FnMut(u16) -> std::io::Result<Terminal<B>> + Send>,
}

/// The real terminal, drawn on stderr.
pub fn stderr_screen() -> Screen<CrosstermBackend<std::io::Stderr>> {
    Screen {
        size: Box::new(|| {
            let (width, height) = ratatui::crossterm::terminal::size()?;
            Ok(Size::new(width, height))
        }),
        open: Box::new(|height| {
            Terminal::with_options(
                CrosstermBackend::new(std::io::stderr()),
                TerminalOptions {
                    viewport: Viewport::Inline(height),
                },
            )
        }),
    }
}

/// Shows run progress in an inline viewport. Drawing is best effort: terminal
/// errors are ignored and never fail the run.
pub struct Dashboard<B: Backend + Send + 'static> {
    inner: Mutex<Inner<B>>,
    started: Instant,
    /// Dropping the sender stops the ticker thread.
    ticker: Mutex<Option<(Sender<()>, JoinHandle<()>)>>,
}

struct Inner<B: Backend> {
    screen: Screen<B>,
    /// `None` while suspended, or after a viewport failed to open.
    terminal: Option<Terminal<B>>,
    state: DashboardState,
    suspended: bool,
    finished: bool,
    /// A viewport failed to open mid-run: stop drawing rather than query a
    /// terminal that does not answer on every tick.
    broken: bool,
}

impl<B: Backend + Send + 'static> Dashboard<B> {
    /// Opens the first viewport (header and elapsed time); fails if the terminal cannot be opened.
    pub fn open(
        mut screen: Screen<B>,
        input_name: String,
        model: String,
    ) -> std::io::Result<Arc<Self>> {
        let state = DashboardState::new(input_name, model);
        let layout = plan_layout(state.chunks.len(), (screen.size)()?);
        let terminal = (screen.open)(layout.height())?;
        let dashboard = Arc::new(Self {
            inner: Mutex::new(Inner {
                screen,
                terminal: Some(terminal),
                state,
                suspended: false,
                finished: false,
                broken: false,
            }),
            started: Instant::now(),
            ticker: Mutex::new(None),
        });
        dashboard.draw();
        Ok(dashboard)
    }

    /// Redraws every `every` on a background thread until `finish`. Does nothing
    /// once finished or if a ticker already runs.
    pub fn start_ticker(self: &Arc<Self>, every: Duration) {
        let mut ticker = lock(&self.ticker);
        if ticker.is_some() || lock(&self.inner).finished {
            return;
        }
        let (stop, ticks) = mpsc::channel::<()>();
        // A weak handle, so that the ticker alone does not keep the dashboard alive.
        let dashboard = Arc::downgrade(self);
        let handle = std::thread::spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = ticks.recv_timeout(every) {
                match dashboard.upgrade() {
                    Some(d) => d.draw(),
                    None => break,
                }
            }
        });
        *ticker = Some((stop, handle));
    }

    /// Draws one frame now.
    pub fn draw(&self) {
        let now = self.started.elapsed();
        lock(&self.inner).draw(now);
    }
}

impl<B: Backend> Inner<B> {
    fn draw(&mut self, now: Duration) {
        if self.suspended || self.finished || self.broken {
            return;
        }
        let Ok(size) = (self.screen.size)() else {
            return;
        };
        let layout = plan_layout(self.state.chunks.len(), size);
        let height = layout.height();
        // The inline viewport cannot change height in place: erase it and open
        // a new one where it started.
        if self.terminal.as_mut().map(viewport_height) != Some(height) {
            if let Some(terminal) = self.terminal.as_mut() {
                erase(terminal);
            }
            self.terminal = (self.screen.open)(height).ok();
            self.broken = self.terminal.is_none();
        }
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        print_warnings(terminal, self.state.take_warnings());
        let state = &self.state;
        let _ = terminal.draw(|frame| render(state, &layout, now, frame));
    }
}

impl<B: Backend + Send + 'static> Reporter for Dashboard<B> {
    fn event(&self, e: &Event) {
        let now = self.started.elapsed();
        let mut inner = lock(&self.inner);
        inner.state.apply(e, now);
        inner.suspended = false;
        inner.draw(now);
    }

    fn suspend(&self) {
        let mut inner = lock(&self.inner);
        // After the run, the screen below the erased viewport belongs to the recap.
        if inner.finished {
            return;
        }
        if let Some(mut terminal) = inner.terminal.take() {
            erase(&mut terminal);
        }
        inner.suspended = true;
    }

    fn finish(&self) {
        // Stop the ticker before taking the state lock: a ticker blocked on
        // that lock would never let `join` return.
        let ticker = lock(&self.ticker).take();
        if let Some((stop, handle)) = ticker {
            drop(stop);
            let _ = handle.join();
        }
        let mut inner = lock(&self.inner);
        if inner.finished {
            return;
        }
        let Inner {
            terminal, state, ..
        } = &mut *inner;
        // Kept after erasing, so tests can inspect what is left on screen.
        if let Some(terminal) = terminal.as_mut() {
            print_warnings(terminal, state.take_warnings());
            erase(terminal);
        }
        inner.finished = true;
    }
}

impl<B: Backend + Send + 'static> Drop for Dashboard<B> {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().unwrap_or_else(|p| p.into_inner());
        if !inner.finished
            && let Some(terminal) = inner.terminal.as_mut()
        {
            let _ = terminal.show_cursor();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere must not take the display down with it.
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

fn viewport_height<B: Backend>(terminal: &mut Terminal<B>) -> u16 {
    terminal.get_frame().area().height
}

/// Clears the viewport and leaves the cursor visible at its top-left corner,
/// where the next output (or the next viewport) starts.
fn erase<B: Backend>(terminal: &mut Terminal<B>) {
    let _ = terminal.clear();
    let origin = terminal.get_frame().area().as_position();
    let _ = terminal.set_cursor_position(origin);
    let _ = terminal.show_cursor();
}

/// Prints each warning above the viewport, wrapped to the terminal width, where
/// it stays in the terminal history.
fn print_warnings<B: Backend>(terminal: &mut Terminal<B>, warnings: Vec<String>) {
    let width = terminal.get_frame().area().width;
    for message in warnings {
        let paragraph = Paragraph::new(Line::from(vec![
            Span::styled("warning:", Style::default().fg(Color::Yellow)),
            Span::raw(format!(" {message}")),
        ]))
        .wrap(Wrap { trim: true });
        let height = wrapped_height(&paragraph, width, message.chars().count());
        let _ = terminal.insert_before(height, |buf| paragraph.render(buf.area, buf));
    }
}

/// Rows `paragraph` takes at `width`, found by rendering it into a scratch
/// buffer (`Paragraph::line_count` needs an unstable Ratatui feature).
fn wrapped_height(paragraph: &Paragraph, width: u16, chars: usize) -> u16 {
    if width == 0 {
        return 1;
    }
    // Every row holds at least one character; the prefix adds a few more.
    let rows = u16::try_from(chars + "warning: ".len()).unwrap_or(u16::MAX);
    let area = Rect::new(0, 0, width, rows);
    let mut buf = Buffer::empty(area);
    paragraph.render(area, &mut buf);
    (0..rows)
        .rev()
        .find(|&y| (0..width).any(|x| buf[(x, y)].symbol() != " "))
        .map_or(1, |y| y + 1)
}

#[cfg(test)]
impl<B: Backend + Send + 'static> Dashboard<B> {
    fn viewport_height(&self) -> Option<u16> {
        lock(&self.inner).terminal.as_mut().map(viewport_height)
    }
}

#[cfg(test)]
impl Dashboard<ratatui::backend::TestBackend> {
    /// Scrollback then screen of the current backend, one line per row.
    fn screen_text(&self) -> String {
        let mut inner = lock(&self.inner);
        let Some(terminal) = inner.terminal.as_mut() else {
            return String::new();
        };
        let backend = terminal.backend();
        [backend.scrollback(), backend.buffer()]
            .into_iter()
            .flat_map(|buf| {
                buf.content()
                    .chunks(usize::from(buf.area.width.max(1)))
                    .map(|row| row.iter().map(|c| c.symbol()).collect::<String>() + "\n")
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Event, Reporter, Stage};
    use ratatui::backend::TestBackend;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_screen(w: u16, h: u16) -> Screen<TestBackend> {
        Screen {
            size: Box::new(move || Ok(Size::new(w, h))),
            open: Box::new(move |height| {
                Terminal::with_options(
                    TestBackend::new(w, h),
                    TerminalOptions {
                        viewport: Viewport::Inline(height),
                    },
                )
                .map_err(|e| match e {})
            }),
        }
    }

    #[test]
    fn viewport_grows_when_chunks_are_planned() {
        let d = Dashboard::open(test_screen(80, 40), "talk.mkv".into(), "m".into()).unwrap();
        assert_eq!(d.viewport_height(), Some(6));
        d.event(&Event::ChunksPlanned(
            (0..8).map(|i| (i as f64, i as f64 + 1.0)).collect(),
        ));
        assert_eq!(d.viewport_height(), Some(17));
        assert!(d.screen_text().contains("srtty · talk.mkv · m"));
    }

    #[test]
    fn viewport_height_follows_terminal_size() {
        let d = Dashboard::open(test_screen(80, 10), "a".into(), "m".into()).unwrap();
        d.event(&Event::ChunksPlanned(
            (0..20).map(|i| (i as f64, i as f64 + 1.0)).collect(),
        ));
        assert_eq!(d.viewport_height(), Some(1)); // compact
    }

    #[test]
    fn warnings_go_above_the_viewport() {
        let d = Dashboard::open(test_screen(80, 40), "a".into(), "m".into()).unwrap();
        d.event(&Event::Warning("no speech detected".into()));
        assert_eq!(
            d.screen_text()
                .matches("warning: no speech detected")
                .count(),
            1
        );
    }

    #[test]
    fn warnings_queued_while_suspended_are_printed_once() {
        let d = Dashboard::open(test_screen(80, 40), "a".into(), "m".into()).unwrap();
        d.suspend();
        assert_eq!(d.viewport_height(), None);
        d.draw();
        assert_eq!(d.viewport_height(), None); // stays released until the next event
        d.event(&Event::Warning("w1".into()));
        d.draw();
        assert_eq!(d.viewport_height(), Some(6));
        assert_eq!(d.screen_text().matches("warning: w1").count(), 1);
    }

    #[test]
    fn finish_erases_the_viewport_and_is_idempotent() {
        let d = Dashboard::open(test_screen(80, 40), "talk.mkv".into(), "m".into()).unwrap();
        d.start_ticker(TICK);
        d.event(&Event::StageStarted(Stage::Extract));
        d.finish();
        d.finish();
        assert!(!d.screen_text().contains("srtty ·"));
        d.event(&Event::StageStarted(Stage::Split)); // after finish: no redraw
        assert!(!d.screen_text().contains("srtty ·"));
    }

    #[test]
    fn suspend_after_finish_keeps_the_screen() {
        let d = Dashboard::open(test_screen(80, 40), "a".into(), "m".into()).unwrap();
        d.finish();
        d.suspend();
        assert_eq!(d.viewport_height(), Some(6)); // not erased and released again
    }

    #[test]
    fn a_failed_reopen_leaves_the_dashboard_dark() {
        let opens = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&opens);
        let screen = Screen {
            size: Box::new(|| Ok(Size::new(80, 40))),
            open: Box::new(move |height| {
                if counted.fetch_add(1, Ordering::SeqCst) > 0 {
                    return Err(std::io::Error::other("no cursor position"));
                }
                Terminal::with_options(
                    TestBackend::new(80, 40),
                    TerminalOptions {
                        viewport: Viewport::Inline(height),
                    },
                )
                .map_err(|e| match e {})
            }),
        };
        let d = Dashboard::open(screen, "a".into(), "m".into()).unwrap();
        d.event(&Event::ChunksPlanned(
            (0..8).map(|i| (i as f64, i as f64 + 1.0)).collect(),
        ));
        d.draw();
        d.draw();
        d.event(&Event::StageStarted(Stage::Split));
        assert_eq!(opens.load(Ordering::SeqCst), 2);
        assert_eq!(d.viewport_height(), None);
    }

    #[test]
    fn ticker_starts_once_and_never_after_finish() {
        let ticker_id = |d: &Dashboard<TestBackend>| {
            lock(&d.ticker)
                .as_ref()
                .map(|(_, handle)| handle.thread().id())
        };
        let d = Dashboard::open(test_screen(80, 40), "a".into(), "m".into()).unwrap();
        d.start_ticker(TICK);
        let first = ticker_id(&d);
        assert!(first.is_some());
        d.start_ticker(TICK);
        assert_eq!(ticker_id(&d), first);
        d.finish();
        d.start_ticker(TICK);
        assert_eq!(ticker_id(&d), None);
    }

    #[test]
    fn display_mode_rules() {
        let none = |_: &str| None;
        let env =
            |k: &'static str, v: &'static str| move |name: &str| (name == k).then(|| v.to_string());
        assert_eq!(
            display_mode(false, false, true, true, &none),
            DisplayMode::Dashboard
        );
        assert_eq!(
            display_mode(true, false, true, true, &none),
            DisplayMode::Quiet
        );
        assert_eq!(
            display_mode(true, true, false, false, &none),
            DisplayMode::Quiet
        );
        assert_eq!(
            display_mode(false, true, true, true, &none),
            DisplayMode::Plain
        ); // --json
        assert_eq!(
            display_mode(false, false, false, true, &none),
            DisplayMode::Plain
        ); // stdout piped
        assert_eq!(
            display_mode(false, false, true, false, &none),
            DisplayMode::Plain
        ); // stderr redirected
        assert_eq!(
            display_mode(false, false, true, true, &env("NO_COLOR", "1")),
            DisplayMode::Plain
        );
        assert_eq!(
            display_mode(false, false, true, true, &env("NO_COLOR", "")),
            DisplayMode::Dashboard
        );
        assert_eq!(
            display_mode(false, false, true, true, &env("TERM", "dumb")),
            DisplayMode::Plain
        );
    }

    #[test]
    fn long_warnings_wrap() {
        let d = Dashboard::open(test_screen(80, 40), "a".into(), "m".into()).unwrap();
        let message = format!("{} zebra", "word ".repeat(23));
        assert_eq!(message.len(), 121);
        d.event(&Event::Warning(message));
        let text = d.screen_text();
        assert_eq!(text.matches("warning: word").count(), 1);
        assert!(text.contains("zebra"));
    }
}

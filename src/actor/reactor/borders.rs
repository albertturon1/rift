//! Drives JankyBorders from the reactor.
//!
//! After every event that can change what is on screen the reactor asks the layout engine for the border state of
//! the active workspace, applies `layout_engine::border_rule` and tells JankyBorders. Focus commands change the
//! engine's focus before the window server reports anything, so the colours are already right when the new window
//! takes focus: the border never flashes the wrong state. One owner starts JankyBorders, so there is never a second
//! copy (a burst of separate handler processes used to start dozens).

use std::time::{Duration, Instant};

use tracing::debug;

use super::{Event, Reactor};
use crate::layout_engine::border_rule::{self, BorderInput, BorderRule};
use crate::sys::janky_borders::{BordersClient, JankyBorders, SendError};

/// After starting JankyBorders, give it this long to register before starting it again.
const START_GRACE: Duration = Duration::from_secs(3);

/// The two moments the reactor settles the border in one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Sync {
    /// After the model changed, before windows move: always sent.
    Preset,
    /// After the outcome was applied (discoveries and the like can still change the answer): sent only on a change.
    Correct,
    /// The hold timed out: send whatever is decided now, shown or not.
    Release,
}

/// How long a shown border is held back at most for the real focus to arrive.
const HOLD_MS: u64 = 120;

pub(super) struct BordersController {
    client: Box<dyn BordersClient>,
    started_at: Option<Instant>,
    binary: Option<String>,
    last_sent: Option<bool>,
    /// The settings JankyBorders was last given in full (style, width and colours), to resend them only when the
    /// config changes.
    settings_sent: Option<crate::common::config::BordersSettings>,
    /// A timer that will release a held border is already running.
    release_pending: bool,
}

impl Default for BordersController {
    fn default() -> Self {
        Self { client: Box::new(JankyBorders::default()), started_at: None, binary: None, last_sent: None, settings_sent: None, release_pending: false }
    }
}

impl BordersController {
    #[cfg(test)]
    pub(super) fn with_client(client: Box<dyn BordersClient>) -> Self {
        Self { client, started_at: None, binary: None, last_sent: None, settings_sent: None, release_pending: false }
    }

    /// Deliver the decision. A preset always sends, even when nothing changed: it is one message, and it keeps a
    /// JankyBorders that was restarted behind our back in step. A correction only sends when the answer changed.
    ///
    /// Hiding is early and showing is late. A hidden border is sent before the focus moves, so the window that
    /// takes focus never shows the old state. A shown border is held back while the border is hidden and the real
    /// focus has not reached the window the decision is about (`focus_landed`): JankyBorders would draw it around
    /// the window focus is leaving, a flash on the wrong window. The next event, usually the focus report itself,
    /// sends it.
    fn apply(
        &mut self,
        settings: &crate::common::config::BordersSettings,
        show: bool,
        sync: Sync,
        focus_landed: bool,
    ) -> bool {
        if self.binary != settings.binary {
            // The config names another `borders` program (or none any more): talk to that one from now on.
            self.binary = settings.binary.clone();
            self.client = Box::new(JankyBorders { binary: settings.binary.as_deref().map(Into::into) });
            self.settings_sent = None;
        }
        if sync != Sync::Release && show && !focus_landed && self.last_sent == Some(false) {
            return true;
        }
        let configured = self.settings_sent.as_ref() == Some(settings);
        if configured && self.last_sent == Some(show) {
            // Nothing changed. JankyBorders gets one message per change, not one per event: several full
            // messages during one focus move made it redraw late.
            return false;
        }
        self.last_sent = Some(show);
        let args = if configured { settings.colors(show) } else { settings.arguments(show) };
        match self.client.send(&args) {
            Ok(()) => {
                self.settings_sent = Some(settings.clone());
                self.started_at = None;
            }
            Err(SendError::NotRunning) => {
                if self.started_at.is_none_or(|at| at.elapsed() > START_GRACE) {
                    // Start with the visible settings and apply the decision once it is up (see `BordersClient::start`).
                    self.client.start(&settings.arguments(true), &settings.colors(show));
                    self.started_at = Some(Instant::now());
                    self.settings_sent = Some(settings.clone());
                }
            }
            Err(SendError::Failed) => debug!("JankyBorders did not take the message"),
        }
        false
    }
}

impl Reactor {
    /// Set the border for the state the active workspace is in, or is about to be in.
    pub(super) fn sync_borders(&mut self, sync: Sync) {
        let settings = &self.config.settings.borders;
        if !settings.enabled {
            return;
        }
        let Some(space) = self.default_query_space() else {
            return;
        };
        if self.is_fullscreen_space(space) {
            return;
        }
        let Some(state) =
            self.layout_manager.layout_engine.border_state(&self.state.windows, space)
        else {
            return;
        };
        let rule = BorderRule {
            hide_in_stack: settings.hide_in_stack,
            min_visible_windows: settings.min_visible_windows,
        };
        let show = border_rule::should_show(
            &BorderInput { tree: &state.tree, floating: &state.floating, focused: state.focused },
            rule,
        );
        #[cfg(test)]
        self.border_sync_trace.push((self.event_outcome_phase_trace.len(), show));
        let focus_landed = state.focused
            == self.main_window().map(crate::model::broadcast::protocol_window_id);
        let settings = self.config.settings.borders.clone();
        let last = self.borders.last_sent;
        let held = self.borders.apply(&settings, show, sync, focus_landed);
        // Every decision with what it was based on, to find out why a border looked wrong (RUST_LOG=rift_wm=debug).
        debug!(target: "borders", ?sync, show, held, focus_landed, ?last, focused = ?state.focused.map(|w| w.idx),
            main = ?self.main_window().map(|w| w.idx.get()), "border decision");
        if sync == Sync::Release {
            self.borders.release_pending = false;
        }
        // The focus report may never match the window the engine chose (an app whose main window differs from its
        // focused one), so a held border is released after a short time instead of waiting for it forever.
        if held && !self.borders.release_pending && let Some(tx) = self.communication_manager.events_tx.clone() {
            self.borders.release_pending = true;
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(HOLD_MS));
                tx.send(Event::BordersRefresh);
            });
        }
    }
}

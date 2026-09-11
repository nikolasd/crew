//! The pane renderer: a crew-authored, self-formatted view of a
//! protocol-mode turn, wired through the existing
//! `crate::display::attach`/`crate::display::coordinator` machinery
//! unchanged. Claude's own screen does not exist in protocol mode --
//! there is no PTY -- so a viewer attaching to this run's pane sees
//! lines this module writes, one per normalized event, never a byte
//! claude itself painted.
//!
//! **This is a crew-authored report, not a recording of the vendor's
//! own output**, and that distinction matters for two reasons stated
//! plainly rather than left for a reader to infer:
//!
//! - **Provenance.** [`crate::adapter::tui`]'s own pane shows the
//!   vendor's raw PTY bytes verbatim; this pane shows text crew parsed
//!   and formatted from the control channel. A human attached to this
//!   pane is watching crew's report of the turn, not the turn itself --
//!   the banner [`attach_banner`] writes on every attach says so, and a
//!   caller citing this pane as "what claude printed" would be wrong in
//!   a way citing the TUI pane's own text never is.
//! - **Redaction.** The durable journal redacts every free-text field
//!   before it becomes part of a committed `RuntimeEvent` (invariant 4).
//!   This pane's lines are built from the SAME normalized text before
//!   that boundary, so they are NOT redacted -- exactly like the TUI
//!   pane's raw PTY bytes are not redacted either, for the identical
//!   reason (a live view is not the durable record). Recorded here so
//!   the next person asking "is this pane also redacted" finds the
//!   answer already written rather than re-deriving it.
//!
//! `write_input` on [`PaneAttachTarget`] is a documented no-op: this
//! adapter has no path from a viewer's keystroke back into claude's own
//! stdin mid-turn (`declared_capabilities().steering` is already
//! `None`). A keystroke a viewer sends is never silently swallowed,
//! though -- see [`PaneAttachTarget::on_user_input`]'s own doc comment
//! for why it gets an immediate, local, unredacted notice rather than a
//! journaled `OutOfBandInput` (that event asserts the journal may be
//! incomplete, which cannot be true here: nothing the viewer typed ever
//! reached the vendor).

use std::sync::Arc;

use tokio::sync::broadcast;

use crew_protocol::{DisplayBackend, DisplayPlacement, HostProgramHint};

use crate::config::crew::CloseOnExit;
use crate::display::{AttachError, AttachTarget, PaneCoordinator};

/// Everything [`super::adapter::ClaudeProtocolAdapter`] needs to attach
/// a pane for one run -- present only when a [`crate::adapter::tui::TuiSupport`]
/// bundle was ever supplied to the registry (`AdapterRegistry::set_tui_support`);
/// `None` otherwise, in which case the adapter runs with no pane at all
/// rather than refusing to start. Unlike a `TuiAdapter`, this adapter's
/// pane is a convenience view, not its control surface -- the turn
/// completes identically whether or not anyone is watching.
pub(crate) struct PaneSupport {
    pub(crate) pane_coordinator: Arc<PaneCoordinator>,
    pub(crate) panes_dir: std::path::PathBuf,
    pub(crate) placement: DisplayPlacement,
    pub(crate) forced_backend: Option<DisplayBackend>,
    pub(crate) launch_program: Option<HostProgramHint>,
    pub(crate) close_on_exit: CloseOnExit,
}

/// The banner [`PaneAttachTarget`]'s owner writes as the pane's first
/// line, before any turn content -- the operational form of this
/// module's own provenance note: a viewer must not be able to attach
/// and read even one line without already knowing what they are
/// looking at.
pub(crate) fn attach_banner() -> Vec<u8> {
    render_line(
        "crew-rendered view -- not claude's own screen. The vendor's own transcript is the record.",
    )
}

/// Encodes one line for the pane: `text` plus a CRLF terminator. CRLF,
/// not a bare `\n`: `crewd attach`'s own client puts the local terminal
/// into raw mode (`cli.rs`'s `cfmakeraw` wrapper) before pumping bytes,
/// so nothing downstream supplies the carriage return a cooked terminal
/// would have added for free.
pub(crate) fn render_line(text: &str) -> Vec<u8> {
    let mut line = text.as_bytes().to_vec();
    line.extend_from_slice(b"\r\n");
    line
}

/// The [`AttachTarget`] this adapter's pane wiring hands to
/// [`crate::display::AttachServer::start`]. Holds the one
/// `broadcast::Sender` [`super::reader::drive_turn`]'s caller pushes
/// formatted lines into as the turn progresses -- `subscribe_output`
/// hands out receivers off that same sender, exactly
/// `tests/attach.rs`'s own `FakeTarget` precedent for a non-PTY target.
pub(crate) struct PaneAttachTarget {
    output_tx: broadcast::Sender<Vec<u8>>,
}

impl PaneAttachTarget {
    pub(crate) fn new() -> (Self, broadcast::Sender<Vec<u8>>) {
        let (output_tx, _) = broadcast::channel(256);
        (
            Self {
                output_tx: output_tx.clone(),
            },
            output_tx,
        )
    }

    /// The `on_user_input` callback [`crate::display::AttachServer::start`]
    /// takes: writes a single, immediate, local notice back into this
    /// same pane's own broadcast rather than journaling
    /// `RuntimeEvent::OutOfBandInput`. That event's own meaning is "a
    /// human bypassed the adapter, so the journal may not reflect
    /// everything the vendor received" -- untrue here by construction,
    /// since [`AttachTarget::write_input`] on this type never reaches
    /// claude at all. Journaling it anyway would raise a flag asserting
    /// a condition that cannot have occurred; feedback exactly where the
    /// keystroke went is the correct answer, not a misfiled event.
    pub(crate) fn on_user_input(&self) -> Box<dyn Fn(Vec<u8>) + Send + Sync> {
        let output_tx = self.output_tx.clone();
        Box::new(move |_bytes: Vec<u8>| {
            // No alternative path is offered here: this adapter declares
            // `steering: None` and `send` refuses every message kind
            // unconditionally (`declared_capabilities()`'s own doc
            // comment) -- there is no working way to steer this run yet,
            // and the notice must not imply one exists.
            let _ = output_tx.send(render_line(
                "this pane is a read-only view -- what was typed here was discarded, not sent \
                 to claude. This run cannot be steered.",
            ));
        })
    }
}

impl AttachTarget for PaneAttachTarget {
    /// A documented no-op -- see this module's own doc comment for why.
    fn write_input<'a>(
        &'a self,
        _bytes: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AttachError>> + Send + 'a>>
    {
        Box::pin(async move { Ok(()) })
    }

    fn subscribe_output(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_lines_are_crlf_terminated() {
        let line = render_line("hello");
        assert_eq!(line, b"hello\r\n");
    }

    #[tokio::test]
    async fn write_input_is_a_no_op_that_never_errors() {
        let (target, _tx) = PaneAttachTarget::new();
        target.write_input(b"anything".to_vec()).await.unwrap();
    }

    #[tokio::test]
    async fn on_user_input_writes_a_read_only_notice_never_an_out_of_band_event() {
        let (target, _tx) = PaneAttachTarget::new();
        let mut rx = target.subscribe_output();
        let callback = target.on_user_input();
        callback(b"someone typed here".to_vec());
        let received = rx.try_recv().expect("a notice must be sent");
        let text = String::from_utf8(received).unwrap();
        assert!(text.contains("read-only"));
        assert!(text.contains("discarded"));
    }
}

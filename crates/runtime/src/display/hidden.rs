//! Hidden display backend: no pane at all.
//!
//! The one backend that is unconditionally available (spawns no process,
//! touches no filesystem, never fails), and the terminal link in every
//! fallback chain built in this module (`DisplayRegistry::resolve`
//! against a registry built by [`super::DisplayRegistry::with_default_backends`]
//! never returns `selected: None`) and in [`super::coordinator::PaneCoordinator`]'s
//! own hidden-on-failure fallback.
//!
//! Deliberately distinct from the retired `TerminalDisplay`: that was an
//! inert stand-in for "no real backend chosen yet"; this is a real,
//! intentional choice a config or a failed pane creation can land on,
//! and its `create_pane` documents that choice rather than pretending to
//! degrade into a raw terminal rendering.

use crew_protocol::{DisplayBackend, DisplayConfig, DisplayStatus};

use super::{DisplayBackendTrait, DisplayFuture, PaneHandle, PaneRequest};

/// No-pane display backend. Always available; `create_pane` never runs a
/// command, it only returns a handle carrying an empty `pane_ref` so
/// callers can tell a real pane from this one.
pub struct HiddenDisplay {
    #[allow(dead_code)] // carried for parity with the other backends; no field of it is read yet
    config: DisplayConfig,
    active: bool,
}

impl HiddenDisplay {
    #[must_use]
    pub fn new(config: DisplayConfig) -> Self {
        HiddenDisplay {
            config,
            active: false,
        }
    }
}

impl DisplayBackendTrait for HiddenDisplay {
    fn backend_name(&self) -> &str {
        "hidden"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn activate(&mut self) -> Result<(), String> {
        self.active = true;
        Ok(())
    }

    fn status(&self) -> DisplayStatus {
        DisplayStatus {
            backend: DisplayBackend::Hidden,
            available: true,
            active: self.active,
            dimensions: None,
        }
    }

    fn create_pane(&self, req: PaneRequest) -> DisplayFuture<'_, PaneHandle> {
        Box::pin(async move {
            Ok(PaneHandle {
                backend: DisplayBackend::Hidden,
                pane_ref: String::new(),
                placement: req.placement,
            })
        })
    }

    fn close_pane(&self, _handle: &PaneHandle) -> DisplayFuture<'_, ()> {
        // Nothing was ever created, so nothing to refuse or tear down.
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crew_protocol::DisplayPlacement;

    #[test]
    fn always_available_and_names_itself_hidden() {
        let hidden = HiddenDisplay::new(DisplayConfig::default());
        assert!(hidden.is_available());
        assert_eq!(hidden.backend_name(), "hidden");
    }

    #[test]
    fn activation_toggles_status_active() {
        let mut hidden = HiddenDisplay::new(DisplayConfig::default());
        assert!(!hidden.status().active);
        assert!(hidden.activate().is_ok());
        assert!(hidden.status().active);
    }

    #[tokio::test]
    async fn create_pane_never_runs_the_command_and_returns_an_empty_pane_ref() {
        let hidden = HiddenDisplay::new(DisplayConfig::default());
        let handle = hidden
            .create_pane(PaneRequest {
                title: "crew: worker-1 (claude)".to_string(),
                command: vec![
                    "crewd".to_string(),
                    "attach".to_string(),
                    "run-1".to_string(),
                ],
                placement: DisplayPlacement::SplitRight,
                launch_program: None,
            })
            .await
            .expect("hidden pane creation never fails");
        assert_eq!(handle.backend, DisplayBackend::Hidden);
        assert_eq!(handle.pane_ref, "");
    }

    #[tokio::test]
    async fn close_pane_always_succeeds() {
        let hidden = HiddenDisplay::new(DisplayConfig::default());
        let handle = PaneHandle {
            backend: DisplayBackend::Hidden,
            pane_ref: String::new(),
            placement: DisplayPlacement::SplitRight,
        };
        assert!(hidden.close_pane(&handle).await.is_ok());
    }
}

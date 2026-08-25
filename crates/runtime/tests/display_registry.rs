//! Display registry integration tests.

use crew_protocol::{DisplayBackend, DisplayStatus};
use crew_runtime::display::{
    DisplayBackendTrait, DisplayFuture, DisplayRegistry, PaneHandle, PaneRequest,
};

/// Every fake backend below shares the same "no pane support" stub --
/// none of these registry-focused tests exercise pane creation (see
/// `coordinator`'s own tests for that).
fn no_pane_support<'a, T>() -> DisplayFuture<'a, T> {
    Box::pin(async { Err("this fake backend has no pane support".to_string()) })
}

#[test]
fn display_registry_basic() {
    let registry = DisplayRegistry::new();
    assert!(registry.backends().is_empty());
}

#[test]
fn display_registry_register_and_list() {
    let mut registry = DisplayRegistry::new();

    // Register a mock backend
    struct MockBackend;
    impl DisplayBackendTrait for MockBackend {
        fn backend_name(&self) -> &str {
            "mock"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn activate(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn status(&self) -> DisplayStatus {
            DisplayStatus::new(DisplayBackend::Hidden, true, false)
        }
        fn create_pane(&self, _req: PaneRequest) -> DisplayFuture<'_, PaneHandle> {
            no_pane_support()
        }
        fn close_pane(&self, _handle: &PaneHandle) -> DisplayFuture<'_, ()> {
            no_pane_support()
        }
    }

    registry.register(Box::new(MockBackend));
    assert_eq!(registry.backends().len(), 1);
    assert_eq!(registry.backends()[0].backend_name(), "mock");
}

#[test]
fn display_registry_activation_error_surface() {
    let mut registry = DisplayRegistry::new();

    struct FailingBackend {
        activated: bool,
    }
    impl FailingBackend {
        fn new() -> Self {
            FailingBackend { activated: false }
        }
    }
    impl DisplayBackendTrait for FailingBackend {
        fn backend_name(&self) -> &str {
            "failing"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn activate(&mut self) -> Result<(), String> {
            self.activated = true;
            Err("activation failed: permission denied".to_string())
        }
        fn status(&self) -> DisplayStatus {
            DisplayStatus::new(DisplayBackend::Tmux, true, self.activated)
        }
        fn create_pane(&self, _req: PaneRequest) -> DisplayFuture<'_, PaneHandle> {
            no_pane_support()
        }
        fn close_pane(&self, _handle: &PaneHandle) -> DisplayFuture<'_, ()> {
            no_pane_support()
        }
    }

    let mut failing = FailingBackend::new();
    // The activation error surfaces to the caller directly.
    assert_eq!(
        failing.activate(),
        Err("activation failed: permission denied".to_string())
    );
    assert!(failing.activated);

    registry.register(Box::new(failing));
    assert_eq!(registry.backends()[0].backend_name(), "failing");
}

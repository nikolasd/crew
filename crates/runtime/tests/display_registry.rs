//! Display registry integration tests.

use crew_protocol::{DisplayBackend, DisplayStatus};
use crew_runtime::display::{DisplayBackendTrait, DisplayRegistry};

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
            DisplayStatus::new(DisplayBackend::Terminal, true, false)
        }
    }

    registry.register(Box::new(MockBackend));
    assert_eq!(registry.backends().len(), 1);
    assert_eq!(registry.backends()[0].backend_name(), "mock");
}

#[test]
fn display_registry_select_best_favors_available() {
    let mut registry = DisplayRegistry::new();

    // Register an unavailable backend first
    struct UnavailableBackend;
    impl DisplayBackendTrait for UnavailableBackend {
        fn backend_name(&self) -> &str {
            "unavailable"
        }
        fn is_available(&self) -> bool {
            false
        }
        fn activate(&mut self) -> Result<(), String> {
            Err("not available".to_string())
        }
        fn status(&self) -> DisplayStatus {
            DisplayStatus::new(DisplayBackend::Tmux, false, false)
        }
    }

    // Register an available backend second
    struct AvailableBackend;
    impl DisplayBackendTrait for AvailableBackend {
        fn backend_name(&self) -> &str {
            "available"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn activate(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn status(&self) -> DisplayStatus {
            DisplayStatus::new(DisplayBackend::Herdr, true, false)
        }
    }

    registry.register(Box::new(UnavailableBackend));
    registry.register(Box::new(AvailableBackend));

    // select_best should return the available one (first available in order)
    let best = registry.select_best();
    assert!(best.is_some());
    assert_eq!(best.unwrap().backend_name(), "available");
}

#[test]
fn display_registry_select_best_returns_none_when_unavailable() {
    let mut registry = DisplayRegistry::new();

    struct UnavailableBackend;
    impl DisplayBackendTrait for UnavailableBackend {
        fn backend_name(&self) -> &str {
            "unavailable"
        }
        fn is_available(&self) -> bool {
            false
        }
        fn activate(&mut self) -> Result<(), String> {
            Err("not available".to_string())
        }
        fn status(&self) -> DisplayStatus {
            DisplayStatus::new(DisplayBackend::Tmux, false, false)
        }
    }

    registry.register(Box::new(UnavailableBackend));

    let best = registry.select_best();
    assert!(best.is_none());
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
    }

    registry.register(Box::new(FailingBackend::new()));

    // select_best should still return it (it's available)
    let best = registry.select_best();
    assert!(best.is_some());
    assert_eq!(best.unwrap().backend_name(), "failing");
}

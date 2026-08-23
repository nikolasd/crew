//! Display selector tests.

use crew_protocol::{DisplayBackend, DisplayStatus};
use crew_runtime::display::{DisplayBackendTrait, DisplayRegistry, DisplaySelector};

/// Fake backend for display selector tests.
struct FakeBackend {
    name: String,
    available: bool,
}

impl DisplayBackendTrait for FakeBackend {
    fn backend_name(&self) -> &str {
        &self.name
    }

    fn is_available(&self) -> bool {
        self.available
    }

    fn activate(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn status(&self) -> DisplayStatus {
        DisplayStatus::new(
            match self.name.as_str() {
                "herdr" => DisplayBackend::Herdr,
                "tmux" => DisplayBackend::Tmux,
                _ => DisplayBackend::Terminal,
            },
            self.available,
            false,
        )
    }
}

fn make_fake(name: &str, available: bool) -> FakeBackend {
    FakeBackend {
        name: name.to_string(),
        available,
    }
}

#[test]
fn display_selector_selects_first_available() {
    let mut registry = DisplayRegistry::new();
    registry.register(Box::new(make_fake("herdr", true)));
    registry.register(Box::new(make_fake("tmux", true)));

    let selector = DisplaySelector::new(vec![DisplayBackend::Tmux, DisplayBackend::Herdr]);

    let selected = selector.select(&registry);
    assert!(selected.is_some());
    assert_eq!(selected.unwrap().backend_name(), "tmux");
}

#[test]
fn display_selector_falls_back_to_second() {
    let mut registry = DisplayRegistry::new();
    registry.register(Box::new(make_fake("herdr", true)));
    registry.register(Box::new(make_fake("tmux", false)));

    let selector = DisplaySelector::new(vec![DisplayBackend::Tmux, DisplayBackend::Herdr]);

    let selected = selector.select(&registry);
    assert!(selected.is_some());
    assert_eq!(selected.unwrap().backend_name(), "herdr");
}

#[test]
fn display_selector_returns_none_when_nothing_available() {
    let mut registry = DisplayRegistry::new();
    registry.register(Box::new(make_fake("herdr", false)));
    registry.register(Box::new(make_fake("tmux", false)));

    let selector = DisplaySelector::new(vec![DisplayBackend::Tmux, DisplayBackend::Herdr]);

    let selected = selector.select(&registry);
    assert!(selected.is_none());
}

#[test]
fn display_selector_select_index_returns_first_available() {
    let mut registry = DisplayRegistry::new();
    registry.register(Box::new(make_fake("herdr", true)));
    registry.register(Box::new(make_fake("tmux", true)));

    let selector = DisplaySelector::new(vec![DisplayBackend::Tmux, DisplayBackend::Herdr]);

    let index = selector.select_index(&registry);
    assert!(index.is_some());
    assert_eq!(index.unwrap(), 1); // tmux is at index 1
}

#[test]
fn display_selector_select_index_returns_none_when_nothing_available() {
    let mut registry = DisplayRegistry::new();
    registry.register(Box::new(make_fake("herdr", false)));
    registry.register(Box::new(make_fake("tmux", false)));

    let selector = DisplaySelector::new(vec![DisplayBackend::Tmux, DisplayBackend::Herdr]);

    let index = selector.select_index(&registry);
    assert!(index.is_none());
}

#[test]
fn display_selector_prefers_earlier_in_list() {
    let mut registry = DisplayRegistry::new();
    registry.register(Box::new(make_fake("terminal", true)));
    registry.register(Box::new(make_fake("herdr", true)));
    registry.register(Box::new(make_fake("tmux", true)));

    // Prefer tmux first, then herdr, then terminal
    let selector = DisplaySelector::new(vec![
        DisplayBackend::Tmux,
        DisplayBackend::Herdr,
        DisplayBackend::Terminal,
    ]);

    let selected = selector.select(&registry);
    assert!(selected.is_some());
    assert_eq!(selected.unwrap().backend_name(), "tmux");
}

#[test]
fn display_selector_falls_back_through_entire_list() {
    let mut registry = DisplayRegistry::new();
    registry.register(Box::new(make_fake("terminal", true)));

    // Prefer tmux, then herdr, then terminal
    let selector = DisplaySelector::new(vec![
        DisplayBackend::Tmux,
        DisplayBackend::Herdr,
        DisplayBackend::Terminal,
    ]);

    let selected = selector.select(&registry);
    assert!(selected.is_some());
    assert_eq!(selected.unwrap().backend_name(), "terminal");
}

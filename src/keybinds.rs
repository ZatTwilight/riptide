// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};

/// Additional key bindings from config.json. Built-in bindings always remain active.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeybindConfig(pub HashMap<String, Vec<String>>);

const ACTIONS: &[(&str, KeyCode, KeyModifiers)] = &[
    ("quit", KeyCode::Char('q'), KeyModifiers::NONE),
    ("help", KeyCode::Char('?'), KeyModifiers::NONE),
    ("command_palette", KeyCode::Char('/'), KeyModifiers::NONE),
    ("next_tab", KeyCode::Tab, KeyModifiers::NONE),
    ("previous_tab", KeyCode::BackTab, KeyModifiers::SHIFT),
    ("play_pause", KeyCode::Char(' '), KeyModifiers::NONE),
    ("next_track", KeyCode::Char('n'), KeyModifiers::NONE),
    ("previous_track", KeyCode::Char('p'), KeyModifiers::NONE),
    ("toggle_shuffle", KeyCode::Char('z'), KeyModifiers::NONE),
    ("back", KeyCode::Esc, KeyModifiers::NONE),
    ("volume_up", KeyCode::Char('+'), KeyModifiers::NONE),
    ("volume_down", KeyCode::Char('-'), KeyModifiers::NONE),
    ("up", KeyCode::Up, KeyModifiers::NONE),
    ("down", KeyCode::Down, KeyModifiers::NONE),
    ("left", KeyCode::Left, KeyModifiers::NONE),
    ("right", KeyCode::Right, KeyModifiers::NONE),
    ("page_up", KeyCode::PageUp, KeyModifiers::NONE),
    ("page_down", KeyCode::PageDown, KeyModifiers::NONE),
    ("select", KeyCode::Enter, KeyModifiers::NONE),
    ("add_to_queue", KeyCode::Char('a'), KeyModifiers::NONE),
    ("toggle_favorite", KeyCode::Char('f'), KeyModifiers::NONE),
    ("go_to_artist", KeyCode::Char('g'), KeyModifiers::NONE),
    ("sort", KeyCode::Char('s'), KeyModifiers::NONE),
    ("start_radio", KeyCode::Char('r'), KeyModifiers::NONE),
    ("copy_track", KeyCode::Char('c'), KeyModifiers::NONE),
    ("copy_collection", KeyCode::Char('C'), KeyModifiers::SHIFT),
    ("remove_from_queue", KeyCode::Char('d'), KeyModifiers::NONE),
    ("move_queue_up", KeyCode::Up, KeyModifiers::CONTROL),
    ("move_queue_down", KeyCode::Down, KeyModifiers::CONTROL),
];

impl KeybindConfig {
    /// Translate configured keys to the built-in key used by the existing event handlers.
    /// Character remapping is disabled while entering text so `j`, `k`, etc. remain typeable.
    pub fn remap(&self, key: KeyEvent, entering_text: bool) -> KeyEvent {
        if entering_text && matches!(key.code, KeyCode::Char(_)) {
            return key;
        }

        if !entering_text {
            let vim_code = match key.code {
                KeyCode::Char('h') => Some(KeyCode::Left),
                KeyCode::Char('j') => Some(KeyCode::Down),
                KeyCode::Char('k') => Some(KeyCode::Up),
                KeyCode::Char('l') => Some(KeyCode::Right),
                _ => None,
            };
            if let Some(code) = vim_code {
                return KeyEvent::new(code, KeyModifiers::NONE);
            }
        }

        // Additive bindings must not steal an existing built-in key from its action.
        if ACTIONS
            .iter()
            .any(|(_, code, modifiers)| key_matches_target(key, code, *modifiers))
        {
            return key;
        }

        for &(action, ref target_code, target_modifiers) in ACTIONS {
            let Some(keys) = self.0.get(action) else {
                continue;
            };
            if keys.iter().any(|spec| key_matches_spec(key, spec)) {
                return KeyEvent::new(target_code.clone(), target_modifiers);
            }
        }

        key
    }

    pub fn unknown_actions(&self) -> Vec<&str> {
        self.0
            .keys()
            .filter(|action| !ACTIONS.iter().any(|(known, _, _)| known == action))
            .map(String::as_str)
            .collect()
    }
}

fn key_matches_target(key: KeyEvent, code: &KeyCode, modifiers: KeyModifiers) -> bool {
    key.code == *code
        && key.modifiers.contains(modifiers)
        && (!modifiers.is_empty()
            || !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT))
}

fn key_matches_spec(key: KeyEvent, spec: &str) -> bool {
    let parts: Vec<&str> = spec.split('+').collect();
    let key_name = parts.last().copied().unwrap_or_default();
    let mut modifiers = KeyModifiers::NONE;
    for modifier in &parts[..parts.len().saturating_sub(1)] {
        match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.insert(KeyModifiers::CONTROL),
            "alt" => modifiers.insert(KeyModifiers::ALT),
            "shift" => modifiers.insert(KeyModifiers::SHIFT),
            _ => return false,
        }
    }

    let code_matches = match key_name.to_ascii_lowercase().as_str() {
        "up" => key.code == KeyCode::Up,
        "down" => key.code == KeyCode::Down,
        "left" => key.code == KeyCode::Left,
        "right" => key.code == KeyCode::Right,
        "enter" | "return" => key.code == KeyCode::Enter,
        "esc" | "escape" => key.code == KeyCode::Esc,
        "tab" => key.code == KeyCode::Tab,
        "backtab" => key.code == KeyCode::BackTab,
        "space" => key.code == KeyCode::Char(' '),
        "pageup" | "page_up" => key.code == KeyCode::PageUp,
        "pagedown" | "page_down" => key.code == KeyCode::PageDown,
        "backspace" => key.code == KeyCode::Backspace,
        "delete" | "del" => key.code == KeyCode::Delete,
        _ => {
            let mut chars = key_name.chars();
            match (chars.next(), chars.next(), key.code) {
                (Some(expected), None, KeyCode::Char(actual))
                    if modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    actual.eq_ignore_ascii_case(&expected)
                }
                (Some(expected), None, KeyCode::Char(actual)) => actual == expected,
                _ => false,
            }
        }
    };

    code_matches
        && key.modifiers.contains(modifiers)
        && (!modifiers.is_empty()
            || !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vim_navigation_is_enabled_by_default() {
        let bindings = KeybindConfig::default();
        assert_eq!(
            bindings
                .remap(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), false)
                .code,
            KeyCode::Down
        );
        assert_eq!(
            bindings
                .remap(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), false)
                .code,
            KeyCode::Up
        );
    }

    #[test]
    fn characters_are_not_remapped_during_text_entry() {
        let bindings = KeybindConfig::default();
        assert_eq!(
            bindings
                .remap(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), true)
                .code,
            KeyCode::Char('j')
        );
    }

    #[test]
    fn configured_bindings_are_additive() {
        let bindings = KeybindConfig(HashMap::from([(
            "quit".to_string(),
            vec!["ctrl+x".to_string()],
        )]));
        let remapped = bindings.remap(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            false,
        );
        assert_eq!(remapped.code, KeyCode::Char('q'));
    }

    #[test]
    fn configured_binding_does_not_steal_a_builtin_key() {
        let bindings = KeybindConfig(HashMap::from([("up".to_string(), vec!["q".to_string()])]));
        let remapped = bindings.remap(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), false);
        assert_eq!(remapped.code, KeyCode::Char('q'));
    }
}

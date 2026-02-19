use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};

const DEFAULT_SYNC_INTERVAL_SECONDS: u64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub keybindings: Keybindings,
    #[serde(default)]
    pub signatures: Signatures,
    #[serde(default = "default_sync_interval_seconds")]
    pub sync_interval_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Signatures {
    pub new_message: Option<String>,
    pub reply: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybindings {
    pub next_panel: Vec<String>,
    pub prev_panel: Vec<String>,
    pub move_up: Vec<String>,
    pub move_down: Vec<String>,
    pub mark_read: Vec<String>,
    pub new_message: Vec<String>,
    pub reply: Vec<String>,
    pub forward: Vec<String>,
    pub delete: Vec<String>,
    pub archive: Vec<String>,
    pub send_message: Vec<String>,
    pub quit: Vec<String>,
    pub undo: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keybindings: Keybindings {
                next_panel: keys(&["l", "Right", "Tab"]),
                prev_panel: keys(&["h", "Left", "BackTab"]),
                move_up: keys(&["k", "Up"]),
                move_down: keys(&["j", "Down"]),
                mark_read: keys(&[" "]),
                new_message: keys(&["n"]),
                reply: keys(&["r"]),
                forward: keys(&["f"]),
                delete: keys(&["Backspace", "d"]),
                archive: keys(&["a"]),
                send_message: keys(&["ctrl-s"]),
                quit: keys(&["q"]),
                undo: keys(&["u"]),
            },
            signatures: Signatures::default(),
            sync_interval_seconds: DEFAULT_SYNC_INTERVAL_SECONDS,
        }
    }
}

fn keys(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|s| (*s).to_string()).collect()
}

fn default_sync_interval_seconds() -> u64 {
    DEFAULT_SYNC_INTERVAL_SECONDS
}

impl Config {
    pub fn load() -> Self {
        std::fs::read_to_string("settings.toml")
            .ok()
            .and_then(|content| toml::from_str(&content).ok())
            .unwrap_or_default()
    }
}

/// Parse a key binding string like `"ctrl-s"` into a `(KeyCode, KeyModifiers)` pair.
pub fn parse_key_string(key_str: &str) -> (KeyCode, KeyModifiers) {
    let parts: Vec<&str> = key_str.split('-').collect();
    let (modifiers_parts, base_key_str) = parts.split_at(parts.len().saturating_sub(1));

    let mut modifiers = KeyModifiers::empty();
    for part in modifiers_parts {
        match part.to_lowercase().as_str() {
            "ctrl" => modifiers.insert(KeyModifiers::CONTROL),
            "alt" => modifiers.insert(KeyModifiers::ALT),
            "shift" => modifiers.insert(KeyModifiers::SHIFT),
            "cmd" | "command" | "super" => modifiers.insert(KeyModifiers::SUPER),
            "meta" => modifiers.insert(KeyModifiers::META),
            _ => {}
        }
    }

    let base = base_key_str.first().copied().unwrap_or("");
    let code = match base {
        "Backspace" => KeyCode::Backspace,
        "Enter" => KeyCode::Enter,
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Tab" => KeyCode::Tab,
        "BackTab" => KeyCode::BackTab,
        "Esc" => KeyCode::Esc,
        " " => KeyCode::Char(' '),
        s if s.len() == 1 => KeyCode::Char(s.chars().next().unwrap()),
        _ => KeyCode::Null,
    };

    (code, modifiers)
}

/// Check whether a key event matches any of the given binding strings.
pub fn matches_key(event: KeyEvent, bindings: &[String]) -> bool {
    bindings.iter().any(|b| {
        let (code, modifiers) = parse_key_string(b);
        event.code == code && event.modifiers == modifiers
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_key_requires_exact_modifiers() {
        let event = KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        let bindings = vec!["ctrl-s".to_string()];

        assert!(!matches_key(event, &bindings));
    }

    #[test]
    fn default_sync_interval_is_30() {
        let config = Config::default();
        assert_eq!(config.sync_interval_seconds, 30);
    }
}

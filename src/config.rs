use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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

/// A set of key bindings parsed eagerly from string definitions.
///
/// Deserializes from `Vec<String>` (e.g. `["ctrl-s", "Enter"]`) and stores
/// pre-parsed `(KeyCode, KeyModifiers)` pairs so that `matches_key` never
/// has to re-parse at runtime.
#[derive(Debug, Clone)]
pub struct KeyBindingSet(Vec<(KeyCode, KeyModifiers)>);

impl KeyBindingSet {
    fn from_strs(entries: &[&str]) -> Self {
        Self(entries.iter().map(|s| parse_key_string(s)).collect())
    }
}

impl Serialize for KeyBindingSet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Round-trip as Vec<String> is not needed in practice, but keeps
        // the Serialize derive on Keybindings working for debug/test.
        let strings: Vec<String> = self
            .0
            .iter()
            .map(|(code, mods)| format_key_binding(*code, *mods))
            .collect();
        strings.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for KeyBindingSet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let strings: Vec<String> = Vec::deserialize(deserializer)?;
        Ok(Self(strings.iter().map(|s| parse_key_string(s)).collect()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybindings {
    pub next_panel: KeyBindingSet,
    pub prev_panel: KeyBindingSet,
    pub move_up: KeyBindingSet,
    pub move_down: KeyBindingSet,
    pub mark_read: KeyBindingSet,
    pub new_message: KeyBindingSet,
    pub reply: KeyBindingSet,
    pub forward: KeyBindingSet,
    pub delete: KeyBindingSet,
    pub archive: KeyBindingSet,
    pub send_message: KeyBindingSet,
    pub quit: KeyBindingSet,
    pub undo: KeyBindingSet,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keybindings: Keybindings {
                next_panel: KeyBindingSet::from_strs(&["l", "Right", "Tab"]),
                prev_panel: KeyBindingSet::from_strs(&["h", "Left", "BackTab"]),
                move_up: KeyBindingSet::from_strs(&["k", "Up"]),
                move_down: KeyBindingSet::from_strs(&["j", "Down"]),
                mark_read: KeyBindingSet::from_strs(&[" "]),
                new_message: KeyBindingSet::from_strs(&["n"]),
                reply: KeyBindingSet::from_strs(&["r"]),
                forward: KeyBindingSet::from_strs(&["f"]),
                delete: KeyBindingSet::from_strs(&["Backspace", "d"]),
                archive: KeyBindingSet::from_strs(&["a"]),
                send_message: KeyBindingSet::from_strs(&["ctrl-s"]),
                quit: KeyBindingSet::from_strs(&["q"]),
                undo: KeyBindingSet::from_strs(&["u"]),
            },
            signatures: Signatures::default(),
            sync_interval_seconds: DEFAULT_SYNC_INTERVAL_SECONDS,
        }
    }
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
fn parse_key_string(key_str: &str) -> (KeyCode, KeyModifiers) {
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

/// Format a (KeyCode, KeyModifiers) pair back to a string for serialization.
fn format_key_binding(code: KeyCode, modifiers: KeyModifiers) -> String {
    let mut parts = Vec::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl".to_string());
    }
    if modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt".to_string());
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift".to_string());
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        parts.push("super".to_string());
    }
    if modifiers.contains(KeyModifiers::META) {
        parts.push("meta".to_string());
    }

    let key = match code {
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "BackTab".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Char(c) => c.to_string(),
        _ => "Unknown".to_string(),
    };
    parts.push(key);
    parts.join("-")
}

/// Check whether a key event matches any binding in the set.
pub fn matches_key(event: KeyEvent, bindings: &KeyBindingSet) -> bool {
    bindings
        .0
        .iter()
        .any(|(code, modifiers)| event.code == *code && event.modifiers == *modifiers)
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
        let bindings = KeyBindingSet::from_strs(&["ctrl-s"]);

        assert!(!matches_key(event, &bindings));
    }

    #[test]
    fn default_sync_interval_is_30() {
        let config = Config::default();
        assert_eq!(config.sync_interval_seconds, 30);
    }

    #[test]
    fn keybinding_set_deserializes_from_strings() {
        let json = r#"["ctrl-s", "Enter", "a"]"#;
        let set: KeyBindingSet = serde_json::from_str(json).unwrap();
        assert_eq!(set.0.len(), 3);
        assert_eq!(set.0[0], (KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(set.0[1], (KeyCode::Enter, KeyModifiers::empty()));
        assert_eq!(set.0[2], (KeyCode::Char('a'), KeyModifiers::empty()));
    }
}

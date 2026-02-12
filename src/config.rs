use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub keybindings: Keybindings,
    #[serde(default)]
    pub signatures: Signatures,
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
                next_panel: vec!["l".to_string(), "Right".to_string(), "Tab".to_string()],
                prev_panel: vec!["h".to_string(), "Left".to_string(), "BackTab".to_string()],
                move_up: vec!["k".to_string(), "Up".to_string()],
                move_down: vec!["j".to_string(), "Down".to_string()],
                mark_read: vec![" ".to_string()],
                new_message: vec!["n".to_string()],
                reply: vec!["r".to_string()],
                forward: vec!["f".to_string()],
                delete: vec!["Backspace".to_string(), "d".to_string()],
                archive: vec!["a".to_string()],
                send_message: vec!["ctrl-s".to_string()],
                quit: vec!["q".to_string()],
                undo: vec!["u".to_string()],
            },
            signatures: Signatures::default(),
        }
    }
}

pub fn parse_key_string(key_str: &str) -> (KeyCode, KeyModifiers) {
    let mut parts: Vec<&str> = key_str.split('-').collect();
    let mut modifiers = KeyModifiers::empty();

    // We process from the end to find the base key, then consume prefixes
    let base_key_str = parts.pop().unwrap_or("");

    for part in parts {
        match part.to_lowercase().as_str() {
            "ctrl" => modifiers.insert(KeyModifiers::CONTROL),
            "alt" => modifiers.insert(KeyModifiers::ALT),
            "shift" => modifiers.insert(KeyModifiers::SHIFT),
            "cmd" | "command" | "super" => modifiers.insert(KeyModifiers::SUPER),
            "meta" => modifiers.insert(KeyModifiers::META),
            _ => {}
        }
    }

    let code = match base_key_str {
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

pub fn matches_key(event: KeyEvent, bindings: &[String]) -> bool {
    bindings.iter().any(|b| {
        let (code, modifiers) = parse_key_string(b);
        event.code == code && event.modifiers.contains(modifiers)
    })
}

impl Config {
    pub fn load() -> Self {
        use std::fs;
        if let Ok(content) = fs::read_to_string("settings.toml") {
            if let Ok(config) = toml::from_str(&content) {
                return config;
            }
        }
        Self::default()
    }
}

use crossterm::event::KeyCode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub keybindings: Keybindings,
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
    pub delete: Vec<String>,
    pub archive: Vec<String>,
    pub quit: Vec<String>,
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
                delete: vec!["Backspace".to_string(), "d".to_string()],
                archive: vec!["a".to_string()],
                quit: vec!["q".to_string()],
            },
        }
    }
}

pub fn parse_key(key: &str) -> KeyCode {
    match key {
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
    }
}

pub fn matches_key(key: KeyCode, bindings: &[String]) -> bool {
    bindings.iter().any(|b| parse_key(b) == key)
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

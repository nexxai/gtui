use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::cell::RefCell;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastPosition {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
    Middle,
}

pub struct Toast {
    pub text: String,
    pub position: ToastPosition,
    pub style: Style,
    pub duration: Duration,
    pub action: Option<Box<dyn FnMut()>>,
    shown_at: RefCell<Option<Instant>>,
}

impl Default for Toast {
    fn default() -> Self {
        Self {
            text: String::new(),
            position: ToastPosition::TopCenter,
            style: Style::default().fg(Color::White).bg(Color::Black),
            duration: Duration::from_secs(3),
            action: None,
            shown_at: RefCell::new(None),
        }
    }
}

impl Toast {
    pub fn new(text: impl Into<String>, position: ToastPosition) -> Self {
        Self {
            text: text.into(),
            position,
            style: Style::default().fg(Color::White).bg(Color::Black),
            duration: Duration::from_secs(3),
            action: None,
            shown_at: RefCell::new(None),
        }
    }

    pub fn elapsed(&self) -> Option<Duration> {
        self.shown_at.borrow().map(|t| t.elapsed())
    }

    fn calculate_area(&self, area: Rect) -> Rect {
        let width = (area.width as f32 * 0.8).clamp(20.0, 60.0) as u16;
        let height = 3; // Fixed height for toast

        let x = match self.position {
            ToastPosition::TopLeft | ToastPosition::BottomLeft => 1,
            ToastPosition::TopCenter | ToastPosition::BottomCenter | ToastPosition::Middle => {
                (area.width.saturating_sub(width)) / 2
            }
            ToastPosition::TopRight | ToastPosition::BottomRight => {
                area.width.saturating_sub(width + 1)
            }
        };

        let y = match self.position {
            ToastPosition::TopLeft | ToastPosition::TopCenter | ToastPosition::TopRight => 1,
            ToastPosition::Middle => (area.height.saturating_sub(height)) / 2,
            ToastPosition::BottomLeft
            | ToastPosition::BottomCenter
            | ToastPosition::BottomRight => area.height.saturating_sub(height + 1),
        };

        Rect {
            x,
            y,
            width,
            height,
        }
    }
}

impl ratatui::widgets::Widget for &Toast {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        if self.text.is_empty() {
            return;
        }

        // Set shown_at on first render
        if self.shown_at.borrow().is_none() {
            *self.shown_at.borrow_mut() = Some(Instant::now());
        }

        let toast_area = self.calculate_area(area);

        // Clear the area first
        Clear.render(toast_area, buf);

        // Create the toast paragraph
        let paragraph = Paragraph::new(self.text.as_str())
            .style(self.style)
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Gray)),
            );

        paragraph.render(toast_area, buf);
    }
}

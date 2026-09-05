use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Clear, Paragraph},
};
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
    shown_at: Instant,
}

impl Toast {
    pub fn new(text: impl Into<String>, position: ToastPosition) -> Self {
        Self {
            text: text.into(),
            position,
            style: Style::default().fg(Color::White).bg(Color::Black),
            duration: Duration::from_secs(3),
            shown_at: Instant::now(),
        }
    }

    pub fn is_expired(&self) -> bool {
        self.shown_at.elapsed() >= self.duration
    }

    fn calculate_area(&self, area: Rect) -> Rect {
        let width = area.width.saturating_mul(4).saturating_div(5).min(60);
        let height = area.height.min(3);
        let horizontal_padding = area.width.saturating_sub(width).min(1);
        let vertical_padding = area.height.saturating_sub(height).min(1);

        let x = match self.position {
            ToastPosition::TopLeft | ToastPosition::BottomLeft => area.x + horizontal_padding,
            ToastPosition::TopCenter | ToastPosition::BottomCenter | ToastPosition::Middle => {
                area.x + area.width.saturating_sub(width) / 2
            }
            ToastPosition::TopRight | ToastPosition::BottomRight => {
                area.x + area.width.saturating_sub(width + horizontal_padding)
            }
        };

        let y = match self.position {
            ToastPosition::TopLeft | ToastPosition::TopCenter | ToastPosition::TopRight => {
                area.y + vertical_padding
            }
            ToastPosition::Middle => area.y + area.height.saturating_sub(height) / 2,
            ToastPosition::BottomLeft
            | ToastPosition::BottomCenter
            | ToastPosition::BottomRight => {
                area.y + area.height.saturating_sub(height + vertical_padding)
            }
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

        let toast_area = self.calculate_area(area);

        Clear.render(toast_area, buf);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_area_stays_within_its_parent() {
        let toast = Toast::new("test", ToastPosition::BottomRight);
        let area = Rect::new(5, 7, 10, 2);
        let toast_area = toast.calculate_area(area);

        assert!(toast_area.x >= area.x);
        assert!(toast_area.y >= area.y);
        assert!(toast_area.right() <= area.right());
        assert!(toast_area.bottom() <= area.bottom());
    }
}

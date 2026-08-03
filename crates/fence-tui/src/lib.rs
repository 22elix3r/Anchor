//! Terminal review frontend for Fence.
//!
//! This crate is published as an implementation layer for the `fence` CLI. Its
//! Rust API is prerelease and may change between `0.1.0-alpha.N` versions.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewModel {
    pub title: String,
    pub files: Vec<ReviewFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewFile {
    pub path: String,
    pub status: String,
    pub before: ReviewContent,
    pub after: ReviewContent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewContent {
    Text(Vec<String>),
    Binary { size: u64 },
    Absent,
    Description(String),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct AppState {
    selected: usize,
    vertical_scroll: u16,
    horizontal_scroll: u16,
}

/// Run the reviewer in the current terminal until `q` or Escape is pressed.
///
/// # Errors
///
/// Returns [`ReviewError`] when terminal setup, drawing, or event handling fails.
pub fn review(model: &ReviewModel) -> Result<ReviewAction, ReviewError> {
    if model.files.is_empty() {
        return Err(ReviewError::NoChanges);
    }
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let _guard = TerminalGuard;
    terminal.clear()?;

    let mut state = AppState::default();
    loop {
        terminal.draw(|frame| render(frame, model, state))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(ReviewAction::Quit),
            KeyCode::Char('r') => {
                return Ok(ReviewAction::RestoreSelected {
                    index: state.selected,
                });
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.selected = (state.selected + 1).min(model.files.len() - 1);
                state.vertical_scroll = 0;
                state.horizontal_scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.selected = state.selected.saturating_sub(1);
                state.vertical_scroll = 0;
                state.horizontal_scroll = 0;
            }
            KeyCode::PageDown => {
                state.vertical_scroll = state.vertical_scroll.saturating_add(20);
            }
            KeyCode::PageUp => {
                state.vertical_scroll = state.vertical_scroll.saturating_sub(20);
            }
            KeyCode::Right | KeyCode::Char('l') => {
                state.horizontal_scroll = state.horizontal_scroll.saturating_add(4);
            }
            KeyCode::Left | KeyCode::Char('h') => {
                state.horizontal_scroll = state.horizontal_scroll.saturating_sub(4);
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewAction {
    Quit,
    RestoreSelected { index: usize },
}

fn render(frame: &mut ratatui::Frame<'_>, model: &ReviewModel, state: AppState) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    let help =
        Paragraph::new("q quit  r restore selected  j/k file  ←/→ horizontal  PgUp/PgDn vertical")
            .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(help, outer[1]);

    let file = &model.files[state.selected];
    if outer[0].width >= 100 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(24),
                Constraint::Percentage(38),
                Constraint::Percentage(38),
            ])
            .split(outer[0]);
        render_files(frame, columns[0], model, state.selected);
        render_content(frame, columns[1], "Before", &file.before, state);
        render_content(frame, columns[2], "Session end", &file.after, state);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(35),
                Constraint::Percentage(32),
                Constraint::Percentage(33),
            ])
            .split(outer[0]);
        render_files(frame, rows[0], model, state.selected);
        render_content(frame, rows[1], "Before", &file.before, state);
        render_content(frame, rows[2], "Session end", &file.after, state);
    }
}

fn render_files(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    model: &ReviewModel,
    selected: usize,
) {
    let items = model
        .files
        .iter()
        .map(|file| ListItem::new(format!("{}  {}", file.status, file.path)))
        .collect::<Vec<_>>();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(model.title.as_str()),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut list_state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_content(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    title: &str,
    content: &ReviewContent,
    state: AppState,
) {
    let text = match content {
        ReviewContent::Text(lines) => Text::from(
            lines
                .iter()
                .map(|line| Line::raw(line.clone()))
                .collect::<Vec<_>>(),
        ),
        ReviewContent::Binary { size } => Text::from(format!("Binary file ({size} bytes)")),
        ReviewContent::Absent => Text::from("(absent)"),
        ReviewContent::Description(value) => Text::from(value.clone()),
    };
    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((state.vertical_scroll, state.horizontal_scroll))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _raw = disable_raw_mode();
        let _screen = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[derive(Debug, Error)]
pub enum ReviewError {
    #[error("session has no file changes to review")]
    NoChanges,
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn renders_narrow_and_wide_layouts() {
        let model = ReviewModel {
            title: "Session".to_owned(),
            files: vec![ReviewFile {
                path: "src/main.rs".to_owned(),
                status: "M".to_owned(),
                before: ReviewContent::Text(vec!["before".to_owned()]),
                after: ReviewContent::Text(vec!["after".to_owned()]),
            }],
        };
        for (width, height) in [(80, 24), (140, 40)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| render(frame, &model, AppState::default()))
                .unwrap();
            let rendered = terminal.backend().buffer().content().to_vec();
            assert!(rendered.iter().any(|cell| cell.symbol() == "b"));
            let text = rendered
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(text.contains("r restore selected"));
        }
    }
}

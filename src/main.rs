use std::{
    env, io,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyEventKind};
use jot_cli::{App, Focus, Update, parse_args};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

fn main() -> io::Result<()> {
    let data_path = match parse_args(env::args()) {
        Ok(path) => path,
        Err(message) if message.starts_with("Usage:") => {
            println!("{message}");
            return Ok(());
        }
        Err(message) => {
            eprintln!("{message}");
            return Ok(());
        }
    };

    let store = jot_cli::Store::load(&data_path)?;
    let mut app = App::new(store);

    let terminal = ratatui::init();
    let result = run(terminal, &mut app, &data_path);
    ratatui::restore();
    result
}

fn run(
    mut terminal: DefaultTerminal,
    app: &mut App,
    data_path: &std::path::Path,
) -> io::Result<()> {
    let tick_rate = Duration::from_millis(250);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|frame| draw(frame, app))?;

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            match app.handle_key(key) {
                Update::Quit => {
                    app.store.save(data_path)?;
                    return Ok(());
                }
                Update::Save => app.store.save(data_path)?,
                Update::None => {}
            }
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
        }
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(frame.area());

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(24), Constraint::Min(10)])
        .split(layout[0]);

    let workspaces_focused = app.focus == Focus::Workspaces;
    let tasks_focused = app.focus == Focus::Tasks;

    let workspace_items = app
        .store
        .workspaces
        .iter()
        .enumerate()
        .map(|(index, workspace)| {
            let label = format!("{} ({})", workspace.name, workspace.items.len());
            if index == app.store.selected_workspace {
                ListItem::new(label).style(selection_style(workspaces_focused))
            } else {
                ListItem::new(label)
            }
        })
        .collect::<Vec<_>>();

    let move_state = match &app.mode {
        jot_cli::Mode::Moving { origin, as_child } => Some((origin.clone(), *as_child)),
        _ => None,
    };

    let items = app
        .flattened_items()
        .into_iter()
        .map(|item| {
            let indent = "  ".repeat(item.depth);
            let selected = app.selected_path.as_ref() == Some(&item.path);
            let is_origin = move_state
                .as_ref()
                .map(|(origin, _)| origin == &item.path)
                .unwrap_or(false);

            // Leading glyph: ⇅ while this item is the one being moved,
            // ▼ when its children are folded away, otherwise blank.
            let lead = if is_origin {
                "⇅"
            } else if item.has_children && item.folded {
                "▼"
            } else {
                " "
            };

            let row_style = if selected {
                selection_style(tasks_focused)
            } else if is_origin {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };

            // White circle for open tasks, green ✗ for completed — both bold.
            // On the highlighted row the symbol adopts the row color so it
            // stays legible against the selection background.
            let symbol = if item.done { "✗" } else { "○" };
            let symbol_style = if selected {
                row_style.add_modifier(Modifier::BOLD)
            } else if item.done {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            };

            let mut spans = vec![
                Span::raw(format!("{indent}{lead} ")),
                Span::styled(symbol, symbol_style),
                Span::raw(format!(" {}", item.title)),
            ];

            // While moving, the selected row is the drop target. Show where the
            // item will land, and an arrow when it will nest as a child.
            if selected && let Some((_, as_child)) = &move_state {
                let hint = if *as_child {
                    "  ↳ as child"
                } else {
                    "  ← insert after"
                };
                spans.push(Span::styled(
                    hint,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            }

            ListItem::new(Line::from(spans)).style(row_style)
        })
        .collect::<Vec<_>>();

    let workspace_title = format!("Workspace: {}", app.current_workspace().name);
    frame.render_widget(
        List::new(workspace_items)
            .block(focus_block("Workspaces", workspaces_focused)),
        columns[0],
    );
    frame.render_widget(
        List::new(items).block(focus_block(workspace_title, tasks_focused)),
        columns[1],
    );

    let status = match &app.mode {
        jot_cli::Mode::Normal | jot_cli::Mode::ConfirmDelete | jot_cli::Mode::Moving { .. } => {
            app.status.clone()
        }
        jot_cli::Mode::Editing { input, .. } => format!("Input: {input}"),
    };
    frame.render_widget(
        Paragraph::new(status).block(Block::default().title("Status").borders(Borders::ALL)),
        layout[1],
    );

    if let jot_cli::Mode::Editing { target, input } = &app.mode {
        let prompt = match target {
            jot_cli::EditTarget::NewWorkspace => "New workspace",
            jot_cli::EditTarget::NewSibling => "New item",
            jot_cli::EditTarget::NewChild => "New child item",
            jot_cli::EditTarget::RenameSelected => "Rename item",
        };

        let popup = centered_rect(frame.area(), 60, 20);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(input.clone())
                .block(Block::default().title(prompt).borders(Borders::ALL)),
            popup,
        );
    }
}

/// Highlight the selected row; brighter when its panel currently has focus.
fn selection_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::REVERSED)
    }
}

/// A bordered block whose border is highlighted when its panel has focus.
fn focus_block(title: impl Into<String>, focused: bool) -> Block<'static> {
    let border_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .title(title.into())
        .borders(Borders::ALL)
        .border_style(border_style)
}

fn centered_rect(
    area: ratatui::layout::Rect,
    width_percent: u16,
    height_percent: u16,
) -> ratatui::layout::Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

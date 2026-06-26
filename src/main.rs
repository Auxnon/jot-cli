use std::{
    env, io,
    time::{Duration, Instant},
};

use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    cursor::MoveToColumn,
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement},
};
use jot_cli::{App, CliArgs, Focus, Mode, Update, parse_args};
use ratatui::{
    DefaultTerminal, Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

fn main() -> io::Result<()> {
    let args = match parse_args(env::args()) {
        Ok(args) => args,
        Err(message) if message.starts_with("Usage:") => {
            println!("{message}");
            return Ok(());
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };

    let mut store = jot_cli::Store::load(&args.data_path)?;

    // `--add` is a one-shot command-line action: add the task and exit without
    // ever entering the TUI.
    if let Some(title) = &args.add {
        match store.add_item(title, args.workspace.as_deref()) {
            Ok(workspace) => {
                store.save(&args.data_path)?;
                if !args.silent {
                    println!("Added \"{title}\" to {workspace}");
                }
            }
            Err(message) => {
                eprintln!("{message}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // `-w`/`--workspace` without `--add`: pop up an inline input field, add the
    // typed task to the (possibly named) workspace, then exit.
    if args.prompt_add {
        return prompt_add(&mut store, &args);
    }

    let mut app = App::new(store);

    let terminal = ratatui::init();
    let result = run(terminal, &mut app, &args.data_path);
    ratatui::restore();
    result
}

/// Validate the target workspace, show a single inline input field, and add
/// whatever the user types. Enter confirms (empty input cancels), Esc cancels.
fn prompt_add(store: &mut jot_cli::Store, args: &CliArgs) -> io::Result<()> {
    // Resolve the workspace up front so a bad name fails before we prompt.
    let workspace = match store.workspace_name(args.workspace.as_deref()) {
        Ok(name) => name,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };

    let Some(title) = read_inline_input(&workspace)? else {
        return Ok(()); // canceled, or nothing typed
    };

    let name = store
        .add_item(&title, args.workspace.as_deref())
        .expect("workspace was validated above");
    store.save(&args.data_path)?;
    if !args.silent {
        println!("Added \"{title}\" to {name}");
    }
    Ok(())
}

/// Draw a one-line inline prompt — styled after Charm's `gum input` — and
/// collect a task title. An empty field shows a dim "Add to <workspace>"
/// placeholder; a block cursor blinks in a bright pastel. Returns `None` when
/// the user cancels (Esc) or submits empty input.
fn read_inline_input(workspace: &str) -> io::Result<Option<String>> {
    let placeholder = format!("Add to {workspace}");
    // ANSI 212 is gum's default cursor colour — a bright pastel pink.
    let pastel = Color::Indexed(212);
    let prompt_style = Style::default().fg(pastel).add_modifier(Modifier::BOLD);
    let placeholder_style = Style::default()
        .fg(Color::Indexed(244))
        .add_modifier(Modifier::DIM);
    // A block cursor: the cell under it adopts the pastel as its background.
    let cursor_style = Style::default().fg(Color::Black).bg(pastel);

    enable_raw_mode()?;
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(1),
        },
    )?;

    let mut input = String::new();
    let blink = Duration::from_millis(500);
    let mut last_blink = Instant::now();
    let mut cursor_on = true;
    // Any edit makes the cursor solid again, so it never blinks mid-keystroke.
    let wake = |on: &mut bool, at: &mut Instant| {
        *on = true;
        *at = Instant::now();
    };

    let outcome = loop {
        terminal.draw(|frame| {
            let mut spans = vec![Span::styled("> ", prompt_style)];
            if input.is_empty() {
                // Cursor sits over the first placeholder character.
                let mut chars = placeholder.chars();
                match chars.next() {
                    Some(first) => {
                        let head = if cursor_on {
                            cursor_style
                        } else {
                            placeholder_style
                        };
                        spans.push(Span::styled(first.to_string(), head));
                        spans.push(Span::styled(chars.as_str().to_string(), placeholder_style));
                    }
                    None if cursor_on => spans.push(Span::styled(" ", cursor_style)),
                    None => {}
                }
            } else {
                spans.push(Span::raw(input.clone()));
                if cursor_on {
                    spans.push(Span::styled(" ", cursor_style));
                }
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), frame.area());
        })?;

        let timeout = blink.saturating_sub(last_blink.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Enter => break Some(input.trim().to_string()),
                    KeyCode::Esc => break None,
                    KeyCode::Backspace => {
                        input.pop();
                        wake(&mut cursor_on, &mut last_blink);
                    }
                    KeyCode::Char(ch) => {
                        input.push(ch);
                        wake(&mut cursor_on, &mut last_blink);
                    }
                    _ => {}
                },
                Event::Paste(content) => {
                    input.push_str(&content.replace(['\n', '\r'], " "));
                    wake(&mut cursor_on, &mut last_blink);
                }
                _ => {}
            }
        }

        if last_blink.elapsed() >= blink {
            cursor_on = !cursor_on;
            last_blink = Instant::now();
        }
    };

    // Wipe the prompt line so it doesn't linger above our output, then restore.
    // MoveToColumn(0) returns the cursor to the start of the line — without it
    // raw mode leaves it where the prompt ended, indenting subsequent output.
    terminal.clear()?;
    let _ = execute!(io::stdout(), DisableBracketedPaste, MoveToColumn(0));
    disable_raw_mode()?;

    Ok(outcome.filter(|title| !title.is_empty()))
}

fn run(
    mut terminal: DefaultTerminal,
    app: &mut App,
    data_path: &std::path::Path,
) -> io::Result<()> {
    // Bracketed paste lets the terminal hand us paste payloads as Event::Paste.
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    // The Kitty keyboard protocol lets supporting terminals report the macOS
    // Command key (as SUPER), so Cmd+C can reach us instead of being swallowed.
    let enhanced = matches!(supports_keyboard_enhancement(), Ok(true));
    if enhanced {
        let _ = execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let mut clipboard = arboard::Clipboard::new().ok();

    let result = event_loop(&mut terminal, app, data_path, &mut clipboard);

    if enhanced {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    data_path: &std::path::Path,
    clipboard: &mut Option<arboard::Clipboard>,
) -> io::Result<()> {
    let tick_rate = Duration::from_millis(250);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|frame| draw(frame, app))?;

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    // Ctrl on most platforms; SUPER is the macOS Command key.
                    let modkey = key.modifiers.contains(KeyModifiers::CONTROL)
                        || key.modifiers.contains(KeyModifiers::SUPER);
                    let copy = modkey
                        && matches!(app.mode, Mode::Normal)
                        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'));
                    let paste =
                        modkey && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'));

                    if copy {
                        if let Some(text) = app.copy_selected()
                            && let Some(cb) = clipboard.as_mut()
                        {
                            let _ = cb.set_text(text);
                        }
                    } else if paste {
                        let content = clipboard
                            .as_mut()
                            .and_then(|cb| cb.get_text().ok())
                            .unwrap_or_default();
                        app.paste(content);
                    } else {
                        match app.handle_key(key) {
                            Update::Quit => {
                                app.store.save(data_path)?;
                                return Ok(());
                            }
                            Update::Save => app.store.save(data_path)?,
                            Update::None => {}
                        }
                    }
                }
                Event::Paste(content) => app.paste(content),
                _ => {}
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

    // A muted hint colour: lighter than the body text but not glaring, so it
    // stays readable on the highlighted (cyan) row.
    let hint_style = Style::default().fg(Color::Gray);

    let (move_src_ws, move_origin, move_item_dest) = match &app.mode {
        jot_cli::Mode::Moving {
            src_ws,
            origin,
            dest,
        } => match dest {
            jot_cli::MoveDest::Item { as_child, .. } => {
                (Some(*src_ws), Some(origin.clone()), Some(*as_child))
            }
            jot_cli::MoveDest::Workspace => (Some(*src_ws), Some(origin.clone()), None),
        },
        _ => (None, None, None),
    };
    // Picking a destination workspace (the origin item leaves via the left edge).
    let choosing_workspace = matches!(
        &app.mode,
        jot_cli::Mode::Moving {
            dest: jot_cli::MoveDest::Workspace,
            ..
        }
    );
    // The ⇅ marker only makes sense while the source workspace is on screen.
    let origin_visible = move_src_ws == Some(app.store.selected_workspace);

    let workspace_items = app
        .store
        .workspaces
        .iter()
        .enumerate()
        .map(|(index, workspace)| {
            let label = format!("{} ({})", workspace.name, workspace.items.len());
            let selected = index == app.store.selected_workspace;
            let style = if selected {
                selection_style(workspaces_focused)
            } else {
                Style::default()
            };
            if selected && choosing_workspace {
                ListItem::new(Line::from(vec![
                    Span::raw(label),
                    Span::styled("  ← move here", hint_style),
                ]))
                .style(style)
            } else {
                ListItem::new(label).style(style)
            }
        })
        .collect::<Vec<_>>();

    let items = app
        .flattened_items()
        .into_iter()
        .map(|item| {
            let indent = "  ".repeat(item.depth);
            let selected = app.selected_path.as_ref() == Some(&item.path);
            let is_origin = origin_visible && move_origin.as_ref() == Some(&item.path);

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

            // While moving within the tree, the selected row is the drop target.
            // Show where the item will land, with an arrow when it nests.
            if selected && let Some(as_child) = move_item_dest {
                let hint = if as_child {
                    "  ↳ as child"
                } else {
                    "  ← insert after"
                };
                spans.push(Span::styled(hint, hint_style));
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

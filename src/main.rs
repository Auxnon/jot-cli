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
use jot_cli::{App, CliArgs, Focus, Mode, Update, parse_args, wrap_words};
use ratatui::{
    DefaultTerminal, Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
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

    // With Google enabled, make sure the default-list workspace exists.
    #[cfg(feature = "google")]
    store.ensure_google_workspace();

    // `--sync` is a one-shot command-line action: reconcile with Google and exit.
    if args.sync {
        #[cfg(feature = "google")]
        {
            match jot_cli::sync::sync_store(&mut store) {
                Ok(summary) => {
                    store.save(&args.data_path)?;
                    if !args.silent {
                        println!("{}", summary.describe());
                    }
                }
                Err(message) => {
                    eprintln!("Sync failed: {message}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        #[cfg(not(feature = "google"))]
        {
            eprintln!(
                "This build has no Google support. Rebuild with: cargo build --features google"
            );
            std::process::exit(1);
        }
    }

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

    // Auto-sync on launch when the user has enabled it.
    #[cfg(feature = "google")]
    if store.auto_sync {
        match jot_cli::sync::sync_store(&mut store) {
            Ok(_) => {
                let _ = store.save(&args.data_path);
            }
            Err(message) => eprintln!("Auto-sync on launch failed: {message}"),
        }
    }

    let mut app = App::new(store);

    let terminal = ratatui::init();
    let result = run(terminal, &mut app, &args.data_path);
    ratatui::restore();

    // Auto-sync on quit when enabled (after the terminal is restored, so any
    // first-time auth prompt prints cleanly).
    #[cfg(feature = "google")]
    if app.store.auto_sync {
        match jot_cli::sync::sync_store(&mut app.store) {
            Ok(_) => {
                let _ = app.store.save(&args.data_path);
            }
            Err(message) => eprintln!("Auto-sync on quit failed: {message}"),
        }
    }

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

    let mut input = jot_cli::EditField::default();
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
            if input.text.is_empty() {
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
                let (before, at, after) = input.split_at_cursor();
                spans.push(Span::raw(before.to_string()));
                match at {
                    Some(ch) => {
                        let style = if cursor_on { cursor_style } else { Style::default() };
                        spans.push(Span::styled(ch.to_string(), style));
                        spans.push(Span::raw(after.to_string()));
                    }
                    None if cursor_on => spans.push(Span::styled(" ", cursor_style)),
                    None => {}
                }
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), frame.area());
        })?;

        let timeout = blink.saturating_sub(last_blink.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Enter => break Some(input.text.trim().to_string()),
                    KeyCode::Esc => break None,
                    KeyCode::Backspace => {
                        input.backspace();
                        wake(&mut cursor_on, &mut last_blink);
                    }
                    KeyCode::Delete => {
                        input.delete();
                        wake(&mut cursor_on, &mut last_blink);
                    }
                    KeyCode::Left => {
                        input.move_left();
                        wake(&mut cursor_on, &mut last_blink);
                    }
                    KeyCode::Right => {
                        input.move_right();
                        wake(&mut cursor_on, &mut last_blink);
                    }
                    KeyCode::Home => {
                        input.move_home();
                        wake(&mut cursor_on, &mut last_blink);
                    }
                    KeyCode::End => {
                        input.move_end();
                        wake(&mut cursor_on, &mut last_blink);
                    }
                    KeyCode::Char(ch) => {
                        input.insert(ch);
                        wake(&mut cursor_on, &mut last_blink);
                    }
                    _ => {}
                },
                Event::Paste(content) => {
                    input.insert_str(&content.replace(['\n', '\r'], " "));
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
        // Tell the app how wide the editing dialog currently is, so Up/Down
        // in a dialog move the cursor by exactly one wrapped row.
        app.set_edit_wrap_width(modal_inner_width(terminal.size()?.width));

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
                    // Ctrl+Z / Cmd+Z undoes the last edit. Handled here so it
                    // never records itself onto the undo stack.
                    let undo = modkey
                        && matches!(app.mode, Mode::Normal)
                        && matches!(key.code, KeyCode::Char('z') | KeyCode::Char('Z'));

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
                    } else if undo {
                        if app.undo() {
                            app.store.save(data_path)?;
                        }
                    } else {
                        match app.handle_key(key) {
                            Update::Quit => {
                                app.store.save(data_path)?;
                                return Ok(());
                            }
                            Update::Save => app.store.save(data_path)?,
                            Update::None => {}
                            #[cfg(feature = "google")]
                            Update::Sync => {
                                // Blocking network round-trip; the status line
                                // already shows "Syncing…" from this frame.
                                terminal.draw(|frame| draw(frame, app))?;
                                match jot_cli::sync::sync_store(&mut app.store) {
                                    Ok(summary) => app.set_status(summary.describe()),
                                    Err(message) => {
                                        app.set_status(format!("Sync failed: {message}"))
                                    }
                                }
                                app.refresh_after_sync();
                                app.store.save(data_path)?;
                            }
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

/// The text shown in the status bar for the current mode.
fn status_text(app: &App) -> String {
    match &app.mode {
        jot_cli::Mode::Editing { input, .. } => format!("Input: {}", input.text),
        _ => app.status.clone(),
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let status = status_text(app);

    // Size the status bar to the text it's actually showing: the long controls
    // line wraps across several rows (capped), while a short message needs only
    // one — so the box shrinks back down when the help isn't displayed.
    let inner_width = frame.area().width.saturating_sub(2).max(1) as usize;
    let status_rows = status
        .chars()
        .count()
        .div_ceil(inner_width)
        .clamp(1, 4) as u16;
    let status_height = status_rows + 2; // + top/bottom borders

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(status_height)])
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

    // While reordering workspaces, preview the new order without mutating the
    // store: the workspace at `origin` is shown at `target`, the rest shift.
    let reorder = match &app.mode {
        jot_cli::Mode::MovingWorkspace { origin, target } => Some((*origin, *target)),
        _ => None,
    };
    let ws_count = app.store.workspaces.len();
    let display_order: Vec<usize> = match reorder {
        Some((origin, target)) => {
            let mut order: Vec<usize> = (0..ws_count).collect();
            let moved = order.remove(origin);
            order.insert(target, moved);
            order
        }
        None => (0..ws_count).collect(),
    };

    let ws_inner_width = columns[0].width.saturating_sub(2) as usize;
    let workspace_items = display_order
        .iter()
        .map(|&index| {
            let workspace = &app.store.workspaces[index];
            let (selected, moving) = match reorder {
                // Highlight (and mark) the workspace being moved.
                Some((origin, _)) => (index == origin, index == origin),
                None => (index == app.store.selected_workspace, false),
            };
            let style = if selected {
                selection_style(workspaces_focused)
            } else {
                Style::default()
            };
            let marker = if moving { "⇅ " } else { "" };
            let label = format!("{marker}{} ({})", workspace.name, workspace.items.len());

            // Long names wrap; the item keeps its style across every line.
            let mut lines: Vec<Line> = wrap_words(&label, ws_inner_width)
                .into_iter()
                .map(|line| Line::from(Span::raw(line)))
                .collect();
            if selected && choosing_workspace
                && let Some(last) = lines.last_mut()
            {
                last.spans.push(Span::styled("  ← move here", hint_style));
            }
            ListItem::new(Text::from(lines)).style(style)
        })
        .collect::<Vec<_>>();

    let flat = app.flattened_items();
    let tasks_inner_width = columns[1].width.saturating_sub(2) as usize;
    let items = flat
        .iter()
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

            // Completed tasks get a muted title so they recede; the selected
            // row keeps its row style for legibility against the highlight.
            let title_style = if !selected && item.done {
                Style::default().fg(Color::Indexed(244))
            } else {
                Style::default()
            };

            // Word-wrap the title to the pane; continuation lines hang under
            // the first title character, past the indent/glyph/symbol head.
            let head_width = item.depth * 2 + 4;
            let title_width = tasks_inner_width.saturating_sub(head_width).max(1);
            let wrapped = wrap_words(&item.title, title_width);

            let mut lines = vec![Line::from(vec![
                Span::raw(format!("{indent}{lead} ")),
                Span::styled(symbol, symbol_style),
                Span::styled(format!(" {}", wrapped[0]), title_style),
            ])];
            for continuation in wrapped.iter().skip(1) {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(head_width)),
                    Span::styled(continuation.clone(), title_style),
                ]));
            }

            // While moving within the tree, the selected row is the drop target.
            // Show where the item will land, with an arrow when it nests.
            if selected
                && let Some(as_child) = move_item_dest
                && let Some(last) = lines.last_mut()
            {
                let hint = if as_child {
                    "  ↳ as child"
                } else {
                    "  ← insert after"
                };
                last.spans.push(Span::styled(hint, hint_style));
            }

            ListItem::new(Text::from(lines)).style(row_style)
        })
        .collect::<Vec<_>>();

    let workspace_title = if app.current_workspace().hide_completed {
        format!(
            "Workspace: {} ({} hidden)",
            app.current_workspace().name,
            app.hidden_count()
        )
    } else {
        format!("Workspace: {}", app.current_workspace().name)
    };
    // Stateful rendering scrolls each pane so the selected (multi-line) item
    // stays fully visible now that wrapped items can outgrow the viewport.
    let selected_ws_row = match reorder {
        Some((_, target)) => Some(target),
        None => display_order
            .iter()
            .position(|&index| index == app.store.selected_workspace),
    };
    let mut ws_state = ListState::default();
    ws_state.select(selected_ws_row);
    frame.render_stateful_widget(
        List::new(workspace_items).block(focus_block("Workspaces", workspaces_focused)),
        columns[0],
        &mut ws_state,
    );

    let selected_task_row = app
        .selected_path
        .as_ref()
        .and_then(|path| flat.iter().position(|item| &item.path == path));
    let mut task_state = ListState::default();
    task_state.select(selected_task_row);
    frame.render_stateful_widget(
        List::new(items).block(focus_block(workspace_title, tasks_focused)),
        columns[1],
        &mut task_state,
    );

    frame.render_widget(
        Paragraph::new(status)
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Status").borders(Borders::ALL)),
        layout[1],
    );

    if let jot_cli::Mode::Editing { target, input } = &app.mode {
        let prompt = match target {
            jot_cli::EditTarget::NewWorkspace => "New workspace",
            jot_cli::EditTarget::NewSibling => "New item",
            jot_cli::EditTarget::NewChild => "New child item",
            jot_cli::EditTarget::RenameSelected => "Rename item",
            jot_cli::EditTarget::RenameWorkspace => "Rename workspace",
        };

        // A block cursor: the character under it renders inverted (a plain
        // space when the cursor sits at the end of the input).
        let cursor_style = Style::default().fg(Color::Black).bg(Color::Cyan);

        // Hard-wrap the text at the dialog's inner width — the exact width
        // the event loop reported to the app, so Up/Down land where drawn.
        let area = frame.area();
        let inner_width = modal_inner_width(area.width);
        let mut cells: Vec<(char, bool)> = input
            .text
            .chars()
            .enumerate()
            .map(|(i, ch)| (ch, i == input.cursor))
            .collect();
        if input.cursor >= cells.len() {
            cells.push((' ', true));
        }
        let lines: Vec<Line> = cells
            .chunks(inner_width)
            .map(|row| {
                let mut spans = Vec::new();
                let mut run = String::new();
                for &(ch, under_cursor) in row {
                    if under_cursor {
                        if !run.is_empty() {
                            spans.push(Span::raw(std::mem::take(&mut run)));
                        }
                        spans.push(Span::styled(ch.to_string(), cursor_style));
                    } else {
                        run.push(ch);
                    }
                }
                if !run.is_empty() {
                    spans.push(Span::raw(run));
                }
                Line::from(spans)
            })
            .collect();

        // The dialog grows with the text up to most of the screen; past that
        // the wrapped text scrolls to keep the cursor's row visible.
        let max_rows = area.height.saturating_sub(4).max(1);
        let visible_rows = (lines.len() as u16).clamp(1, max_rows);
        let cursor_row = (input.cursor / inner_width) as u16;
        let scroll = cursor_row.saturating_sub(visible_rows - 1);

        let popup = popup_rect(area, inner_width as u16 + 2, visible_rows + 2);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .scroll((scroll, 0))
                .block(Block::default().title(prompt).borders(Borders::ALL)),
            popup,
        );
    }
}

/// Characters per row inside the editing dialog at this terminal width. The
/// event loop reports it to the app so Up/Down move the cursor by exactly one
/// rendered row.
fn modal_inner_width(terminal_width: u16) -> usize {
    let popup_width = (terminal_width as usize * 60 / 100)
        .max(20)
        .min(terminal_width as usize);
    popup_width.saturating_sub(2).max(1)
}

/// A rect of the given size centered in `area`, clamped to fit.
fn popup_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
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


#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use ratatui::{Terminal, backend::TestBackend};

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::from(code));
    }

    fn type_str(app: &mut App, text: &str) {
        for ch in text.chars() {
            press(app, KeyCode::Char(ch));
        }
    }

    /// Render the app into a test buffer and return it as one string per row.
    fn render(app: &App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn long_task_titles_word_wrap_in_the_list() {
        let mut app = App::new(jot_cli::Store::default());
        press(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "aaaa bbbb cccc");
        press(&mut app, KeyCode::Enter);

        // Tasks pane at width 40: 40 - 24 (workspaces) - 2 (borders) = 14
        // columns, minus the 4-column head = a 10-wide title. "aaaa bbbb"
        // fits the first line; "cccc" hangs underneath, aligned to the title.
        let rows = render(&app, 40, 10);
        let first = rows
            .iter()
            .position(|row| row.contains("aaaa bbbb"))
            .expect("wrapped first line shown");
        assert!(!rows[first].contains("cccc"), "title should have wrapped");
        assert!(rows[first + 1].contains("cccc"));
    }

    #[test]
    fn editing_dialog_wraps_long_input() {
        let mut app = App::new(jot_cli::Store::default());
        app.set_edit_wrap_width(modal_inner_width(40));
        press(&mut app, KeyCode::Char('a'));
        type_str(&mut app, &"x".repeat(60));

        // 60 chars in a 22-wide dialog (40 * 60% - borders) span three rows.
        let rows = render(&app, 40, 12);
        let wrapped_rows = rows
            .iter()
            .filter(|row| row.contains("xxxxxxxxxx"))
            .count();
        assert!(
            wrapped_rows >= 2,
            "dialog input should wrap across rows, got {rows:?}"
        );
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let mut app = App::new(jot_cli::Store::default());
        press(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "a task that is long enough to wrap many times");
        render(&app, 10, 4);
        render(&app, 3, 2);
        press(&mut app, KeyCode::Enter);
        render(&app, 10, 4);
    }
}

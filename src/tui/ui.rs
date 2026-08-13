//! Rendering for the TUI.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::model::Kind;

use super::form::Form;
use super::state::{Mode, UiState};

// Kept minimal and semantic so the UI reads on any terminal theme: color only
// for meaning (kind, warnings); structure via bold/dim/reversed which the
// terminal maps to its own foreground/background.
const FUNCTION_COLOR: Color = Color::Magenta;
const ALIAS_COLOR: Color = Color::Green;
const WARN: Color = Color::Yellow;

fn label_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn header_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Render the whole UI.
pub fn draw(frame: &mut Frame, state: &UiState, form: Option<&Form>) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    draw_search(frame, chunks[0], state);

    let panes = Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(chunks[1]);
    draw_list(frame, panes[0], state);
    draw_detail(frame, panes[1], state);

    draw_status(frame, chunks[2], state);

    match &state.mode {
        Mode::Help => draw_help(frame),
        Mode::Confirm { message, .. } => draw_confirm(frame, message),
        _ => {}
    }
    if let Some(form) = form {
        draw_form(frame, form);
    }
}

fn draw_search(frame: &mut Frame, area: Rect, state: &UiState) {
    let searching = state.mode == Mode::Search;
    let title = if searching {
        " Search (typing) "
    } else {
        " Search "
    };
    let cursor = if searching { "\u{2588}" } else { "" };
    let line = Line::from(vec![
        Span::styled("/ ", header_style()),
        Span::raw(state.query.clone()),
        Span::styled(cursor, header_style()),
        Span::raw("   "),
        Span::styled(state.filter_label(), label_style()),
    ]);
    let border = if searching {
        header_style()
    } else {
        label_style()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border);
    frame.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_list(frame: &mut Frame, area: Rect, state: &UiState) {
    let entries = state.filtered_entries();
    let items: Vec<ListItem> = entries
        .iter()
        .map(|e| {
            // No explicit foreground so the reversed selection bar stays clean
            // and readable on any theme.
            let mut spans = vec![
                Span::styled(format!("{:<16}", e.name), header_style()),
                Span::styled(format!("[{}]", e.kind), label_style()),
            ];
            if e.destructive {
                spans.push(Span::styled("  !", Style::default().fg(WARN)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let title = format!(" Entries ({}) ", entries.len());
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .highlight_symbol("> ");

    let mut list_state = ListState::default();
    if !entries.is_empty() {
        list_state.select(Some(state.selected_index()));
    }
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn draw_detail(frame: &mut Frame, area: Rect, state: &UiState) {
    let block = Block::default().borders(Borders::ALL).title(" Detail ");
    let Some(entry) = state.selected_entry() else {
        let empty = Paragraph::new("No entries. Press 'a' to add one.")
            .block(block)
            .wrap(Wrap { trim: true });
        frame.render_widget(empty, area);
        return;
    };

    let mut lines = vec![
        field_line("alias", &entry.name),
        Line::from(vec![
            Span::styled("kind      ", label_style()),
            Span::styled(
                entry.kind.to_string(),
                Style::default().fg(kind_color(entry.kind)),
            ),
        ]),
        field_line("category", &entry.category),
    ];
    if !entry.tags.is_empty() {
        lines.push(field_line("tags", &entry.tags.join(", ")));
    }
    if entry.destructive {
        lines.push(Line::from(Span::styled(
            "flagged destructive",
            Style::default().fg(WARN),
        )));
    }
    lines.push(Line::from(""));
    lines.push(section("description"));
    lines.push(Line::from(entry.description.clone()));

    lines.push(Line::from(""));
    lines.push(section(if entry.kind == Kind::Function {
        "body"
    } else {
        "command"
    }));
    for l in entry.command.lines() {
        lines.push(Line::from(Span::styled(l.to_string(), header_style())));
    }

    if !entry.usage.is_empty() {
        lines.push(Line::from(""));
        lines.push(section("usage"));
        for l in entry.usage.lines() {
            lines.push(Line::from(l.to_string()));
        }
    }

    if !entry.options.is_empty() {
        lines.push(Line::from(""));
        lines.push(section("options"));
        for l in entry.options.lines() {
            lines.push(Line::from(l.to_string()));
        }
    }

    if !entry.examples.is_empty() {
        lines.push(Line::from(""));
        lines.push(section("examples"));
        for l in entry.examples.lines() {
            lines.push(Line::from(l.to_string()));
        }
    }

    lines.push(Line::from(""));
    lines.push(section("example output"));
    if entry.example_output.is_empty() {
        lines.push(Line::from(Span::styled(
            "(none captured - press 'c')",
            label_style(),
        )));
    } else {
        for l in entry.example_output.lines() {
            lines.push(Line::from(l.to_string()));
        }
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_status(frame: &mut Frame, area: Rect, state: &UiState) {
    let hints = match state.mode {
        Mode::Search => "type to filter  Enter/Esc: done",
        _ => {
            "/ search  j/k move  Tab kind  g category  a add  e edit  d delete  c capture  r reload  b backup  ? help  q quit"
        }
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", state.status),
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(hints, label_style()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_confirm(frame: &mut Frame, message: &str) {
    let area = centered(60, 20, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Confirm ")
        .border_style(Style::default().fg(WARN));
    let text = vec![
        Line::from(message.to_string()),
        Line::from(""),
        Line::from(Span::styled("y: yes    n/Esc: no", label_style())),
    ];
    frame.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_help(frame: &mut Frame) {
    let area = centered(70, 70, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(header_style());
    let lines = vec![
        help_row("/", "start fuzzy search"),
        help_row("j / k, arrows", "move selection"),
        help_row("PgUp / PgDn", "page through the list"),
        help_row("Tab", "cycle kind filter (all/alias/function)"),
        help_row("g", "cycle category filter"),
        help_row("a", "add a new entry"),
        help_row("e", "edit the selected entry"),
        help_row("d", "delete the selected entry"),
        help_row("c", "capture example output"),
        help_row("r", "regenerate shell files"),
        help_row("b", "back up to GitHub"),
        help_row("?", "toggle this help"),
        help_row("q / Esc", "quit"),
    ];
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_form(frame: &mut Frame, form: &Form) {
    let area = centered(70, 80, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", form.title))
        .border_style(header_style());

    let mut lines = Vec::new();
    for (i, field) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        let label = if field.required {
            format!("{}*", field.label)
        } else {
            field.label.to_string()
        };
        if focused {
            // Reversed video reads clearly on both light and dark terminals.
            let focus_style = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
            lines.push(Line::from(vec![Span::styled(
                format!("{label:<28}{}\u{2588}", field.value),
                focus_style,
            )]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("{label:<28}"), label_style()),
                Span::raw(field.value.clone()),
            ]));
        }
        lines.push(Line::from(""));
    }

    if let Some(err) = &form.error {
        lines.push(Line::from(Span::styled(
            format!("  {err}"),
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(Span::styled(
        "Tab/Shift-Tab: fields   Ctrl-F: fetch help   Enter: save   Esc: cancel",
        Style::default().add_modifier(Modifier::DIM),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn kind_color(kind: Kind) -> Color {
    match kind {
        Kind::Alias => ALIAS_COLOR,
        Kind::Function => FUNCTION_COLOR,
    }
}

fn field_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), label_style()),
        Span::raw(value.to_string()),
    ])
}

fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_uppercase(),
        Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    ))
}

fn help_row(keys: &'static str, desc: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {keys:<16}"), header_style()),
        Span::raw(desc),
    ])
}

fn centered(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    let horizontal = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1]);
    horizontal[1]
}

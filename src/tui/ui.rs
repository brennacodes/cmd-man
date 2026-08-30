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

/// The gutter every form label is padded into, so values line up in a column.
const FORM_LABEL_WIDTH: usize = 28;

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

    let inner_width = area.width.saturating_sub(2) as usize;
    let mut lines = Vec::new();
    for (i, field) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        let label = if field.required {
            format!("{}*", field.label)
        } else {
            field.label.to_string()
        };
        let label = clip(&format!("{label:<FORM_LABEL_WIDTH$}"), inner_width, false);
        let cursor = if focused && width(&label) < inner_width {
            "\u{2588}"
        } else {
            ""
        };
        let budget = inner_width - width(&label) - width(cursor);
        let value = clip(&field.value, budget, focused);
        if focused {
            // Reversed video reads clearly on both light and dark terminals.
            let focus_style = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
            lines.push(Line::from(vec![Span::styled(
                format!("{label}{value}{cursor}"),
                focus_style,
            )]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(label, label_style()),
                Span::raw(value),
            ]));
        }
    }

    if let Some(err) = &form.error {
        lines.push(Line::from(Span::styled(
            format!("  {err}"),
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Tab/Shift-Tab: fields   Ctrl-F: fetch help   Enter: save   Esc: cancel",
        Style::default().add_modifier(Modifier::DIM),
    )));

    let inner_rows = area.height.saturating_sub(2) as usize;
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((form_scroll(form.focus, inner_rows), 0)),
        area,
    );
}

/// The columns `text` occupies, measured the way the renderer measures it.
fn width(text: &str) -> usize {
    Span::raw(text).width()
}

/// `text` shortened to `budget` columns, marking the cut with an ellipsis. A
/// focused field keeps its tail so the cursor stays in view while typing; every
/// other field keeps its head.
fn clip(text: &str, budget: usize, keep_tail: bool) -> String {
    if width(text) <= budget {
        return text.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let keep = budget - 1;
    let mut used = 0;
    if keep_tail {
        let mut start = text.len();
        for (i, ch) in text.char_indices().rev() {
            used += width(&text[i..i + ch.len_utf8()]);
            if used > keep {
                break;
            }
            start = i;
        }
        format!("\u{2026}{}", &text[start..])
    } else {
        let mut end = 0;
        for (i, ch) in text.char_indices() {
            used += width(&text[i..i + ch.len_utf8()]);
            if used > keep {
                break;
            }
            end = i + ch.len_utf8();
        }
        format!("{}\u{2026}", &text[..end])
    }
}

/// The first row of the form to draw, so the focused field stays on screen on a
/// popup too short for every field. Each field occupies exactly one row because
/// `draw_form` clips every value to the width of the popup.
fn form_scroll(focus: usize, inner_rows: usize) -> u16 {
    let last_visible = inner_rows.saturating_sub(1);
    u16::try_from(focus.saturating_sub(last_visible)).unwrap_or(u16::MAX)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Entry;
    use crate::tui::form::SCOPE;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// The form popup rendered into a terminal of the given size, one string
    /// per row.
    fn render_form(form: &Form, width: u16, height: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|frame| draw_form(frame, form)).unwrap();
        let buffer = term.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn every_field_fits_a_standard_eighty_by_twenty_four_terminal() {
        let rows = render_form(&Form::new_entry(), 80, 24);
        for field in Form::new_entry().fields.iter() {
            assert!(
                rows.iter().any(|row| row.contains(field.label)),
                "{} is off screen in:\n{}",
                field.label,
                rows.join("\n")
            );
        }
    }

    #[test]
    fn a_field_value_sits_in_its_own_column() {
        let mut form = Form::new_entry();
        form.fields[SCOPE].value = "api".into();
        let rows = render_form(&form, 80, 24);
        let scope = rows
            .iter()
            .find(|row| row.contains("Scope"))
            .unwrap_or_else(|| panic!("no scope row in:\n{}", rows.join("\n")));
        let label = "Scope (global/here/<id>)";
        assert!(
            scope.contains(&format!("{label:<FORM_LABEL_WIDTH$}api")),
            "scope row was: {scope}"
        );
    }

    #[test]
    fn the_focused_field_stays_on_screen_on_a_short_terminal() {
        let mut form = Form::new_entry();
        form.focus = SCOPE;
        let rows = render_form(&form, 80, 12);
        assert!(
            rows.iter().any(|row| row.contains("Scope")),
            "focused field scrolled away in:\n{}",
            rows.join("\n")
        );
    }

    #[test]
    fn long_values_do_not_push_the_focused_field_off_screen() {
        let mut form = Form::new_entry();
        for field in form.fields.iter_mut() {
            field.value = "-a, --all do not ignore entries starting with .".repeat(6);
        }
        form.focus = SCOPE;
        for height in [12, 24] {
            let rows = render_form(&form, 80, height);
            assert!(
                rows.iter().any(|row| row.contains("Scope")),
                "focused field scrolled away at height {height} in:\n{}",
                rows.join("\n")
            );
        }
    }

    #[test]
    fn a_focused_field_keeps_the_end_of_a_long_value_in_view() {
        let mut form = Form::new_entry();
        form.fields[SCOPE].value = format!("{}tail", "x".repeat(60));
        form.focus = SCOPE;
        let rows = render_form(&form, 80, 24);
        let scope = rows
            .iter()
            .find(|row| row.contains("Scope"))
            .unwrap_or_else(|| panic!("no scope row in:\n{}", rows.join("\n")));
        assert!(
            scope.contains("\u{2026}") && scope.contains("tail\u{2588}"),
            "scope row was: {scope}"
        );
    }

    #[test]
    fn a_value_of_double_width_characters_still_fits_one_row() {
        let mut form = Form::new_entry();
        for field in form.fields.iter_mut() {
            field.value = "\u{5217}\u{8868}\u{6587}\u{4EF6}".repeat(20);
        }
        form.focus = SCOPE;
        let rows = render_form(&form, 80, 24);
        for field in Form::new_entry().fields.iter() {
            assert_eq!(
                rows.iter().filter(|row| row.contains(field.label)).count(),
                1,
                "{} does not occupy exactly one row in:\n{}",
                field.label,
                rows.join("\n")
            );
        }
    }

    #[test]
    fn a_long_value_is_clipped_to_one_row_per_field() {
        let mut form = Form::new_entry();
        for field in form.fields.iter_mut() {
            field.value = "ls -lah /usr/local/bin ".repeat(10);
        }
        let rows = render_form(&form, 80, 24);
        for field in Form::new_entry().fields.iter() {
            let matched = rows.iter().filter(|row| row.contains(field.label)).count();
            assert_eq!(
                matched,
                1,
                "{} spans {matched} rows in:\n{}",
                field.label,
                rows.join("\n")
            );
        }
    }

    #[test]
    fn an_edit_form_renders_the_current_scope() {
        let mut entry = Entry::new("ll", Kind::Alias, "ls -lah", "d");
        entry.scope = Some("api".into());
        let rows = render_form(&Form::edit(&entry), 80, 24);
        assert!(
            rows.iter()
                .any(|row| row.contains("Scope (global/here/<id>)") && row.contains("api")),
            "no scope row in:\n{}",
            rows.join("\n")
        );
    }

    #[test]
    fn scrolling_starts_at_the_top_until_the_focus_would_fall_off() {
        assert_eq!(form_scroll(0, 12), 0);
        assert_eq!(form_scroll(9, 12), 0);
        assert_eq!(form_scroll(9, 5), 5);
        assert_eq!(form_scroll(9, 0), 9);
    }
}

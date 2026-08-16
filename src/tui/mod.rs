//! Full-screen keyboard TUI for browsing and managing entries.

mod form;
mod state;
mod ui;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{App, now_timestamp};
use crate::backup;

use form::Form;
use state::{Action, ConfirmKind, Mode, UiState};

/// Launch the TUI event loop.
pub fn run() -> Result<()> {
    let mut app = App::load()?;
    // Pull the latest at launch; mid-session mutations sync via App::persist.
    backup::spawn_sync(&app.paths);
    let mut state = UiState::new(app.store.entries().to_vec());
    let mut form: Option<Form> = None;

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &mut state, &mut form);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    state: &mut UiState,
    form: &mut Option<Form>,
) -> Result<()> {
    loop {
        terminal.draw(|frame| ui::draw(frame, state, form.as_ref()))?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if form.is_some() {
            // Ctrl-F fetches help and fills empty fields; must be intercepted
            // before the form's text input consumes the 'f'.
            if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL) {
                fetch_help_into_form(form, app);
                continue;
            }
            if let Some(action) = handle_form_key(form, key) {
                execute(action, app, state)?;
            }
            continue;
        }

        if let Some(action) = handle_key(state, key) {
            match action {
                Action::Quit => return Ok(()),
                // Opening a form needs the current selection and the form slot,
                // so it is resolved here rather than in `execute`.
                Action::OpenForm { edit } => {
                    *form = if edit {
                        state.selected_entry().map(Form::edit)
                    } else {
                        Some(Form::new_entry())
                    };
                }
                other => execute(other, app, state)?,
            }
        }
    }
}

/// Fetch help for the form's command and fill empty fields.
fn fetch_help_into_form(form: &mut Option<Form>, app: &App) {
    let Some(f) = form.as_mut() else {
        return;
    };
    let command = f.command_value().to_string();
    if command.is_empty() {
        f.error = Some("Enter a command first".into());
        return;
    }
    let sections = app.fetch_help(&command);
    f.helped_command = Some(command);
    if f.fill_empty_help(&sections) {
        f.error = None;
    } else {
        f.error = Some("No help sections found".into());
    }
}

/// Handle a key while a form modal is open.
fn handle_form_key(form: &mut Option<Form>, key: KeyEvent) -> Option<Action> {
    let f = form.as_mut()?;
    match key.code {
        KeyCode::Esc => {
            *form = None;
            None
        }
        KeyCode::Enter => match f.to_entry() {
            Ok(entry) => {
                let original = f.original_name.clone();
                // Skip the on-save fetch if help was already pulled for this
                // exact command via Ctrl-F.
                let autofill = f.helped_command.as_deref() != Some(f.command_value());
                *form = None;
                Some(Action::Submit {
                    original,
                    entry: Box::new(entry),
                    autofill,
                })
            }
            // Surface the validation error instead of failing silently.
            Err(msg) => {
                f.error = Some(msg);
                None
            }
        },
        KeyCode::Tab | KeyCode::Down => {
            f.focus_next();
            None
        }
        KeyCode::BackTab | KeyCode::Up => {
            f.focus_prev();
            None
        }
        KeyCode::Backspace => {
            f.backspace();
            None
        }
        KeyCode::Char(c) => {
            f.input(c);
            None
        }
        _ => None,
    }
}

/// Handle a key in the main browse/search/confirm/help modes.
fn handle_key(state: &mut UiState, key: KeyEvent) -> Option<Action> {
    match state.mode.clone() {
        Mode::Search => handle_search_key(state, key),
        Mode::Confirm { kind, .. } => handle_confirm_key(state, kind, key),
        Mode::Help => {
            // Any key dismisses help.
            state.mode = Mode::Browse;
            None
        }
        Mode::Browse => handle_browse_key(state, key),
    }
}

fn handle_browse_key(state: &mut UiState, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Some(Action::Quit),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return Some(Action::Quit);
        }
        KeyCode::Char('/') => state.mode = Mode::Search,
        KeyCode::Char('j') | KeyCode::Down => state.move_down(),
        KeyCode::Char('k') | KeyCode::Up => state.move_up(),
        KeyCode::PageDown => state.page_down(10),
        KeyCode::PageUp => state.page_up(10),
        KeyCode::Tab => state.cycle_kind(),
        KeyCode::Char('g') => state.cycle_category(),
        KeyCode::Char('?') => state.mode = Mode::Help,
        KeyCode::Char('a') => return Some(Action::open_add()),
        KeyCode::Char('e') => return Some(Action::open_edit()),
        KeyCode::Char('r') => return Some(Action::Reload),
        KeyCode::Char('b') => return Some(Action::Backup),
        KeyCode::Char('d') => {
            if let Some(entry) = state.selected_entry() {
                let name = entry.name.clone();
                state.mode = Mode::Confirm {
                    kind: ConfirmKind::Delete(name.clone()),
                    message: format!("Delete '{name}'? This regenerates your shell files."),
                };
            }
        }
        KeyCode::Char('c') => {
            if let Some(entry) = state.selected_entry() {
                return Some(Action::CaptureRequest(entry.name.clone()));
            }
        }
        _ => {}
    }
    None
}

fn handle_search_key(state: &mut UiState, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => state.mode = Mode::Browse,
        KeyCode::Backspace => state.pop_query_char(),
        KeyCode::Down => state.move_down(),
        KeyCode::Up => state.move_up(),
        KeyCode::Char(c) => state.push_query_char(c),
        _ => {}
    }
    None
}

fn handle_confirm_key(state: &mut UiState, kind: ConfirmKind, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            state.mode = Mode::Browse;
            match kind {
                ConfirmKind::Delete(name) => Some(Action::Delete(name)),
                ConfirmKind::Capture(name) => Some(Action::CaptureConfirmed(name)),
            }
        }
        _ => {
            state.mode = Mode::Browse;
            None
        }
    }
}

/// Carry out an action against the App and refresh state.
fn execute(action: Action, app: &mut App, state: &mut UiState) -> Result<()> {
    match action {
        // Quit and OpenForm are handled in the event loop before reaching here.
        Action::Quit | Action::OpenForm { .. } => {}
        Action::Reload => match app.regenerate_shells() {
            Ok(()) => state.set_status("Regenerated shell files"),
            Err(e) => state.set_status(format!("Reload failed: {e}")),
        },
        Action::Backup => match backup::run_backup(&app.paths, &app.config, &now_timestamp()) {
            Ok(report) => {
                let last = report.messages.last().cloned().unwrap_or_default();
                state.set_status(format!("Backup: {last}"));
            }
            Err(e) => state.set_status(format!("Backup failed: {e}")),
        },
        Action::Delete(name) => match app.remove(&name) {
            Ok(_) => {
                refresh(app, state);
                state.set_status(format!("Deleted '{name}'"));
            }
            Err(e) => state.set_status(format!("Delete failed: {e}")),
        },
        Action::Submit {
            original,
            entry,
            autofill,
        } => {
            let mut entry = *entry;
            if autofill {
                app.fill_from_help(&mut entry);
            }
            let name = entry.name.clone();
            let result = match &original {
                Some(old) => app.update(old, entry),
                None => app.add(entry),
            };
            match result {
                Ok(()) => {
                    refresh(app, state);
                    state.set_status(format!("Saved '{name}'"));
                }
                Err(e) => state.set_status(format!("Save failed: {e}")),
            }
        }
        Action::CaptureRequest(name) => {
            if let Some(entry) = app.store.find(&name).cloned() {
                let assessment = app.assess(&entry);
                if assessment.is_blocked() {
                    state.set_status(format!("Capture disabled for '{name}'"));
                } else if assessment.needs_confirmation() {
                    let reasons = if assessment.reasons.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", assessment.reasons.join("; "))
                    };
                    state.mode = Mode::Confirm {
                        kind: ConfirmKind::Capture(name.clone()),
                        message: format!("Run '{name}' to capture output?{reasons}"),
                    };
                } else {
                    perform_capture(app, state, &name, assessment.destructive);
                }
            }
        }
        Action::CaptureConfirmed(name) => {
            let destructive = app
                .store
                .find(&name)
                .map(|e| app.assess(e).destructive)
                .unwrap_or(false);
            perform_capture(app, state, &name, destructive);
        }
    }
    Ok(())
}

fn perform_capture(app: &mut App, state: &mut UiState, name: &str, destructive: bool) {
    let entry = match app.store.find(name).cloned() {
        Some(e) => e,
        None => return,
    };
    match app.capture(&entry) {
        Ok(result) => {
            let note = if result.timed_out { " (timed out)" } else { "" };
            let backend = result.backend;
            if let Err(e) = app.record_capture(name, result.output.trim().to_string(), destructive)
            {
                state.set_status(format!("Capture save failed: {e}"));
            } else {
                refresh(app, state);
                state.set_status(format!("Captured '{name}' via {backend}{note}"));
            }
        }
        Err(e) => state.set_status(format!("Capture failed: {e}")),
    }
}

/// Rebuild the view from the App's current store, opening a form if requested.
fn refresh(app: &App, state: &mut UiState) {
    state.set_entries(app.store.entries().to_vec());
}

/// Actions that open a form need the current selection, so they are resolved in
/// the run loop rather than in state. These helpers keep intent readable.
impl Action {
    fn open_add() -> Action {
        Action::OpenForm { edit: false }
    }
    fn open_edit() -> Action {
        Action::OpenForm { edit: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::{Entry, Kind};
    use crate::paths::Paths;
    use crate::store::Store;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn browse_quit_keys() {
        let mut state = UiState::new(vec![]);
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
    }

    #[test]
    fn slash_enters_search_mode() {
        let mut state = UiState::new(vec![]);
        handle_key(&mut state, key(KeyCode::Char('/')));
        assert_eq!(state.mode, Mode::Search);
        handle_key(&mut state, key(KeyCode::Char('x')));
        assert_eq!(state.query, "x");
        handle_key(&mut state, key(KeyCode::Esc));
        assert_eq!(state.mode, Mode::Browse);
    }

    #[test]
    fn delete_flow_confirms_then_removes() {
        let mut state = UiState::new(vec![Entry::new("gitsw", Kind::Alias, "git switch", "s")]);
        let none = handle_key(&mut state, key(KeyCode::Char('d')));
        assert!(none.is_none());
        assert!(matches!(state.mode, Mode::Confirm { .. }));
        let action = handle_key(&mut state, key(KeyCode::Char('y')));
        assert_eq!(action, Some(Action::Delete("gitsw".into())));
    }

    #[test]
    fn confirm_no_cancels() {
        let mut state = UiState::new(vec![Entry::new("gitsw", Kind::Alias, "git switch", "s")]);
        handle_key(&mut state, key(KeyCode::Char('d')));
        let action = handle_key(&mut state, key(KeyCode::Char('n')));
        assert!(action.is_none());
        assert_eq!(state.mode, Mode::Browse);
    }

    #[test]
    fn form_enter_with_invalid_name_shows_error() {
        let mut form = Some(Form::new_entry());
        if let Some(f) = form.as_mut() {
            for c in "bad name".chars() {
                f.input(c); // focus starts on the name field
            }
            f.focus = 2; // command
            for c in "echo hi".chars() {
                f.input(c);
            }
            f.focus = 3; // description
            for c in "desc".chars() {
                f.input(c);
            }
        }
        let action = handle_form_key(&mut form, key(KeyCode::Enter));
        assert!(action.is_none());
        assert!(form.as_ref().unwrap().error.is_some());
    }

    #[test]
    fn submit_action_adds_entry_to_app() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let mut app = App::with(paths, Config::default(), Store::default());
        let mut state = UiState::new(vec![]);
        let entry = Entry::new("gitsw", Kind::Alias, "git switch", "switch");
        execute(
            Action::Submit {
                original: None,
                entry: Box::new(entry),
                autofill: false,
            },
            &mut app,
            &mut state,
        )
        .unwrap();
        assert!(app.store.find("gitsw").is_some());
        assert_eq!(state.filtered_entries().len(), 1);
    }
}

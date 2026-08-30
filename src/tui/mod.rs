//! Full-screen keyboard TUI for browsing and managing entries.

mod form;
mod state;
mod ui;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{App, now_timestamp};
use crate::backup;
use crate::model::EntryKey;
use crate::scopes::Placement;

use form::Form;
use state::{Action, ConfirmKind, Mode, UiState};

/// Launch the TUI event loop.
pub fn run() -> Result<()> {
    let mut app = App::load()?;
    // Pull the latest at launch; mid-session mutations sync via App::persist.
    backup::spawn_sync(&app.paths);
    let mut state = UiState::new(&app);
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
                let original = f.original.clone();
                // Skip the on-save fetch if help was already pulled for this
                // exact command via Ctrl-F.
                let autofill = f.helped_command.as_deref() != Some(f.command_value());
                let placement = f.placement();
                *form = None;
                Some(Action::Submit {
                    original,
                    entry: Box::new(entry),
                    placement,
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
        KeyCode::Char('s') => return Some(Action::CycleScope),
        KeyCode::Char('?') => state.mode = Mode::Help,
        KeyCode::Char('a') => return Some(Action::open_add()),
        KeyCode::Char('e') => return Some(Action::open_edit()),
        KeyCode::Char('r') => return Some(Action::Reload),
        KeyCode::Char('b') => return Some(Action::Backup),
        KeyCode::Char('d') => {
            if let Some(entry) = state.selected_entry() {
                let key = EntryKey::of(entry);
                let label = key.label();
                state.mode = Mode::Confirm {
                    kind: ConfirmKind::Delete(key),
                    message: format!("Delete '{label}'? This regenerates your shell files."),
                };
            }
        }
        KeyCode::Char('c') => {
            if let Some(entry) = state.selected_entry() {
                return Some(Action::CaptureRequest(EntryKey::of(entry)));
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
                ConfirmKind::Delete(key) => Some(Action::Delete(key)),
                ConfirmKind::Capture(key) => Some(Action::CaptureConfirmed(key)),
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
        Action::CycleScope => state.cycle_scope(app),
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
        Action::Delete(key) => match app.remove(&key) {
            Ok(_) => {
                refresh(app, state);
                state.set_status(format!("Deleted '{}'", key.label()));
            }
            Err(e) => state.set_status(format!("Delete failed: {e}")),
        },
        Action::Submit {
            original,
            entry,
            placement,
            autofill,
        } => {
            let mut entry = *entry;
            if autofill {
                app.fill_from_help(&mut entry);
            }
            let result = match &original {
                Some(old) => app.update(old, entry, requested_move(old, &placement)),
                None => app.add(entry, &placement),
            };
            match result {
                Ok(key) => {
                    refresh(app, state);
                    // A body only one shell can carry is saved, so the status
                    // line is the one place the user hears about the other.
                    let status = match app.exclusion_notes(&key).first() {
                        Some(note) => format!("Saved '{}'; {note}", key.label()),
                        None => format!("Saved '{}'", key.label()),
                    };
                    state.set_status(status);
                }
                Err(e) => state.set_status(format!("Save failed: {e}")),
            }
        }
        Action::CaptureRequest(key) => {
            if let Some(entry) = app.store.find(&key).cloned() {
                let assessment = app.assess(&entry);
                let label = key.label();
                if assessment.is_blocked() {
                    state.set_status(format!("Capture disabled for '{label}'"));
                } else if assessment.needs_confirmation() {
                    let reasons = if assessment.reasons.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", assessment.reasons.join("; "))
                    };
                    state.mode = Mode::Confirm {
                        kind: ConfirmKind::Capture(key),
                        message: format!("Run '{label}' to capture output?{reasons}"),
                    };
                } else {
                    perform_capture(app, state, &key, assessment.destructive);
                }
            }
        }
        Action::CaptureConfirmed(key) => {
            let destructive = app
                .store
                .find(&key)
                .map(|e| app.assess(e).destructive)
                .unwrap_or(false);
            perform_capture(app, state, &key, destructive);
        }
    }
    Ok(())
}

/// The move an edit form asks for, or `None` to leave the entry where it is.
/// The scope field is seeded from the entry, so a placement naming the scope
/// the entry is already in is not a move, and a blank field is the user saying
/// nothing rather than asking for the configured default.
fn requested_move<'a>(old: &EntryKey, placement: &'a Placement) -> Option<&'a Placement> {
    match placement {
        Placement::Default => None,
        Placement::Global if old.scope.is_none() => None,
        Placement::Scope(id) if old.scope.as_deref() == Some(id.as_str()) => None,
        other => Some(other),
    }
}

fn perform_capture(app: &mut App, state: &mut UiState, key: &EntryKey, destructive: bool) {
    let entry = match app.store.find(key).cloned() {
        Some(e) => e,
        None => return,
    };
    match app.capture(&entry) {
        Ok(result) => {
            let note = if result.timed_out { " (timed out)" } else { "" };
            let backend = result.backend;
            if let Err(e) = app.record_capture(key, result.output.trim().to_string(), destructive) {
                state.set_status(format!("Capture save failed: {e}"));
            } else {
                refresh(app, state);
                let label = key.label();
                state.set_status(format!("Captured '{label}' via {backend}{note}"));
            }
        }
        Err(e) => state.set_status(format!("Capture failed: {e}")),
    }
}

/// Rebuild the view from the App's current store and scope registry.
fn refresh(app: &App, state: &mut UiState) {
    state.set_entries(app);
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
    use crate::scopes::ScopeRegistry;
    use crate::search::ScopeFilter;
    use crate::store::Store;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    /// A UI state over `entries` with no scopes registered. Nothing built this
    /// way writes, so the root path is never touched.
    fn state_over(entries: Vec<Entry>) -> UiState {
        let mut store = Store::default();
        for entry in entries {
            store.add(entry).unwrap();
        }
        UiState::new(&App::with(
            Paths::with_root(PathBuf::from("/cmd-man")),
            Config::default(),
            store,
        ))
    }

    /// An app rooted entirely in `dir`, including its home, so scope walks stop
    /// at the temporary tree. The store and registry are read back from disk so
    /// two apps built from the same `dir` see each other's writes.
    fn test_app_with(dir: &TempDir, config: Config) -> App {
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let store = Store::load(&paths).unwrap();
        let scopes = ScopeRegistry::load(&paths).unwrap();
        App::with(paths, config, store)
            .with_scopes(scopes)
            .with_home(dir.path().canonicalize().unwrap())
    }

    #[test]
    fn browse_quit_keys() {
        let mut state = state_over(vec![]);
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
    }

    #[test]
    fn slash_enters_search_mode() {
        let mut state = state_over(vec![]);
        handle_key(&mut state, key(KeyCode::Char('/')));
        assert_eq!(state.mode, Mode::Search);
        handle_key(&mut state, key(KeyCode::Char('x')));
        assert_eq!(state.query, "x");
        handle_key(&mut state, key(KeyCode::Esc));
        assert_eq!(state.mode, Mode::Browse);
    }

    #[test]
    fn delete_flow_confirms_then_removes() {
        let mut state = state_over(vec![Entry::new("gitsw", Kind::Alias, "git switch", "s")]);
        let none = handle_key(&mut state, key(KeyCode::Char('d')));
        assert!(none.is_none());
        assert!(matches!(state.mode, Mode::Confirm { .. }));
        let action = handle_key(&mut state, key(KeyCode::Char('y')));
        assert_eq!(action, Some(Action::Delete(EntryKey::global("gitsw"))));
    }

    #[test]
    fn confirm_no_cancels() {
        let mut state = state_over(vec![Entry::new("gitsw", Kind::Alias, "git switch", "s")]);
        handle_key(&mut state, key(KeyCode::Char('d')));
        let action = handle_key(&mut state, key(KeyCode::Char('n')));
        assert!(action.is_none());
        assert_eq!(state.mode, Mode::Browse);
    }

    #[test]
    fn the_s_key_cycles_the_scope_filter() {
        let dir = TempDir::new().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let mut app = test_app_with(&dir, Config::default()).with_cwd(api.canonicalize().unwrap());
        app.add(
            Entry::new("dep", Kind::Alias, "make deploy", "d"),
            &Placement::Here,
        )
        .unwrap();

        let mut state = UiState::new(&app);
        let action = handle_key(&mut state, key(KeyCode::Char('s'))).unwrap();
        assert_eq!(action, Action::CycleScope);
        execute(action, &mut app, &mut state).unwrap();
        assert_eq!(
            state.scope_filter(),
            &ScopeFilter::ActiveIn(vec!["api".to_string()])
        );
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

    /// A valid add form, typed through the key handler, carrying `scope` in
    /// its scope field.
    fn typed_add_form(scope: &str) -> Option<Form> {
        let mut form = Some(Form::new_entry());
        let type_in = |form: &mut Option<Form>, text: &str| {
            for c in text.chars() {
                handle_form_key(form, key(KeyCode::Char(c)));
            }
        };
        type_in(&mut form, "gitsw"); // focus starts on the name field
        handle_form_key(&mut form, key(KeyCode::Tab)); // kind, already alias
        handle_form_key(&mut form, key(KeyCode::Tab)); // command
        type_in(&mut form, "git switch");
        handle_form_key(&mut form, key(KeyCode::Tab)); // description
        type_in(&mut form, "switch branches");
        form.as_mut().unwrap().fields[form::SCOPE].value = scope.into();
        form
    }

    #[test]
    fn the_scope_field_reaches_the_submit_action() {
        for (typed, expected) in [
            ("", Placement::Default),
            ("here", Placement::Here),
            ("global", Placement::Global),
            ("api", Placement::Scope("api".into())),
        ] {
            let mut form = typed_add_form(typed);
            match handle_form_key(&mut form, key(KeyCode::Enter)) {
                Some(Action::Submit { placement, .. }) => assert_eq!(placement, expected),
                other => panic!("expected a submit for '{typed}', got {other:?}"),
            }
        }
    }

    #[test]
    fn a_form_asking_for_here_creates_the_entry_in_that_scope() {
        let dir = TempDir::new().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let mut app = test_app_with(&dir, Config::default()).with_cwd(api.canonicalize().unwrap());
        let mut state = state_over(vec![]);

        let mut form = typed_add_form("here");
        let action = handle_form_key(&mut form, key(KeyCode::Enter)).unwrap();
        execute(action, &mut app, &mut state).unwrap();

        assert_eq!(
            app.store.find_all_named("gitsw")[0].scope.as_deref(),
            Some("api")
        );
    }

    #[test]
    fn submit_action_adds_entry_to_app() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let mut app = App::with(paths, Config::default(), Store::default());
        let mut state = state_over(vec![]);
        let entry = Entry::new("gitsw", Kind::Alias, "git switch", "switch");
        execute(
            Action::Submit {
                original: None,
                entry: Box::new(entry),
                placement: Placement::Default,
                autofill: false,
            },
            &mut app,
            &mut state,
        )
        .unwrap();
        assert!(app.store.find(&EntryKey::global("gitsw")).is_some());
        assert_eq!(state.filtered_entries().len(), 1);
    }

    #[test]
    fn editing_a_scoped_entry_keeps_it_in_its_scope() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let mut app = App::with(paths, Config::default(), Store::default());
        let mut state = state_over(vec![]);
        let mut scoped = Entry::new("ll", Kind::Alias, "ls -la", "list");
        scoped.scope = Some("api".into());
        app.store.add(scoped.clone()).unwrap();

        let form = Form::edit(&scoped);
        let original = form.original.clone();
        let rebuilt = form.to_entry().unwrap();
        execute(
            Action::Submit {
                original,
                entry: Box::new(rebuilt),
                placement: form.placement(),
                autofill: false,
            },
            &mut app,
            &mut state,
        )
        .unwrap();

        assert_eq!(app.store.entries().len(), 1);
        assert_eq!(app.store.entries()[0].scope, Some("api".to_string()));
    }

    #[test]
    fn changing_the_scope_field_on_an_edit_form_moves_the_entry() {
        let dir = TempDir::new().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let mut app = test_app_with(&dir, Config::default()).with_cwd(api.canonicalize().unwrap());
        let mut state = state_over(vec![]);
        let global = Entry::new("ll", Kind::Alias, "ls -la", "list");
        app.store.add(global.clone()).unwrap();

        let mut form = Form::edit(&global);
        form.fields[form::SCOPE].value = "here".into();
        let original = form.original.clone();
        let rebuilt = form.to_entry().unwrap();
        execute(
            Action::Submit {
                original,
                entry: Box::new(rebuilt),
                placement: form.placement(),
                autofill: false,
            },
            &mut app,
            &mut state,
        )
        .unwrap();

        assert_eq!(app.store.entries().len(), 1);
        assert_eq!(app.store.entries()[0].scope.as_deref(), Some("api"));
        assert_eq!(state.status, "Saved 'll (api)'");
    }

    #[test]
    fn saving_a_body_only_one_shell_reads_says_so_in_the_status_line() {
        let dir = TempDir::new().unwrap();
        let mut app = test_app_with(&dir, Config::default());
        let mut state = state_over(vec![]);
        let entry = Entry::new("ft", Kind::Function, "echo a |& cat", "d");

        execute(
            Action::Submit {
                original: None,
                entry: Box::new(entry),
                placement: Placement::Global,
                autofill: false,
            },
            &mut app,
            &mut state,
        )
        .unwrap();

        assert_eq!(app.store.entries().len(), 1);
        assert!(
            state.status.starts_with("Saved 'ft'; ")
                && state.status.contains("left out of the bash file"),
            "status was: {}",
            state.status
        );
    }

    #[test]
    fn the_tui_and_the_cli_agree_on_placement_under_repo_mode() {
        let dir = TempDir::new().unwrap();
        let proj = dir.path().join("proj");
        let deep = proj.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        let cwd = deep.canonicalize().unwrap();

        let mut config = Config::default();
        config.scope.default = "repo".into();

        let mut cli_app = test_app_with(&dir, config.clone()).with_cwd(cwd.clone());
        let cli_key = cli_app
            .add(
                Entry::new("a", Kind::Alias, "echo a", "d"),
                &Placement::Default,
            )
            .unwrap();

        // Built after the CLI app has persisted, so it sees the same registry.
        let mut tui_app = test_app_with(&dir, config).with_cwd(cwd);
        let mut state = UiState::new(&tui_app);
        execute(
            Action::Submit {
                original: None,
                entry: Box::new(Entry::new("b", Kind::Alias, "echo b", "d")),
                placement: Placement::Default,
                autofill: false,
            },
            &mut tui_app,
            &mut state,
        )
        .unwrap();

        let tui_scope = tui_app.store.find_all_named("b")[0].scope.clone();
        assert_eq!(cli_key.scope, tui_scope);
        assert_eq!(cli_key.scope.as_deref(), Some("proj"));
    }

    #[test]
    fn abandoning_the_add_form_registers_no_scope() {
        let dir = TempDir::new().unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        let mut config = Config::default();
        config.scope.default = "repo".into();
        let app = test_app_with(&dir, config).with_cwd(proj.canonicalize().unwrap());

        let _form = Form::new_entry();
        assert!(app.scopes.is_empty());
    }
}

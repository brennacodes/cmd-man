//! The add/edit form model used by the TUI.

use crate::model::{Entry, EntryKey, Kind};
use crate::scopes::Placement;

/// One editable field.
pub struct Field {
    pub label: &'static str,
    pub value: String,
    pub required: bool,
    /// Shown for context but neither focusable nor editable.
    pub readonly: bool,
}

/// A multi-field form for creating or editing an entry.
pub struct Form {
    pub title: String,
    /// `Some(key)` when editing an existing entry.
    pub original: Option<EntryKey>,
    pub fields: Vec<Field>,
    pub focus: usize,
    /// A validation error to show after a failed save attempt.
    pub error: Option<String>,
    /// The command help was last fetched for, to avoid re-fetching on save.
    pub helped_command: Option<String>,
}

const NAME: usize = 0;
const KIND: usize = 1;
const COMMAND: usize = 2;
const DESCRIPTION: usize = 3;
const CATEGORY: usize = 4;
const TAGS: usize = 5;
const USAGE: usize = 6;
const OPTIONS: usize = 7;
const EXAMPLES: usize = 8;
/// Appended rather than inserted so the constants above keep their positions.
pub const SCOPE: usize = 9;

impl Form {
    fn blank_fields() -> Vec<Field> {
        vec![
            Field {
                label: "Alias (trigger, no spaces)",
                value: String::new(),
                required: true,
                readonly: false,
            },
            Field {
                label: "Kind (alias/function)",
                value: "alias".into(),
                required: true,
                readonly: false,
            },
            Field {
                label: "Command / body",
                value: String::new(),
                required: true,
                readonly: false,
            },
            Field {
                label: "Description",
                value: String::new(),
                required: true,
                readonly: false,
            },
            Field {
                label: "Category",
                value: "general".into(),
                required: false,
                readonly: false,
            },
            Field {
                label: "Tags (comma separated)",
                value: String::new(),
                required: false,
                readonly: false,
            },
            Field {
                label: "Usage",
                value: String::new(),
                required: false,
                readonly: false,
            },
            Field {
                label: "Options",
                value: String::new(),
                required: false,
                readonly: false,
            },
            Field {
                label: "Examples",
                value: String::new(),
                required: false,
                readonly: false,
            },
            Field {
                label: "Scope (global/here/<id>)",
                value: String::new(),
                required: false,
                readonly: false,
            },
        ]
    }

    /// A form for creating a new entry.
    pub fn new_entry() -> Self {
        Form {
            title: "Add entry".into(),
            original: None,
            fields: Self::blank_fields(),
            focus: 0,
            error: None,
            helped_command: None,
        }
    }

    /// A form pre-filled from an existing entry.
    pub fn edit(entry: &Entry) -> Self {
        let mut fields = Self::blank_fields();
        fields[NAME].value = entry.name.clone();
        fields[KIND].value = entry.kind.to_string();
        fields[COMMAND].value = entry.command.clone();
        fields[DESCRIPTION].value = entry.description.clone();
        fields[CATEGORY].value = entry.category.clone();
        fields[TAGS].value = entry.tags.join(", ");
        fields[USAGE].value = entry.usage.clone();
        fields[OPTIONS].value = entry.options.clone();
        fields[EXAMPLES].value = entry.examples.clone();
        // An edit never moves an entry, so the scope is shown, not offered.
        fields[SCOPE].value = entry.scope.clone().unwrap_or_else(|| "global".into());
        fields[SCOPE].label = "Scope (read only)";
        fields[SCOPE].readonly = true;
        Form {
            title: format!("Edit {}", entry.name),
            original: Some(EntryKey::of(entry)),
            fields,
            focus: 0,
            error: None,
            helped_command: None,
        }
    }

    pub fn focus_next(&mut self) {
        self.step_focus(true);
    }

    pub fn focus_prev(&mut self) {
        self.step_focus(false);
    }

    /// Move the focus to the next editable field in either direction, leaving
    /// it alone when every field is read only.
    fn step_focus(&mut self, forward: bool) {
        let len = self.fields.len();
        for step in 1..=len {
            let idx = if forward {
                (self.focus + step) % len
            } else {
                (self.focus + len - step) % len
            };
            if !self.fields[idx].readonly {
                self.focus = idx;
                return;
            }
        }
    }

    pub fn input(&mut self, c: char) {
        if self.fields[self.focus].readonly {
            return;
        }
        self.error = None;
        self.fields[self.focus].value.push(c);
    }

    pub fn backspace(&mut self) {
        if self.fields[self.focus].readonly {
            return;
        }
        self.error = None;
        self.fields[self.focus].value.pop();
    }

    /// The placement the scope field asks for on an add form. Blank means the
    /// configured default.
    pub fn placement(&self) -> Placement {
        Placement::parse(&self.fields[SCOPE].value)
    }

    /// The current value of the command field, used to fetch help.
    pub fn command_value(&self) -> &str {
        self.fields[COMMAND].value.trim()
    }

    /// Fill empty description/usage/options/examples fields from help sections.
    /// Returns true when anything was filled.
    pub fn fill_empty_help(&mut self, sections: &crate::capture::HelpSections) -> bool {
        let mut changed = false;
        for (idx, value) in [
            (DESCRIPTION, &sections.description),
            (USAGE, &sections.usage),
            (OPTIONS, &sections.options),
            (EXAMPLES, &sections.examples),
        ] {
            if self.fields[idx].value.trim().is_empty() && !value.is_empty() {
                self.fields[idx].value = value.clone();
                changed = true;
            }
        }
        changed
    }

    /// Build an entry from the form, or return a validation error message.
    pub fn to_entry(&self) -> Result<Entry, String> {
        let kind: Kind = self.fields[KIND].value.parse()?;
        let name = self.fields[NAME].value.trim().to_string();
        let command = self.fields[COMMAND].value.trim().to_string();
        let description = self.fields[DESCRIPTION].value.trim().to_string();

        let mut entry = Entry::new(name, kind, command, description);
        entry.scope = self.original.as_ref().and_then(|k| k.scope.clone());
        let category = self.fields[CATEGORY].value.trim();
        if !category.is_empty() {
            entry.category = category.to_string();
        }
        entry.tags = self.fields[TAGS]
            .value
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        entry.usage = self.fields[USAGE].value.trim().to_string();
        entry.options = self.fields[OPTIONS].value.trim().to_string();
        entry.examples = self.fields[EXAMPLES].value.trim().to_string();

        entry.validate()?;
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_form_builds_valid_entry() {
        let mut form = Form::new_entry();
        for c in "gitsw".chars() {
            form.input(c);
        }
        form.focus = COMMAND;
        for c in "git switch".chars() {
            form.input(c);
        }
        form.focus = DESCRIPTION;
        for c in "switch branches".chars() {
            form.input(c);
        }
        let entry = form.to_entry().unwrap();
        assert_eq!(entry.name, "gitsw");
        assert_eq!(entry.kind, Kind::Alias);
        assert_eq!(entry.command, "git switch");
    }

    #[test]
    fn edit_form_round_trips_fields() {
        let mut e = Entry::new("kruby", Kind::Function, "kill ruby", "kill it");
        e.category = "ruby".into();
        e.tags = vec!["ruby".into(), "server".into()];
        let form = Form::edit(&e);
        assert_eq!(form.original, Some(EntryKey::global("kruby")));
        let back = form.to_entry().unwrap();
        assert_eq!(back.kind, Kind::Function);
        assert_eq!(back.tags, vec!["ruby", "server"]);
        assert_eq!(back.category, "ruby");
    }

    #[test]
    fn edit_form_round_trips_the_scope() {
        let mut e = Entry::new("ll", Kind::Alias, "ls -la", "list");
        e.scope = Some("api".into());
        let form = Form::edit(&e);
        assert_eq!(
            form.original,
            Some(EntryKey {
                scope: Some("api".into()),
                name: "ll".into()
            })
        );
        assert_eq!(form.to_entry().unwrap().scope, Some("api".to_string()));
    }

    #[test]
    fn new_form_builds_a_global_entry() {
        let mut form = Form::new_entry();
        for c in "ll".chars() {
            form.input(c);
        }
        form.fields[COMMAND].value = "ls -la".into();
        form.fields[DESCRIPTION].value = "list".into();
        assert_eq!(form.to_entry().unwrap().scope, None);
    }

    #[test]
    fn a_blank_scope_field_means_the_default_placement() {
        let form = Form::new_entry();
        assert_eq!(form.placement(), Placement::Default);
    }

    #[test]
    fn the_scope_field_parses_ids_and_keywords() {
        let mut form = Form::new_entry();
        form.fields[SCOPE].value = "api".into();
        assert_eq!(form.placement(), Placement::Scope("api".into()));
        form.fields[SCOPE].value = "global".into();
        assert_eq!(form.placement(), Placement::Global);
        form.fields[SCOPE].value = "here".into();
        assert_eq!(form.placement(), Placement::Here);
    }

    #[test]
    fn editing_a_scoped_entry_shows_its_scope() {
        let mut entry = Entry::new("ll", Kind::Alias, "ls -lah", "d");
        entry.scope = Some("api".into());
        let form = Form::edit(&entry);
        assert_eq!(form.fields[SCOPE].value, "api");
        assert_eq!(
            form.original,
            Some(EntryKey {
                scope: Some("api".into()),
                name: "ll".into()
            })
        );
    }

    #[test]
    fn the_edit_form_shows_the_scope_without_offering_to_change_it() {
        let mut entry = Entry::new("ll", Kind::Alias, "ls -lah", "d");
        entry.scope = Some("api".into());
        let mut form = Form::edit(&entry);
        assert!(form.fields[SCOPE].readonly);

        form.focus = EXAMPLES;
        form.focus_next();
        assert_eq!(form.focus, NAME, "focus must skip the read only scope");
        form.focus_prev();
        assert_eq!(form.focus, EXAMPLES);

        form.focus = SCOPE;
        form.input('x');
        form.backspace();
        assert_eq!(form.fields[SCOPE].value, "api");
    }

    #[test]
    fn the_edit_form_labels_an_unscoped_entry_global() {
        let entry = Entry::new("ll", Kind::Alias, "ls -lah", "d");
        let form = Form::edit(&entry);
        assert_eq!(form.fields[SCOPE].value, "global");
        assert_eq!(form.to_entry().unwrap().scope, None);
    }

    #[test]
    fn appending_scope_does_not_renumber_the_other_fields() {
        let form = Form::new_entry();
        assert_eq!(form.fields.len(), SCOPE + 1);
        const { assert!(SCOPE > EXAMPLES) };
    }

    #[test]
    fn invalid_kind_reports_error() {
        let mut form = Form::new_entry();
        form.fields[KIND].value = "banana".into();
        assert!(form.to_entry().is_err());
    }

    #[test]
    fn missing_required_fields_error() {
        let form = Form::new_entry();
        assert!(form.to_entry().is_err());
    }

    #[test]
    fn focus_wraps() {
        let mut form = Form::new_entry();
        form.focus = form.fields.len() - 1;
        form.focus_next();
        assert_eq!(form.focus, 0);
        form.focus_prev();
        assert_eq!(form.focus, form.fields.len() - 1);
    }
}

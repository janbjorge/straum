//! `straum shell`: a prompt at a running Straum. Each command goes to
//! `answer`, which reads it and asks Straum through `client.rs`: `\d`
//! for the views, `\d VIEW` for a view's spec, `\rows VIEW [START
//! [COUNT]]` for a view's rows, a negative START counting back from the
//! end, or an ask written as `QUERY; PARAMETER...`, where a value in
//! single quotes may hold spaces. A command that starts with a backslash
//! is one line; an ask may take several, and `entry` says when it is
//! whole: at the line that holds its semicolon, or at a blank line. At a
//! terminal, `interactive` edits the command with rustyline, which asks
//! `entry` at each Enter, Tab completes from the views the Straum serves
//! (see `complete.rs`), the command entered last that starts as this one
//! does is suggested past the cursor, the commands entered are kept
//! between sessions, and an ask or a view's rows end in a line that says
//! how long they took, as `client.rs` timed them. On a pipe, `piped`
//! reads lines until `entry` finds a command whole, or the input ends,
//! and prints the answers alone.

use std::borrow::Cow;
use std::env;
use std::ffi::OsString;
use std::fmt::{self, Display};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hint, Hinter, HistoryHinter};
use rustyline::history::{DefaultHistory, History};
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{CompletionType, Config, Context, Editor};

use crate::client::{ClientError, Output, RowWindow, Server, Timed, ask, rows, spec, views};
use crate::complete::{Completion, View, complete, hint, read_views};
use crate::help::SHELL_HELP;
use crate::line::parameter_words;

const PROMPT: &str = "straum> ";
// ANSI escapes that dim a hint and end the dimming, so it reads as a note.
const DIM: &str = "\x1b[2m";
const UNDIM: &str = "\x1b[0m";

/// A line the shell could not answer.
#[derive(Debug)]
enum LineError {
    NoSuchCommand(String),
    NotARowNumber(String),
    Client(ClientError),
}

impl Display for LineError {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            LineError::NoSuchCommand(line) => {
                write!(formatter, "no command {line:?}; \\? lists them")
            }
            LineError::NotARowNumber(word) => write!(
                formatter,
                "\\rows takes VIEW [START [COUNT]], whole numbers, not {word:?}"
            ),
            LineError::Client(error) => error.fmt(formatter),
        }
    }
}

impl From<ClientError> for LineError {
    fn from(error: ClientError) -> LineError {
        LineError::Client(error)
    }
}

/// What to do after a line.
enum Outcome {
    Print(String),
    Failed(LineError),
    Leave,
}

/// Whether an ask and a view's rows end in a line that says how long they
/// took. It is for a person at a terminal: on a pipe it would make a
/// script's output differ from run to run.
#[derive(Clone, Copy)]
enum Timings {
    Shown,
    Hidden,
}

/// What one line asks for.
fn answer(server: &Server, line: &str, timings: Timings) -> Outcome {
    let words: Vec<&str> = line.split_whitespace().collect();
    let answered = match words[..] {
        [] => Ok(String::new()),
        ["\\q"] => return Outcome::Leave,
        ["\\?"] => Ok(shell_commands()),
        ["\\d"] => views(server, Output::Text),
        ["\\d", view] => spec(server, view),
        ["\\rows", view, ref bounds @ ..] if bounds.len() <= 2 => {
            let window = match row_window(bounds) {
                Ok(window) => window,
                Err(error) => return Outcome::Failed(error),
            };
            timed(rows(server, view, window), timings)
        }
        [command, ..] if command.starts_with('\\') => {
            return Outcome::Failed(LineError::NoSuchCommand(line.to_owned()));
        }
        _ => {
            let (query, words) = line.split_once(';').unwrap_or((line, ""));
            let mut parameters = Vec::new();
            for word in parameter_words(words) {
                parameters.push(word.to_owned());
            }
            let answered = ask(server, query.trim_end(), &parameters, Output::Text);
            timed(answered, timings)
        }
    };
    match answered {
        Ok(text) => Outcome::Print(text),
        Err(error) => Outcome::Failed(LineError::from(error)),
    }
}

/// What an ask or a view's rows print: the table, and under it how long
/// it took where `timings` shows that.
fn timed(answered: Result<Timed, ClientError>, timings: Timings) -> Result<String, ClientError> {
    let Timed { text, timing } = answered?;
    match timings {
        Timings::Shown => Ok(format!("{text}{timing}\n")),
        Timings::Hidden => Ok(text),
    }
}

/// The START and COUNT after `\rows VIEW`, each optional.
fn row_window(bounds: &[&str]) -> Result<RowWindow, LineError> {
    let mut window = RowWindow::default();
    if let Some(start) = bounds.first() {
        let Ok(start) = start.parse() else {
            return Err(LineError::NotARowNumber((*start).to_owned()));
        };
        window.start = Some(start);
    }
    if let Some(count) = bounds.get(1) {
        let Ok(count) = count.parse() else {
            return Err(LineError::NotARowNumber((*count).to_owned()));
        };
        window.count = Some(count);
    }
    Ok(window)
}

fn shell_commands() -> String {
    let mut text = String::new();
    for line in SHELL_HELP.lines() {
        // The flags come last.
        if line.starts_with("  --") {
            break;
        }
        if line.starts_with("  ") {
            text.push_str(line);
            text.push('\n');
        }
    }
    text
}

/// Whether what has been entered so far is a whole command.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Whole,
    Continued,
}

/// Whether `entered`, its lines joined by newlines, is a whole command.
/// A command that starts with a backslash is one line. An ask is whole
/// at the line that holds its semicolon, so its parameters go on that
/// line, or at a blank line, which ends an ask with no parameters. The
/// semicolon is the shell's and never reaches Straum, which refuses one
/// that ends an ask (ADR-0004).
fn entry(entered: &str) -> Entry {
    let command = entered.trim();
    if command.is_empty() || command.starts_with('\\') || command.contains(';') {
        return Entry::Whole;
    }
    let last_line = entered.rsplit('\n').next().unwrap_or_default();
    if last_line.trim().is_empty() {
        Entry::Whole
    } else {
        Entry::Continued
    }
}

/// A stream of the shell on pipes that failed.
#[derive(Debug)]
pub(crate) enum PipeError {
    /// Reading the commands.
    Reading(io::Error),
    /// Writing an answer, or the error of a command.
    Writing(io::Error),
}

/// Answers each command of `input` on `output` and says each failure on
/// `errors`, going on after one as psql does. A command is the lines
/// `entry` takes, or what is left when the input ends. How many commands
/// failed, so a script can tell. Both writers are flushed at the end, so
/// a write that fails there is told too.
// `impl`: the binary passes the terminal's locked streams, tests pass
// byte buffers and a writer that always fails.
pub(crate) fn piped(
    server: &Server,
    mut input: impl BufRead,
    mut output: impl Write,
    mut errors: impl Write,
) -> Result<usize, PipeError> {
    let failed = answer_each(server, &mut input, &mut output, &mut errors)?;
    match output.flush() {
        // The reader has left, as `head` does after its lines.
        Err(error) if error.kind() != io::ErrorKind::BrokenPipe => {
            return Err(PipeError::Writing(error));
        }
        _ => {}
    }
    errors.flush().map_err(PipeError::Writing)?;
    Ok(failed)
}

// `impl`: as `piped`, whose streams it borrows.
fn answer_each(
    server: &Server,
    mut input: impl BufRead,
    mut output: impl Write,
    mut errors: impl Write,
) -> Result<usize, PipeError> {
    let mut failed = 0;
    let mut entered = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        let read_bytes = input.read_line(&mut line).map_err(PipeError::Reading)?;
        if read_bytes > 0 {
            if !entered.is_empty() {
                entered.push('\n');
            }
            entered.push_str(line.trim_end_matches(['\n', '\r']));
            if entry(&entered) == Entry::Continued {
                continue;
            }
        } else if entered.trim().is_empty() {
            return Ok(failed);
        }
        // Here a command is whole, or the input ended an ask still
        // taking lines.
        match answer(server, entered.trim(), Timings::Hidden) {
            Outcome::Leave => return Ok(failed),
            Outcome::Print(text) => match output.write_all(text.as_bytes()) {
                // The reader has left, as `head` does after its lines.
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(failed),
                written => written.map_err(PipeError::Writing)?,
            },
            Outcome::Failed(error) => {
                failed += 1;
                writeln!(errors, "straum: {error}").map_err(PipeError::Writing)?;
            }
        }
        if read_bytes == 0 {
            return Ok(failed);
        }
        entered.clear();
    }
}

/// Where the lines entered are kept between sessions: under the XDG
/// state directory, which is `~/.local/state` unless `XDG_STATE_HOME`
/// names another. Nowhere without a home, as in Straum's image.
fn history_file(state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let state = match state_home {
        Some(state_home) if is_absolute(&state_home) => PathBuf::from(state_home),
        _ => {
            let home = home?;
            if !is_absolute(&home) {
                return None;
            }
            PathBuf::from(home).join(".local").join("state")
        }
    };
    Some(state.join("straum").join("history"))
}

// The XDG rules take only an absolute path.
fn is_absolute(path: &OsString) -> bool {
    PathBuf::from(path).is_absolute()
}

/// How the line is edited: Tab completes what all candidates share and
/// lists them at once when they differ. Waiting for a second Tab showed
/// nothing where the candidates differ in case, as `fare` and `FROM` do.
fn editor_config() -> Config {
    Config::builder()
        .completion_type(CompletionType::List)
        .completion_show_all_if_ambiguous(true)
        .build()
}

/// The prompt at a terminal, with line editing, history and completion.
pub(crate) fn interactive(server: &Server) -> Result<(), ReadlineError> {
    let mut editor: Editor<Names, DefaultHistory> = Editor::with_config(editor_config())?;
    editor.set_helper(Some(Names { views: Vec::new() }));
    let history = history_file(env::var_os("XDG_STATE_HOME"), env::var_os("HOME"));
    if let Some(file) = &history {
        // There is none before the first session, nor its directory.
        editor.load_history(file).ok();
        if let Some(directory) = file.parent() {
            fs::create_dir_all(directory).ok();
        }
    }
    println!("Straum at {server}. Tab completes; \\? lists the commands, \\q leaves.");
    prompt_until_leaving(server, &mut editor, history.as_deref())
}

/// Adds a line entered to the history and saves the history at once:
/// Ctrl-C while an ask runs ends the process, and with it every line
/// not yet saved.
fn keep_entered(
    history: &mut DefaultHistory,
    line: &str,
    file: Option<&Path>,
) -> Result<(), ReadlineError> {
    history.add(line)?;
    if let Some(file) = file {
        // A history that can't be kept, as on a read-only disk, is lost
        // and nothing else.
        history.save(file).ok();
    }
    Ok(())
}

fn prompt_until_leaving(
    server: &Server,
    editor: &mut Editor<Names, DefaultHistory>,
    history: Option<&Path>,
) -> Result<(), ReadlineError> {
    loop {
        // Read again before each line: a view's buckets move on.
        if let Ok(json) = views(server, Output::Json)
            && let Some(held) = read_views(&json)
            && let Some(names) = editor.helper_mut()
        {
            names.views = held;
        }
        let line = match editor.readline(PROMPT) {
            Ok(line) => line,
            // Ctrl-C drops the line, as in psql.
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => return Ok(()),
            Err(error) => return Err(error),
        };
        keep_entered(editor.history_mut(), line.trim(), history)?;
        match answer(server, line.trim(), Timings::Shown) {
            Outcome::Leave => return Ok(()),
            Outcome::Print(text) => print!("{text}"),
            Outcome::Failed(error) => eprintln!("straum: {error}"),
        }
    }
}

/// What rustyline asks while a line is edited: the views to complete from.
struct Names {
    views: Vec<View>,
}

/// What is shown dimmed past the cursor, and what the right arrow takes
/// into the line: the rest of a line entered before, or nothing for a
/// note on the next parameter, which describes what to type and isn't it.
struct Note {
    text: String,
    completion: Option<String>,
}

impl Hint for Note {
    fn display(&self) -> &str {
        &self.text
    }

    fn completion(&self) -> Option<&str> {
        self.completion.as_deref()
    }
}

impl Completer for Names {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        cursor: usize,
        _: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let Completion { start, candidates } = complete(&self.views, line, cursor);
        Ok((start, candidates))
    }
}

impl Hinter for Names {
    type Hint = Note;

    fn hint(&self, line: &str, cursor: usize, context: &Context<'_>) -> Option<Note> {
        // A line entered before comes first: it is what the user typed, so
        // it is right more often than a guess, and taking it saves the most.
        if let Some(rest) = HistoryHinter::new().hint(line, cursor, context) {
            return Some(Note {
                text: rest.clone(),
                completion: Some(rest),
            });
        }
        let text = hint(&self.views, line, cursor)?;
        Some(Note {
            text,
            completion: None,
        })
    }
}

impl Highlighter for Names {
    // The lifetime is rustyline's: the hint may come back borrowed.
    fn highlight_hint<'hint>(&self, hint: &'hint str) -> Cow<'hint, str> {
        Cow::Owned(format!("{DIM}{hint}{UNDIM}"))
    }
}

impl Validator for Names {
    // Enter on an ask that is not whole yet starts a new line of it.
    fn validate(&self, context: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        Ok(validation(context.input()))
    }
}

/// What rustyline is told of the command being edited.
fn validation(entered: &str) -> ValidationResult {
    match entry(entered) {
        Entry::Whole => ValidationResult::Valid(None),
        Entry::Continued => ValidationResult::Incomplete,
    }
}

impl rustyline::Helper for Names {}

#[cfg(test)]
mod tests;

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
use std::path::PathBuf;

use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hint, Hinter, HistoryHinter};
use rustyline::history::DefaultHistory;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{CompletionType, Config, Context, Editor};

use crate::client::{
    ClientError, Output, RowWindow, Server, Timed, ask, load_views, parameter_words, rows, spec,
    views,
};
use crate::complete::{Completion, View, complete, hint};
use crate::help::SHELL_HELP;

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
            let (query, parameters) = line.split_once(';').unwrap_or((line, ""));
            let parameters: Vec<String> = parameter_words(parameters)
                .into_iter()
                .map(str::to_owned)
                .collect();
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

/// Answers each command of `input` on `output` and says each failure on
/// `errors`, going on after one as psql does. A command is the lines
/// `entry` takes, or what is left when the input ends. How many commands
/// failed, so a script can tell.
// `impl`: the binary passes the terminal's locked streams, tests pass
// byte buffers and a writer that always fails.
pub(crate) fn piped(
    server: &Server,
    mut input: impl BufRead,
    mut output: impl Write,
    mut errors: impl Write,
) -> io::Result<usize> {
    let mut failed = 0;
    let mut entered = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        let read_bytes = input.read_line(&mut line)?;
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
                written => written?,
            },
            Outcome::Failed(error) => {
                failed += 1;
                writeln!(errors, "straum: {error}")?;
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
        // There is none before the first session.
        editor.load_history(file).ok();
    }
    println!("Straum at {server}. Tab completes; \\? lists the commands, \\q leaves.");
    let result = prompt_until_leaving(server, &mut editor);
    if let Some(file) = &history {
        // A history that can't be kept, as on a read-only disk, is lost
        // and nothing else.
        if let Some(directory) = file.parent() {
            fs::create_dir_all(directory).ok();
        }
        editor.save_history(file).ok();
    }
    result
}

fn prompt_until_leaving(
    server: &Server,
    editor: &mut Editor<Names, DefaultHistory>,
) -> Result<(), ReadlineError> {
    loop {
        // Read again before each line: a view's buckets move on.
        if let (Some(views), Some(names)) = (load_views(server), editor.helper_mut()) {
            names.views = views;
        }
        let line = match editor.readline(PROMPT) {
            Ok(line) => line,
            // Ctrl-C drops the line, as in psql.
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => return Ok(()),
            Err(error) => return Err(error),
        };
        editor.add_history_entry(line.trim())?;
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
mod tests {
    use super::*;
    use crate::complete::read_views;
    use crate::testing::{GET_ROWS, GET_VIEWS, POST_ASK, POST_ASK_SPACED, answering, server};
    use rustyline::history::History;

    #[test]
    fn rustyline_is_given_the_candidates_and_the_hint() {
        let logins = r#"{"views":[{"name":"logins","time":"bucket","measures":["logins"],
            "keys":[{"name":"bucket","type":"timestamp"}]}]}"#;
        let names = Names {
            views: read_views(logins).unwrap(),
        };
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let (start, candidates) = names.complete("\\rows lo", 8, &context).unwrap();
        assert_eq!((start, candidates), (6, vec!["logins ".to_owned()]));
        let line = "SELECT logins FROM logins WHERE bucket >= $1; ";
        let note = Hinter::hint(&names, line, line.len(), &context).unwrap();
        assert_eq!(note.display(), "(the timestamp for bucket >= $1)");
        assert_eq!(note.completion(), None);
        assert_eq!(names.highlight_hint("$1"), "\x1b[2m$1\x1b[0m");
        assert!(Hinter::hint(&names, line, 0, &context).is_none());
    }

    #[test]
    fn a_line_entered_before_is_suggested_and_the_arrow_takes_it() {
        let logins = r#"{"views":[{"name":"logins","time":"bucket","measures":["logins"],
            "keys":[{"name":"bucket","type":"timestamp"}]}]}"#;
        let names = Names {
            views: read_views(logins).unwrap(),
        };
        let mut history = DefaultHistory::new();
        let asked = "SELECT logins FROM logins WHERE bucket >= $1; 0";
        history.add(asked).unwrap();
        history.add("\\d").unwrap();
        let context = Context::new(&history);
        let note = Hinter::hint(&names, "SELECT l", 8, &context).unwrap();
        assert_eq!(note.display(), &asked[8..]);
        assert_eq!(note.completion(), Some(&asked[8..]));
        // It goes before the note on the next parameter.
        let line = "SELECT logins FROM logins WHERE bucket >= $1; ";
        let note = Hinter::hint(&names, line, line.len(), &context).unwrap();
        assert_eq!(note.completion(), Some("0"));
    }

    #[test]
    fn nothing_is_suggested_without_a_line_that_starts_the_same() {
        let names = Names { views: Vec::new() };
        let mut history = DefaultHistory::new();
        history.add("\\d").unwrap();
        let context = Context::new(&history);
        // No such line, the whole line, an empty line, the cursor mid-line.
        for (line, cursor) in [("SELECT", 6), ("\\d", 2), ("", 0), ("\\", 0)] {
            assert!(
                Hinter::hint(&names, line, cursor, &context).is_none(),
                "{line}"
            );
        }
    }

    #[test]
    fn the_first_tab_lists_candidates_that_differ() {
        let config = editor_config();
        assert_eq!(config.completion_type(), CompletionType::List);
        assert!(config.completion_show_all_if_ambiguous());
        // rustyline's own default waits for a second Tab.
        assert!(!Config::default().completion_show_all_if_ambiguous());
    }

    /// What the shell wrote, with the URL of the server in it as `URL`.
    fn text(bytes: Vec<u8>, url: &str) -> String {
        String::from_utf8(bytes).unwrap().replace(url, "URL")
    }

    /// What the shell writes for `input`, at a Straum that takes
    /// `request`: its output, its errors, and how many lines failed.
    fn shelled(request: &str, reply: &'static str, input: &str) -> (String, String, usize) {
        let url = answering(request, reply);
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        let failed = piped(&server(&url), input.as_bytes(), &mut output, &mut errors).unwrap();
        (text(output, &url), text(errors, &url), failed)
    }

    /// A table of one row, with the phases Straum names for it.
    const TIMED: &str = "HTTP/1.1 200 OK\r\n\
        server-timing: parse;dur=0.031, answer;dur=2.480, text;dur=0.401, total;dur=2.950\r\n\
        \r\n    1\n";

    /// What `line` prints at a terminal and the error it says, at a
    /// Straum that takes `request`. The time the shell measured differs
    /// from run to run, so it reads `N`.
    fn at_a_terminal(request: &str, reply: &'static str, line: &str) -> (String, String) {
        let url = answering(request, reply);
        match answer(&server(&url), line, Timings::Shown) {
            Outcome::Print(printed) => (steady(&printed), String::new()),
            Outcome::Failed(error) => (String::new(), error.to_string().replace(&url, "URL")),
            Outcome::Leave => (String::new(), String::new()),
        }
    }

    fn steady(printed: &str) -> String {
        let Some((before, rest)) = printed.split_once("Time: ") else {
            return printed.to_owned();
        };
        let (measured, after) = rest.split_once(" ms").unwrap();
        assert!(measured.parse::<f64>().is_ok(), "{printed}");
        format!("{before}Time: N ms{after}")
    }

    #[test]
    fn at_a_terminal_an_answer_ends_in_how_long_it_took() {
        let timed = "    1\n\
            Time: N ms (in Straum: parse 0.031, answer 2.480, text 0.401, total 2.950)\n";
        let ask = "SELECT \"x\" ; 7 paid true";
        let printed = at_a_terminal(POST_ASK, TIMED, ask);
        assert_eq!(printed, (timed.to_owned(), String::new()));
        let printed = at_a_terminal(GET_ROWS, TIMED, "\\rows trips");
        assert_eq!(printed, (timed.to_owned(), String::new()));
        // A Straum that names no phase is timed by the shell alone.
        let printed = at_a_terminal(GET_ROWS, "HTTP/1.1 200 OK\r\n\r\n    1\n", "\\rows trips");
        assert_eq!(printed, ("    1\nTime: N ms\n".to_owned(), String::new()));
    }

    #[test]
    fn only_an_answer_at_a_terminal_is_timed() {
        // Not on a pipe, where a script reads the answer.
        let written = shelled(POST_ASK, TIMED, "SELECT \"x\" ; 7 paid true\n");
        assert_eq!(written, ("    1\n".to_owned(), String::new(), 0));
        let written = shelled(GET_ROWS, TIMED, "\\rows trips\n");
        assert_eq!(written, ("    1\n".to_owned(), String::new(), 0));
        // Not the views, nor a line that fails or leaves.
        let printed = at_a_terminal(GET_VIEWS, TIMED, "\\d");
        assert_eq!(printed, ("    1\n".to_owned(), String::new()));
        let refused = "HTTP/1.1 400 Bad Request\r\nserver-timing: total;dur=1\r\n\r\nno\n";
        let printed = at_a_terminal(POST_ASK, refused, "SELECT \"x\" ; 7 paid true");
        let error = "URL answered \"HTTP/1.1 400 Bad Request\"\nno";
        assert_eq!(printed, (String::new(), error.to_owned()));
        let printed = at_a_terminal(GET_VIEWS, TIMED, "\\q");
        assert_eq!(printed, (String::new(), String::new()));
    }

    /// Takes nothing: its reader has left.
    struct Left;

    impl Write for Left {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_reader_that_left_ends_the_shell_quietly() {
        let mut errors = Vec::new();
        let failed = piped(
            &server("http://127.0.0.1:1"),
            &b"\\?\n\\nope\n"[..],
            Left,
            &mut errors,
        );
        assert_eq!((failed.unwrap(), errors), (0, Vec::new()));
        // Errors that can't be said end it loudly.
        let failed = piped(
            &server("http://127.0.0.1:1"),
            &b"\\nope\n"[..],
            Vec::new(),
            Left,
        );
        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    fn history(state: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
        history_file(state.map(OsString::from), home.map(OsString::from))
    }

    fn kept(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    #[test]
    fn history_is_kept_under_the_xdg_state_directory() {
        assert_eq!(
            history(Some("/state"), Some("/home/jo")),
            Some(kept("/state/straum/history"))
        );
        assert_eq!(
            history(None, Some("/home/jo")),
            Some(kept("/home/jo/.local/state/straum/history"))
        );
        // A relative or empty path is not taken, and no home means no file.
        assert_eq!(
            history(Some("state"), Some("/home/jo")),
            Some(kept("/home/jo/.local/state/straum/history"))
        );
        assert_eq!(history(Some(""), Some("")), None);
        assert_eq!(history(None, Some("home")), None);
        assert_eq!(history(None, None), None);
    }

    #[test]
    fn the_shell_answers_a_line_at_a_time() {
        let written = shelled(
            GET_VIEWS,
            "HTTP/1.1 200 OK\r\n\r\nview trips\n",
            "\n \\d \n",
        );
        assert_eq!(written, ("view trips\n".to_owned(), String::new(), 0));
        let (written, _, _) = shelled(GET_ROWS, "HTTP/1.1 200 OK\r\n\r\ntrips\n", "\\rows trips");
        assert_eq!(written, "trips\n");
        // What follows the semicolon is the parameters.
        let ask = "SELECT \"x\" ; 7 paid   true\n";
        let (written, _, _) = shelled(POST_ASK, "HTTP/1.1 200 OK\r\n\r\n    1\n", ask);
        assert_eq!(written, "    1\n");
    }

    #[test]
    fn an_ask_takes_lines_until_its_semicolon_or_a_blank_line() {
        let whole = [
            "",
            "  ",
            "\\d",
            "\\rows trips",
            "SELECT \"x\"; 7",
            "SELECT \"x\";",
            "SELECT\n\"x\"; 7 paid",
            "SELECT \"x\"\n",
            "SELECT \"x\"\n  ",
            "SELECT \"x\"\r\n\r",
        ];
        for entered in whole {
            assert_eq!(entry(entered), Entry::Whole, "{entered:?}");
            assert!(matches!(validation(entered), ValidationResult::Valid(None)));
        }
        let continued = ["SELECT \"x\"", "SELECT\n\"x\"", "\nSELECT", "SELECT\n\\d"];
        for entered in continued {
            assert_eq!(entry(entered), Entry::Continued, "{entered:?}");
            assert!(matches!(validation(entered), ValidationResult::Incomplete));
        }
    }

    #[test]
    fn an_ask_split_across_lines_is_answered() {
        let request = POST_ASK
            .replacen("SELECT \\\"x\\\"", "SELECT\\n\\\"x\\\"", 1)
            .replacen("content-length: 53", "content-length: 54", 1);
        let reply = "HTTP/1.1 200 OK\r\n\r\n    1\n";
        let ask = "SELECT\n\"x\" ; 7 paid true\n";
        let written = shelled(&request, reply, ask);
        assert_eq!(written, ("    1\n".to_owned(), String::new(), 0));
        // As a terminal hands it on, with no newline after the last line.
        let printed = at_a_terminal(&request, TIMED, "SELECT\n\"x\" ; 7 paid true");
        assert!(printed.0.starts_with("    1\nTime: N ms"), "{printed:?}");
    }

    #[test]
    fn an_unfinished_ask_fails_once_and_the_next_command_runs() {
        // Nothing listens here, so every ask fails.
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        let input = "SELECT trips\nFROM\n\n\\?\nSELECT trips\nFROM";
        let failed = piped(
            &server("http://127.0.0.1:1"),
            input.as_bytes(),
            &mut output,
            &mut errors,
        );
        // The blank line ends the first ask, the end of input the second.
        assert_eq!(failed.unwrap(), 2);
        let errors = String::from_utf8(errors).unwrap();
        assert_eq!(errors.matches("straum: asking").count(), 2, "{errors}");
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("  \\d "), "{output}");
    }

    #[test]
    fn a_command_runs_at_once_without_a_blank_line() {
        let reply = "HTTP/1.1 200 OK\r\n\r\nview trips\n";
        let written = shelled(GET_VIEWS, reply, "\\d\n\\q\nSELECT\n");
        assert_eq!(written, ("view trips\n".to_owned(), String::new(), 0));
    }

    #[test]
    fn completion_reads_the_lines_before_the_cursor() {
        let logins = r#"{"views":[{"name":"logins","time":"bucket","measures":["logins"],
            "keys":[{"name":"bucket","type":"timestamp"}]}]}"#;
        let names = Names {
            views: read_views(logins).unwrap(),
        };
        let history = DefaultHistory::new();
        let context = Context::new(&history);
        let entered = "SELECT logins\nFROM lo";
        let (start, candidates) = names.complete(entered, entered.len(), &context).unwrap();
        assert_eq!((start, candidates), (19, vec!["logins ".to_owned()]));
        // A measure goes after a SELECT on the line before.
        let entered = "SELECT\nlo";
        let (_, candidates) = names.complete(entered, entered.len(), &context).unwrap();
        assert_eq!(candidates, vec!["logins ".to_owned()]);
        // And a view at the start of the line after FROM.
        let entered = "SELECT logins FROM\n";
        let (start, candidates) = names.complete(entered, entered.len(), &context).unwrap();
        assert_eq!((start, candidates), (19, vec!["logins ".to_owned()]));
    }

    #[test]
    fn the_shell_asks_for_the_rows_from_start_and_count() {
        let asked = [
            ("\\rows trips -20", "?start=-20"),
            ("\\rows trips 40 5", "?start=40&count=5"),
        ];
        for (line, query) in asked {
            let request = GET_ROWS.replacen("trips", &format!("trips{query}"), 1);
            let written = shelled(&request, "HTTP/1.1 200 OK\r\n\r\nsome\n", line);
            assert_eq!(written, ("some\n".to_owned(), String::new(), 0), "{line}");
        }
        let (_, errors, failed) = shelled(GET_ROWS, "", "\\rows trips 0 -1\n\\rows trips 1.5\n");
        assert_eq!(failed, 2);
        assert_eq!(
            errors,
            "straum: \\rows takes VIEW [START [COUNT]], whole numbers, not \"-1\"\n\
             straum: \\rows takes VIEW [START [COUNT]], whole numbers, not \"1.5\"\n"
        );
    }

    #[test]
    fn the_shell_asks_for_a_views_spec() {
        let request = GET_ROWS.replacen("trips", "trips/spec", 1);
        let reply = "HTTP/1.1 200 OK\r\n\r\nCREATE VIEW trips AS SELECT 1;\n";
        let written = shelled(&request, reply, "\\d trips\n");
        let spec = "CREATE VIEW trips AS SELECT 1;\n".to_owned();
        assert_eq!(written, (spec, String::new(), 0));
        let (written, errors, failed) = shelled(&request, reply, "\\d trips zones\n\\d a-b\n");
        assert_eq!((written.as_str(), failed), ("", 2));
        assert_eq!(
            errors,
            "straum: no command \"\\\\d trips zones\"; \\? lists them\n\
             straum: \"a-b\" is not a view's name\n"
        );
    }

    #[test]
    fn a_value_in_single_quotes_keeps_its_spaces() {
        let ask = "SELECT \"x\"; 7 'Staten Island' true\n";
        let written = shelled(POST_ASK_SPACED, "HTTP/1.1 200 OK\r\n\r\n    1\n", ask);
        assert_eq!(written, ("    1\n".to_owned(), String::new(), 0));
        // Without the quotes the words are two values, and the ask is
        // another, which this Straum doesn't answer.
        let ask = "SELECT \"x\"; 7 Staten Island true\n";
        let (written, _, failed) = shelled(POST_ASK_SPACED, "HTTP/1.1 200 OK\r\n\r\n    1\n", ask);
        assert_eq!((written.as_str(), failed), ("", 1));
    }

    #[test]
    fn the_shell_goes_on_after_an_error_and_leaves_when_told() {
        let (written, errors, failed) = shelled(
            GET_ROWS,
            "HTTP/1.1 404 Not Found\r\n\r\nno view trips; GET /views lists them\n",
            "\\rows\n\\rows a b\n\\rows a 1 2 3\n\\nope\n\\rows trips\n\\q\n\\d\n",
        );
        assert_eq!((written.as_str(), failed), ("", 5));
        assert_eq!(
            errors,
            "straum: no command \"\\\\rows\"; \\? lists them\n\
             straum: \\rows takes VIEW [START [COUNT]], whole numbers, not \"b\"\n\
             straum: no command \"\\\\rows a 1 2 3\"; \\? lists them\n\
             straum: no command \"\\\\nope\"; \\? lists them\n\
             straum: URL answered \"HTTP/1.1 404 Not Found\"\n\
             no view trips; GET /views lists them\n"
        );
    }

    #[test]
    fn a_query_without_a_semicolon_has_no_parameters() {
        // Nothing listens here, so the ask fails and the shell goes on.
        // The blank line ends the ask.
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        let failed = piped(
            &server("http://127.0.0.1:1"),
            &b"SELECT\n\n\\?\n"[..],
            &mut output,
            &mut errors,
        );
        assert_eq!(failed.unwrap(), 1);
        let refused = String::from_utf8(errors).unwrap();
        let commands = String::from_utf8(output).unwrap();
        let asking = "straum: asking http://127.0.0.1:1 for an answer: ";
        assert!(refused.starts_with(asking), "{refused}");
        assert_eq!(
            commands,
            "  \\d                    every view, as straum views prints them\n\
            \x20 \\d VIEW               the CREATE VIEW statement of VIEW, as the\n\
            \x20                       spec wrote it\n\
            \x20 \\rows VIEW [START [COUNT]]\n\
            \x20                       the rows PostgreSQL would give for VIEW:\n\
            \x20                       COUNT of them, at most and else 1000, from\n\
            \x20                       row START, else 0; a negative START counts\n\
            \x20                       back from the end, so -20 is the last 20\n\
            \x20 QUERY; PARAMETER...   an ask, as straum ask takes it: the query, a\n\
            \x20                       semicolon, then the values of $1, $2 and on;\n\
            \x20                       a value with spaces in single quotes\n\
            \x20 \\?                    these commands\n\
            \x20 \\q                    leave\n"
        );
    }
}

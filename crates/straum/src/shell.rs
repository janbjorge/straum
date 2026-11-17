//! `straum shell`: a prompt at a running Straum. Each line goes to
//! `answer`, which reads it as a command and asks Straum through
//! `client.rs`: `\d` for the views, `\rows VIEW` for a view's rows, or an
//! ask written as `QUERY; PARAMETER...`, where a value in single quotes
//! may hold spaces. At a terminal, `interactive` edits the line with
//! rustyline, Tab completes from the views the Straum serves (see
//! `complete.rs`), and the lines entered are kept between sessions. On a
//! pipe, `piped` reads one line at a time.

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
use rustyline::hint::{Hint, Hinter};
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{CompletionType, Config, Context, Editor};

use crate::client::{ClientError, Output, ask, load_views, parameter_words, rows, views};
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
    Client(ClientError),
}

impl Display for LineError {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            LineError::NoSuchCommand(line) => {
                write!(formatter, "no command {line:?}; \\? lists them")
            }
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

/// What one line asks for.
fn answer(url: &str, line: &str) -> Outcome {
    let words: Vec<&str> = line.split_whitespace().collect();
    let answered = match words[..] {
        [] => Ok(String::new()),
        ["\\q"] => return Outcome::Leave,
        ["\\?"] => Ok(shell_commands()),
        ["\\d"] => views(url, Output::Text),
        ["\\rows", view] => rows(url, view),
        [command, ..] if command.starts_with('\\') => {
            return Outcome::Failed(LineError::NoSuchCommand(line.to_owned()));
        }
        _ => {
            let (query, parameters) = line.split_once(';').unwrap_or((line, ""));
            let parameters: Vec<String> = parameter_words(parameters)
                .into_iter()
                .map(str::to_owned)
                .collect();
            ask(url, query.trim_end(), &parameters, Output::Text)
        }
    };
    match answered {
        Ok(text) => Outcome::Print(text),
        Err(error) => Outcome::Failed(LineError::from(error)),
    }
}

fn shell_commands() -> String {
    let mut text = String::new();
    for line in SHELL_HELP.lines() {
        if line.starts_with("  ") && !line.starts_with("  --") {
            text.push_str(line);
            text.push('\n');
        }
    }
    text
}

/// Answers each line of `input` on `output` and says each failure on
/// `errors`, going on after one as psql does. How many lines failed, so
/// a script can tell.
// `impl`: the binary passes the terminal's locked streams, tests pass
// byte buffers and a writer that always fails.
pub(crate) fn piped(
    url: &str,
    mut input: impl BufRead,
    mut output: impl Write,
    mut errors: impl Write,
) -> io::Result<usize> {
    let mut failed = 0;
    let mut line = String::new();
    while input.read_line(&mut line)? > 0 {
        match answer(url, line.trim()) {
            Outcome::Leave => break,
            Outcome::Print(text) => match output.write_all(text.as_bytes()) {
                // The reader has left, as `head` does after its lines.
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
                written => written?,
            },
            Outcome::Failed(error) => {
                failed += 1;
                writeln!(errors, "straum: {error}")?;
            }
        }
        line.clear();
    }
    Ok(failed)
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
pub(crate) fn interactive(url: &str) -> Result<(), ReadlineError> {
    let mut editor: Editor<Names, DefaultHistory> = Editor::with_config(editor_config())?;
    editor.set_helper(Some(Names { views: Vec::new() }));
    let history = history_file(env::var_os("XDG_STATE_HOME"), env::var_os("HOME"));
    if let Some(file) = &history {
        // There is none before the first session.
        editor.load_history(file).ok();
    }
    println!("Straum at {url}. Tab completes; \\? lists the commands, \\q leaves.");
    let result = prompt_until_leaving(url, &mut editor);
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
    url: &str,
    editor: &mut Editor<Names, DefaultHistory>,
) -> Result<(), ReadlineError> {
    loop {
        // Read again before each line: a view's buckets move on.
        if let (Some(views), Some(names)) = (load_views(url), editor.helper_mut()) {
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
        match answer(url, line.trim()) {
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

/// A hint that the right arrow doesn't take into the line: it describes
/// what to type, it isn't it.
struct Note {
    text: String,
}

impl Hint for Note {
    fn display(&self) -> &str {
        &self.text
    }

    fn completion(&self) -> Option<&str> {
        None
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

    fn hint(&self, line: &str, cursor: usize, _: &Context<'_>) -> Option<Note> {
        let text = hint(&self.views, line, cursor)?;
        Some(Note { text })
    }
}

impl Highlighter for Names {
    // The lifetime is rustyline's: the hint may come back borrowed.
    fn highlight_hint<'hint>(&self, hint: &'hint str) -> Cow<'hint, str> {
        Cow::Owned(format!("{DIM}{hint}{UNDIM}"))
    }
}

impl Validator for Names {}

impl rustyline::Helper for Names {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::complete::read_views;
    use crate::testing::{GET_ROWS, GET_VIEWS, POST_ASK, POST_ASK_SPACED, answering};

    #[test]
    fn rustyline_is_given_the_candidates_and_the_hint() {
        let logins = r#"{"views":[{"name":"logins","measures":["logins"],
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
        assert_eq!(note.display(), "$1: bucket >= timestamp");
        assert_eq!(note.completion(), None);
        assert_eq!(names.highlight_hint("$1"), "\x1b[2m$1\x1b[0m");
        assert!(Hinter::hint(&names, line, 0, &context).is_none());
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
    fn shelled(request: &'static str, reply: &'static str, input: &str) -> (String, String, usize) {
        let url = answering(request, reply);
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        let failed = piped(&url, input.as_bytes(), &mut output, &mut errors).unwrap();
        (text(output, &url), text(errors, &url), failed)
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
            "http://127.0.0.1:1",
            &b"\\?\n\\nope\n"[..],
            Left,
            &mut errors,
        );
        assert_eq!((failed.unwrap(), errors), (0, Vec::new()));
        // Errors that can't be said end it loudly.
        let failed = piped("http://127.0.0.1:1", &b"\\nope\n"[..], Vec::new(), Left);
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
            "\\rows\n\\rows a b\n\\nope\n\\rows trips\n\\q\n\\d\n",
        );
        assert_eq!((written.as_str(), failed), ("", 4));
        assert_eq!(
            errors,
            "straum: no command \"\\\\rows\"; \\? lists them\n\
             straum: no command \"\\\\rows a b\"; \\? lists them\n\
             straum: no command \"\\\\nope\"; \\? lists them\n\
             straum: URL answered \"HTTP/1.1 404 Not Found\"\n\
             no view trips; GET /views lists them\n"
        );
    }

    #[test]
    fn a_query_without_a_semicolon_has_no_parameters() {
        // Nothing listens here, so the ask fails and the shell goes on.
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        let failed = piped(
            "http://127.0.0.1:1",
            &b"SELECT\n\\?\n"[..],
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
            \x20 \\rows VIEW            the rows PostgreSQL would give for VIEW, the\n\
            \x20                       first 1000\n\
            \x20 QUERY; PARAMETER...   an ask, as straum ask takes it: the query, a\n\
            \x20                       semicolon, then the values of $1, $2 and on;\n\
            \x20                       a value with spaces in single quotes\n\
            \x20 \\?                    these commands\n\
            \x20 \\q                    leave\n"
        );
    }
}

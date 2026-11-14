//! `straum shell`: a prompt at a running Straum. A line is a command: `\d`
//! for the views, `\rows VIEW` for a view's rows, or an ask written as
//! `QUERY; PARAMETER...`. At a terminal the line is edited with rustyline,
//! Tab completes from the views the Straum serves (see `complete.rs`), and
//! the lines entered are kept between sessions. On a pipe it reads lines.

use std::borrow::Cow;
use std::env;
use std::ffi::OsString;
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

use crate::SHELL_HELP;
use crate::client::{ask, load_views, rows, views};
use crate::complete::{Completion, View, complete, hint};

const PROMPT: &str = "straum> ";

/// What one line asks for; `None` to leave.
fn answer(url: &str, line: &str) -> Option<Result<String, String>> {
    let words: Vec<&str> = line.split_whitespace().collect();
    Some(match words[..] {
        [] => Ok(String::new()),
        ["\\q"] => return None,
        ["\\?"] => {
            let commands = SHELL_HELP.lines().filter(|line| line.starts_with("  "));
            let commands = commands.filter(|line| !line.starts_with("  --"));
            Ok(commands.map(|line| format!("{line}\n")).collect())
        }
        ["\\d"] => views(url),
        ["\\rows", view] => rows(url, view),
        [command, ..] if command.starts_with('\\') => {
            Err(format!("no command {line:?}; \\? lists them"))
        }
        _ => {
            let (query, parameters) = line.split_once(';').unwrap_or((line, ""));
            let parameters: Vec<String> =
                parameters.split_whitespace().map(str::to_owned).collect();
            ask(url, query.trim_end(), &parameters)
        }
    })
}

/// Answers each line of `input` on `output` and says each failure on
/// `errors`, going on after one as psql does. How many lines failed, so
/// a script can tell.
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
            None => break,
            Some(Ok(text)) => match output.write_all(text.as_bytes()) {
                // The reader has left, as `head` does after its lines.
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => break,
                written => written?,
            },
            Some(Err(error)) => {
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
    // The XDG rules take only an absolute path.
    let absolute = |path: OsString| Some(PathBuf::from(path)).filter(|path| path.is_absolute());
    let state = (state_home.and_then(absolute))
        .or_else(|| Some(home.and_then(absolute)?.join(".local").join("state")))?;
    Some(state.join("straum").join("history"))
}

/// The prompt at a terminal, with line editing, history and completion.
pub(crate) fn interactive(url: &str) -> Result<(), String> {
    let failed = |e: ReadlineError| format!("reading the terminal: {e}");
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .build();
    let mut editor: Editor<Names, DefaultHistory> = Editor::with_config(config).map_err(failed)?;
    editor.set_helper(Some(Names { views: Vec::new() }));
    let history = history_file(env::var_os("XDG_STATE_HOME"), env::var_os("HOME"));
    if let Some(file) = &history {
        // There is none before the first session.
        editor.load_history(file).ok();
    }
    println!("Straum at {url}. Tab completes; \\? lists the commands, \\q leaves.");
    let outcome = loop {
        // Read again before each line: a view's buckets move on.
        if let (Some(views), Some(names)) = (load_views(url), editor.helper_mut()) {
            names.views = views;
        }
        let line = match editor.readline(PROMPT) {
            Ok(line) => line,
            // Ctrl-C drops the line, as in psql.
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break Ok(()),
            Err(e) => break Err(failed(e)),
        };
        editor.add_history_entry(line.trim()).map_err(failed)?;
        match answer(url, line.trim()) {
            None => break Ok(()),
            Some(Ok(text)) => print!("{text}"),
            Some(Err(error)) => eprintln!("straum: {error}"),
        }
    };
    if let Some(file) = &history {
        // A history that can't be kept, as on a read-only disk, is lost
        // and nothing else.
        file.parent().map(fs::create_dir_all);
        editor.save_history(file).ok();
    }
    outcome
}

/// What rustyline asks while a line is edited.
struct Names {
    views: Vec<View>,
}

/// A hint that the right arrow doesn't take into the line: it describes
/// what to type, it isn't it.
struct Note(String);

impl Hint for Note {
    fn display(&self) -> &str {
        &self.0
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
        pos: usize,
        _: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let Completion { start, candidates } = complete(&self.views, line, pos);
        Ok((start, candidates))
    }
}

impl Hinter for Names {
    type Hint = Note;

    fn hint(&self, line: &str, pos: usize, _: &Context<'_>) -> Option<Note> {
        hint(&self.views, line, pos).map(Note)
    }
}

impl Highlighter for Names {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("\x1b[2m{hint}\x1b[0m"))
    }
}

impl Validator for Names {}

impl rustyline::Helper for Names {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::tests::{GET_ROWS, POST_ASK, answering};
    use crate::complete::read_views;

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
        assert_eq!((start, candidates), (6, vec!["logins".to_owned()]));
        let line = "SELECT logins FROM logins WHERE bucket >= $1; ";
        let note = Hinter::hint(&names, line, line.len(), &context).unwrap();
        assert_eq!(note.display(), "$1: bucket >= timestamp");
        assert_eq!(note.completion(), None);
        assert_eq!(names.highlight_hint("$1"), "\x1b[2m$1\x1b[0m");
    }

    /// What the shell writes for `input`, at a Straum that takes
    /// `request`: its output, its errors, and how many lines failed.
    fn shelled(request: &'static str, reply: &'static str, input: &str) -> (String, String, usize) {
        let url = answering(request, reply);
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        let failed = piped(&url, input.as_bytes(), &mut output, &mut errors).unwrap();
        let text = |bytes| String::from_utf8(bytes).unwrap().replace(&url, "URL");
        (text(output), text(errors), failed)
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

    #[test]
    fn history_is_kept_under_the_xdg_state_directory() {
        let file = |state: Option<&str>, home: Option<&str>| {
            history_file(state.map(OsString::from), home.map(OsString::from))
        };
        let kept = |path: &str| Some(PathBuf::from(path));
        assert_eq!(
            file(Some("/state"), Some("/home/jo")),
            kept("/state/straum/history")
        );
        assert_eq!(
            file(None, Some("/home/jo")),
            kept("/home/jo/.local/state/straum/history")
        );
        // A relative or empty path is not taken, and no home means no file.
        assert_eq!(
            file(Some("state"), Some("/home/jo")),
            kept("/home/jo/.local/state/straum/history")
        );
        assert_eq!(file(Some(""), Some("")), None);
        assert_eq!(file(None, None), None);
    }

    #[test]
    fn the_shell_answers_a_line_at_a_time() {
        let views = "GET /views HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: text/plain\r\n\r\n";
        let written = shelled(views, "HTTP/1.1 200 OK\r\n\r\nview trips\n", "\n \\d \n");
        assert_eq!(written, ("view trips\n".to_owned(), String::new(), 0));
        let (written, _, _) = shelled(GET_ROWS, "HTTP/1.1 200 OK\r\n\r\ntrips\n", "\\rows trips");
        assert_eq!(written, "trips\n");
        // What follows the semicolon is the parameters.
        let ask = "SELECT \"x\" ; 7 paid   true\n";
        let (written, _, _) = shelled(POST_ASK, "HTTP/1.1 200 OK\r\n\r\n    1\n", ask);
        assert_eq!(written, "    1\n");
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
        // A query with no semicolon has no parameters; nothing listens here.
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
            \x20                       semicolon, then the values of $1, $2 and on\n\
            \x20 \\?                    these commands\n\
            \x20 \\q                    leave\n"
        );
    }
}

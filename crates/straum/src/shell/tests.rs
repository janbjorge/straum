//! The tests of `shell.rs`: lines answered on a pipe and at a terminal
//! against a fake Straum, and what rustyline is given to complete, hint
//! and keep.

use super::*;
use crate::testing::{GET_ROWS, GET_VIEWS, POST_ASK, POST_ASK_SPACED, answering, server};

/// The views of a Straum that serves `logins` alone, as `GET /views`
/// writes them in JSON.
const LOGINS: &str = r#"{"views":[{"name":"logins","time":"bucket","measures":["logins"],
    "keys":[{"name":"bucket","type":"timestamp"}]}]}"#;

#[test]
fn rustyline_is_given_the_candidates_and_the_hint() {
    let names = Names {
        views: read_views(LOGINS).unwrap(),
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
    let names = Names {
        views: read_views(LOGINS).unwrap(),
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
    assert!(
        matches!(&failed, Err(PipeError::Writing(error)) if error.kind() == io::ErrorKind::BrokenPipe),
        "{failed:?}"
    );
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

/// The lines a history file holds, read as the next session reads it.
fn saved(file: &Path) -> Vec<String> {
    let mut loaded = DefaultHistory::new();
    loaded.load(file).unwrap();
    let mut lines = Vec::new();
    for line in loaded.iter() {
        lines.push(line.clone());
    }
    lines
}

#[test]
fn each_line_entered_is_saved_at_once() {
    let file = std::env::temp_dir().join(format!("straum-history-{}", std::process::id()));
    fs::remove_file(&file).ok();
    let mut history = DefaultHistory::new();
    keep_entered(&mut history, "\\d", Some(&file)).unwrap();
    assert_eq!(saved(&file), ["\\d"]);
    keep_entered(&mut history, "SELECT trips FROM trips;", Some(&file)).unwrap();
    assert_eq!(saved(&file), ["\\d", "SELECT trips FROM trips;"]);
    fs::remove_file(&file).unwrap();
    // With no file the line is kept for this session alone, and a
    // file that can't be written loses the line and nothing else.
    keep_entered(&mut history, "\\q", None).unwrap();
    let unwritable = file.join("history");
    keep_entered(&mut history, "\\rows trips", Some(&unwritable)).unwrap();
    assert!(!file.exists());
    assert_eq!(history.len(), 4);
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
    let names = Names {
        views: read_views(LOGINS).unwrap(),
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

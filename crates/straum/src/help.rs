//! The help text: what `straum help` and each command's `--help` print.
//! `command_line.rs` picks one by the command's `Name`, `main.rs` prints
//! it, and `shell.rs` takes its list of commands from `SHELL_HELP`.

/// What `straum help` prints: every command, a line each.
pub(crate) const HELP: &str = "\
Straum follows tables in PostgreSQL and answers asks over HTTP.

usage: straum <command> [flags]

  run     load the spec's tables, follow their changes, answer asks
  plan    print the SQL that sets PostgreSQL up for the spec
  status  print what a running Straum has done
  views   print the views a running Straum holds, and how much of each
  ask     ask a running Straum and print the answer as a table
  shell   a prompt at a running Straum: its views, their rows, asks
  version print Straum's version
  help    print this; straum help <command> prints its flags

exit status: 0 done, 1 failed, 2 a command line misused, 3 Straum not
ready yet, so ask again later

Guide: https://janbjorge.github.io/straum/
";

/// What `straum run --help` prints.
pub(crate) const RUN_HELP: &str = "\
usage: straum run FILE [flags]

Loads every table FILE names, follows their changes and answers asks over
HTTP. SIGTERM or SIGINT stops it. FILE is the spec.

  --listen ADDRESS             where to serve HTTP (0.0.0.0:8080)
  --wal logical|physical       logical decoding through the publication, or
                               the physical WAL wal_level replica writes
                               (logical)
  --publication NAME           the publication straum plan made (straum)
  --dsn-file FILE              a file holding the DSN; else STRAUM_DSN
  --max-lag SECONDS            lag before /readyz turns off (30)
  --max-retained-wal BYTES     WAL retained for the slot before Straum starts
                               over (4Gi; takes Ki, Mi, Gi or Ti)
  --max-transaction-rows ROWS  rows one transaction may change before
                               Straum starts over (100000)
  --max-page-memory BYTES      with --wal physical: rows held from the WAL
                               before Straum starts over (1Gi)

Guide: https://janbjorge.github.io/straum/cli.html#straum-run
";

/// What `straum plan --help` prints.
pub(crate) const PLAN_HELP: &str = "\
usage: straum plan FILE [flags]

Prints the SQL that sets PostgreSQL up for FILE, the spec: logical
decoding, the role, its grants and the publication. With --wal physical
only the role and its grants. With a DSN the role is its user and gets
its password, as a SCRAM-SHA-256 verifier.

  --wal logical|physical
                      the WAL straum run reads (logical)
  --publication NAME  the publication to make (straum)
  --role NAME         the role straum run connects as (the DSN's user,
                      else straum)
  --dsn-file FILE     a file holding the DSN; else STRAUM_DSN, if set

Guide: https://janbjorge.github.io/straum/postgresql.html#2-run-the-setup-sql
";

/// What `straum status --help` prints.
pub(crate) const STATUS_HELP: &str = "\
usage: straum status [flags]

Prints what the Straum at URL has done: ready or why not, how far behind
PostgreSQL, the WAL retained for it, its starts and its failures. Exits
with 3 while Straum is not ready. In a pod:
kubectl exec deployment/straum -- /straum status

  --url URL  where Straum serves HTTP (STRAUM_URL, else http://127.0.0.1:8080)
  --json     print the same facts as one JSON object, null where unknown

Guide: https://janbjorge.github.io/straum/cli.html#straum-status
";

/// What `straum views --help` prints.
pub(crate) const VIEWS_HELP: &str = "\
usage: straum views [flags]

Prints every view of the Straum at URL: its bucket widths, the cells and
the range of buckets it holds, its keys with their types and its measures.
Then each dim, its rows and the view keys that read it.

  --url URL  where Straum serves HTTP (STRAUM_URL, else http://127.0.0.1:8080)
  --json     print each view's keys with their types, its measures and the
             range of buckets it holds, as JSON

Guide: https://janbjorge.github.io/straum/cli.html#straum-views
";

/// What `straum ask --help` prints.
pub(crate) const ASK_HELP: &str = "\
usage: straum ask [flags] QUERY [PARAMETER...]

Sends QUERY to the Straum at URL and prints the answer as a table, or with
--json as JSON. Each
PARAMETER is the value of $1, $2 and on: 7 is an integer, 20.5 a number
exactly as written, true and false are booleans, anything else is a
string, and '7' in quotes is the string 7.

QUERY reads one view by the names straum views prints. Every value is a
parameter. bucket >= $n AND bucket < $n bound the buckets read; both are
needed. A selected key is also in GROUP BY. A measure is its name, never
sum(...) or count(...).

examples:
  straum ask 'SELECT events FROM usage
      WHERE bucket >= $1 AND bucket < $2' \\
      2026-11-13T00:00:00Z 2026-11-14T00:00:00Z
  straum ask 'SELECT region, events, amount FROM usage
      WHERE kind = $1 AND bucket >= $2 AND bucket < $3
      GROUP BY region' \\
      purchase 2026-11-13T00:00:00Z 2026-11-14T00:00:00Z

  --url URL  where Straum serves HTTP (STRAUM_URL, else http://127.0.0.1:8080)
  --json     print the answer as JSON: as_of and a row object per group

Guide: https://janbjorge.github.io/straum/cli.html#straum-ask
";

/// What `straum shell --help` prints; its indented lines are the shell's
/// commands.
pub(crate) const SHELL_HELP: &str = "\
usage: straum shell [flags]

Reads commands for the Straum at URL, a line each, until \\q or the end
of input. At a terminal Tab completes views, keys, measures, conditions
and the bounds of the buckets held, and the right arrow takes the line
entered before that is shown dimmed past the cursor.

  \\d                    every view, as straum views prints them
  \\rows VIEW [START [COUNT]]
                        the rows PostgreSQL would give for VIEW:
                        COUNT of them, at most and else 1000, from
                        row START, else 0; a negative START counts
                        back from the end, so -20 is the last 20
  QUERY; PARAMETER...   an ask, as straum ask takes it: the query, a
                        semicolon, then the values of $1, $2 and on;
                        a value with spaces in single quotes
  \\?                    these commands
  \\q                    leave

  --url URL  where Straum serves HTTP (STRAUM_URL, else http://127.0.0.1:8080)

Guide: https://janbjorge.github.io/straum/cli.html#straum-shell
";

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use straum_domain::{Literal, Spec};

    use super::*;
    use crate::client::parameter;
    use crate::command_line::{Command, Environment, command};

    const README: &str = include_str!("../../../README.md");

    /// The spec the README shows, which the examples ask about.
    fn readme_spec() -> Spec {
        let start = README.find("CREATE VIEW usage").unwrap();
        let length = README[start..].find("```").unwrap();
        straum_notation::parse(&README[start..start + length]).unwrap()
    }

    /// The command lines under `examples:`, split into words as a shell
    /// would: single quotes hold spaces and newlines, a backslash ends a
    /// line without ending the command.
    fn examples(help: &str) -> Vec<Vec<String>> {
        let start = help.find("examples:\n").unwrap() + "examples:\n".len();
        let length = help[start..].find("\n\n").unwrap();
        let mut commands = Vec::new();
        let mut words = Vec::new();
        let mut word = String::new();
        let mut quoted = false;
        let mut characters = help[start..start + length].chars().peekable();
        while let Some(character) = characters.next() {
            match character {
                '\'' => quoted = !quoted,
                '\\' if !quoted && characters.peek() == Some(&'\n') => {
                    characters.next();
                }
                ' ' | '\n' if !quoted => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                    if character == '\n' {
                        commands.push(std::mem::take(&mut words));
                    }
                }
                _ => word.push(character),
            }
        }
        words.push(word);
        commands.push(words);
        commands
    }

    /// A parameter as `straum ask` sends it, read back as the server does.
    /// The examples pass only text: bounds and keys' values.
    fn literal(text: &str) -> Literal {
        let Value::String(text) = serde_json::from_str(&parameter(text)).unwrap() else {
            panic!("an example passes {text}, which is no text");
        };
        Literal::Text(text)
    }

    /// Why `words` is no ask the README's spec answers, if it isn't.
    fn check_example(spec: &Spec, words: &[String]) -> Result<(), String> {
        let Some((first, rest)) = words.split_first() else {
            return Err("no command".to_owned());
        };
        if first != "straum" {
            return Err(format!("not a straum command: {words:?}"));
        }
        let Ok(Command::Ask {
            query, parameters, ..
        }) = command(rest, &Environment::default())
        else {
            return Err(format!("not an ask: {words:?}"));
        };
        let mut literals = Vec::new();
        for text in &parameters {
            literals.push(literal(text));
        }
        let ask = match straum_notation::parse_ask(&query, &literals) {
            Ok(ask) => ask,
            Err(errors) => return Err(format!("{query}: {errors:?}")),
        };
        match spec.check_ask(&ask) {
            Ok(_) => Ok(()),
            Err(errors) => Err(format!("{query}: {errors:?}")),
        }
    }

    #[test]
    fn every_ask_example_is_an_ask_the_readme_spec_answers() {
        let spec = readme_spec();
        let commands = examples(ASK_HELP);
        assert_eq!(commands.len(), 2);
        for words in commands {
            assert_eq!(check_example(&spec, &words), Ok(()));
        }
    }

    #[test]
    fn an_example_that_drifts_fails() {
        let spec = readme_spec();
        let drifted = "examples:\n  straum ask 'SELECT region, events FROM usage\n      \
            WHERE bucket >= $1 AND bucket < $2' \\\n      1970-01-01T00:00:00Z 1970-01-02T00:00:00Z\n\n";
        for words in examples(drifted) {
            let error = check_example(&spec, &words).unwrap_err();
            assert!(error.contains("KeyNotGrouped"), "{error}");
        }
        let not_straum = ["ls".to_owned()];
        assert!(check_example(&spec, &not_straum).is_err());
        let not_ask = ["straum".to_owned(), "status".to_owned()];
        assert!(check_example(&spec, &not_ask).is_err());
    }
}

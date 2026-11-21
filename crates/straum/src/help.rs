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
usage: straum run FILE --wal logical|physical [flags]

Loads every table FILE names, follows their changes and answers asks over
HTTP. SIGTERM or SIGINT stops it. FILE is the spec. Any failure to load
or follow ends it with exit 1, for whatever runs it to start it again.

  --listen ADDRESS             where to serve HTTP (127.0.0.1:8080)
  --wal logical|physical       logical decoding through the publication, or
                               the physical WAL wal_level replica writes;
                               required
  --publication NAME           the publication straum plan made (straum)
  --dsn-file FILE              a file holding the DSN; else STRAUM_DSN,
                               else PGHOST and the other libpq variables
  --max-lag SECONDS            lag before /readyz turns off (30)
  --max-retained-wal BYTES     WAL retained for the slot before Straum
                               stops (4Gi; takes Ki, Mi, Gi or Ti)
  --max-transaction-rows ROWS  rows one transaction may change before
                               Straum stops (100000)
  --max-page-memory BYTES      with --wal physical: rows held from the WAL
                               before Straum stops (1Gi)
  --max-memory BYTES           resident memory before Straum stops (off;
                               takes Ki, Mi, Gi or Ti; Linux only)

Guide: https://janbjorge.github.io/straum/cli.html#straum-run
";

/// What `straum plan --help` prints.
pub(crate) const PLAN_HELP: &str = "\
usage: straum plan FILE --wal logical|physical [flags]
       straum plan FILE... --copy DIR
       straum plan FILE... --tables

Prints the SQL that sets PostgreSQL up for FILE, the spec. With --wal
logical: logical decoding, the role, its grants and the publication.
With --wal physical: the role and its grants. With a DSN the role is its user and gets
its password, as a SCRAM-SHA-256 verifier. --out writes the SQL to a
file instead of stdout.

--copy writes two psql scripts into DIR that copy the tables of one or
more specs: export.sql runs on the source and writes their schema and
one CSV per table, with only the columns the specs read, in one
read-only snapshot; load.sql runs on the destination and loads them.
psql connects, so Straum reads no database.

  --wal logical|physical
                      the WAL straum run reads; required for the
                      setup SQL
  --publication NAME  the publication to make (straum)
  --role NAME         the role straum run connects as (the DSN's user,
                      else straum)
  --dsn-file FILE     a file holding the DSN; else STRAUM_DSN, else
                      PGHOST and the other libpq variables
  --copy DIR          write export.sql and load.sql into DIR instead
  --tables            print the specs' tables instead, one per line, as
                      `pg_dump -t` takes them
  --out FILE          write the SQL (or the tables) to FILE

Guide: https://janbjorge.github.io/straum/postgresql.html#1-run-the-setup-sql
";

/// What `straum status --help` prints.
pub(crate) const STATUS_HELP: &str = "\
usage: straum status [flags]

Prints what the Straum at URL has done: ready or why not, how far behind
PostgreSQL and the WAL retained for it. Exits with 3 while Straum is not
ready. In a pod:
kubectl exec deployment/straum -- /straum status

  --url URL  where Straum serves HTTP (STRAUM_URL, else http://127.0.0.1:8080)
  --timeout SECONDS
             how long connect, send and the reply may take (10)
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
  --timeout SECONDS
             how long connect, send and the reply may take (10)
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
parameter. The view's time, by the name the view gives it, bounds the
buckets read: bucket >= $n AND bucket < $n for the view usage below. A
bound left out leaves its end open, so with no WHERE an ask reads every
bucket held. A selected key is also in GROUP BY. A measure is its name, never
sum(...) or count(...). Not equal in HAVING is written <>; != is refused.

examples:
  straum ask 'SELECT events FROM usage
      WHERE bucket >= $1 AND bucket < $2' \\
      2026-11-13T00:00:00Z 2026-11-14T00:00:00Z
  straum ask 'SELECT region, events, amount FROM usage
      WHERE kind = $1 AND bucket >= $2 AND bucket < $3
      GROUP BY region' \\
      purchase 2026-11-13T00:00:00Z 2026-11-14T00:00:00Z

  --url URL  where Straum serves HTTP (STRAUM_URL, else http://127.0.0.1:8080)
  --timeout SECONDS
             how long connect, send and the reply may take (10)
  --json     print the answer as JSON: as_of and a row object per group

Guide: https://janbjorge.github.io/straum/cli.html#straum-ask
";

/// What `straum shell --help` prints; its indented lines are the shell's
/// commands.
pub(crate) const SHELL_HELP: &str = "\
usage: straum shell [flags]

Reads commands for the Straum at URL until \\q or the end of input. A
command that starts with \\ is one line. An ask may take several: it
ends at the line that holds its semicolon, so its parameters go on that
line, or at a blank line or the end of input, with no parameters.
At a terminal Tab completes views, keys, measures, conditions
and the bounds of the buckets held, and the right arrow takes the line
entered before that is shown dimmed past the cursor. An ask and \\rows
then end in how long they took: the request, then each part of it in
Straum, in milliseconds.

  \\d                    every view, as straum views prints them
  \\d VIEW               the CREATE VIEW statement of VIEW, as the
                        spec wrote it
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
  --timeout SECONDS
             how long connect, send and the reply may take (10)

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

    /// Where the guide's pages link from.
    const GUIDE: &str = "https://janbjorge.github.io/straum/";

    /// A heading's anchor as mdBook makes it: lower case, whitespace to
    /// `-`, and every other character but letters, digits, `-` and `_`
    /// dropped.
    fn anchor(heading: &str) -> String {
        let mut slug = String::new();
        for character in heading.chars() {
            if character.is_alphanumeric() || character == '-' || character == '_' {
                slug.extend(character.to_lowercase());
            } else if character.is_whitespace() {
                slug.push('-');
            }
        }
        slug
    }

    /// The anchors of a page's headings, outside its fenced code.
    fn anchors(markdown: &str) -> Vec<String> {
        let mut anchors = Vec::new();
        let mut fenced = false;
        for line in markdown.lines() {
            if line.starts_with("```") {
                fenced = !fenced;
                continue;
            }
            if fenced || !line.starts_with('#') {
                continue;
            }
            anchors.push(anchor(line.trim_start_matches('#').trim()));
        }
        anchors
    }

    /// Each link to the guide in `help` whose page or anchor isn't in
    /// `docs/src`.
    fn broken_links(help: &str) -> Vec<String> {
        let docs = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/src/");
        let mut broken = Vec::new();
        for word in help.split_whitespace() {
            let Some(link) = word.strip_prefix(GUIDE) else {
                continue;
            };
            if link.is_empty() {
                // The guide itself, which mdBook opens at its first page.
                continue;
            }
            let (page, anchor) = link.split_once('#').unwrap_or((link, ""));
            let Some(name) = page.strip_suffix(".html") else {
                broken.push(link.to_owned());
                continue;
            };
            let Ok(markdown) = std::fs::read_to_string(format!("{docs}{name}.md")) else {
                broken.push(link.to_owned());
                continue;
            };
            if !anchor.is_empty() && !anchors(&markdown).contains(&anchor.to_owned()) {
                broken.push(link.to_owned());
            }
        }
        broken
    }

    #[test]
    fn every_link_to_the_guide_finds_its_heading() {
        let helps = [
            HELP,
            RUN_HELP,
            PLAN_HELP,
            STATUS_HELP,
            VIEWS_HELP,
            ASK_HELP,
            SHELL_HELP,
        ];
        for help in helps {
            assert_eq!(broken_links(help), Vec::<String>::new());
        }
    }

    #[test]
    fn a_link_to_no_page_or_heading_is_broken() {
        assert_eq!(anchor("1. Run the setup SQL"), "1-run-the-setup-sql");
        assert_eq!(anchor("`straum run`"), "straum-run");
        let fenced = "# Top\n```sh\n# not a heading\n```\n## Next one\n";
        assert_eq!(anchors(fenced), ["top", "next-one"]);
        let links = format!(
            "{GUIDE}cli.html#straum-nope {GUIDE}nowhere.html {GUIDE}cli.md {GUIDE}cli.html"
        );
        assert_eq!(
            broken_links(&links),
            ["cli.html#straum-nope", "nowhere.html", "cli.md"]
        );
    }
}

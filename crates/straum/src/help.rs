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
PostgreSQL, the WAL retained for it, its starts and its failures. In a pod:
kubectl exec deployment/straum -- /straum status

  --url URL  where Straum serves HTTP (http://127.0.0.1:8080)

Guide: https://janbjorge.github.io/straum/cli.html#straum-status
";

/// What `straum views --help` prints.
pub(crate) const VIEWS_HELP: &str = "\
usage: straum views [flags]

Prints every view of the Straum at URL: its bucket widths, the cells and
the range of buckets it holds, its keys with their types and its measures.
Then each dim and its rows.

  --url URL  where Straum serves HTTP (http://127.0.0.1:8080)

Guide: https://janbjorge.github.io/straum/cli.html#straum-views
";

/// What `straum ask --help` prints.
pub(crate) const ASK_HELP: &str = "\
usage: straum ask [flags] QUERY [PARAMETER...]

Sends QUERY to the Straum at URL and prints the answer as a table. Each
PARAMETER is the value of $1, $2 and on: 7 is an integer, 20.5 a number
exactly as written, true and false are booleans, anything else is a
string, and '7' in quotes is the string 7.

  --url URL  where Straum serves HTTP (http://127.0.0.1:8080)

Guide: https://janbjorge.github.io/straum/cli.html#straum-ask
";

/// What `straum shell --help` prints; its indented lines are the shell's
/// commands.
pub(crate) const SHELL_HELP: &str = "\
usage: straum shell [flags]

Reads commands for the Straum at URL, a line each, until \\q or the end
of input. At a terminal Tab completes views, keys, measures, conditions
and the bounds of the buckets held.

  \\d                    every view, as straum views prints them
  \\rows VIEW            the rows PostgreSQL would give for VIEW, the
                        first 1000
  QUERY; PARAMETER...   an ask, as straum ask takes it: the query, a
                        semicolon, then the values of $1, $2 and on
  \\?                    these commands
  \\q                    leave

  --url URL  where Straum serves HTTP (http://127.0.0.1:8080)

Guide: https://janbjorge.github.io/straum/cli.html#straum-shell
";

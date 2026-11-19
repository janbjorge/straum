//! The spec examples in the README and the guide parse, so an example can't
//! drift from the grammar without a test saying which one. Each page's
//! `sql` fences that hold a whole spec are cut out and parsed. The spec
//! files the benchmarks and the demo run parse whole. The asks of the
//! `usage` view are cut out the same way, parsed, and checked against the
//! `usage` view of the Spec page.

use std::fmt;

use straum_domain::{Literal, Spec};
use straum_notation::describe_ask_error;

const DOCS: [Page; 5] = [
    Page {
        name: "README.md",
        markdown: include_str!("../../../README.md"),
    },
    Page {
        name: "docs/src/spec.md",
        markdown: include_str!("../../../docs/src/spec.md"),
    },
    Page {
        name: "docs/src/quick-start.md",
        markdown: include_str!("../../../docs/src/quick-start.md"),
    },
    Page {
        name: "docs/src/introduction.md",
        markdown: include_str!("../../../docs/src/introduction.md"),
    },
    Page {
        name: "docs/src/performance.md",
        markdown: include_str!("../../../docs/src/performance.md"),
    },
];

/// The spec files outside the tests, which nothing else parses before a
/// benchmark or the demo runs them.
const SPEC_FILES: [Page; 2] = [
    Page {
        name: "bench/trips.sql",
        markdown: include_str!("../../../bench/trips.sql"),
    },
    Page {
        name: "bench/demo.sql",
        markdown: include_str!("../../../bench/demo.sql"),
    },
];

/// The pages whose `sql` fences ask the `usage` view of the Spec page.
const ASK_PAGES: [Page; 2] = [
    Page {
        name: "docs/src/asks.md",
        markdown: include_str!("../../../docs/src/asks.md"),
    },
    Page {
        name: "docs/src/how-it-works.md",
        markdown: include_str!("../../../docs/src/how-it-works.md"),
    },
];

/// How an example asking the `usage` view names it; other `sql` fences
/// on those pages are PostgreSQL's own SQL.
const ASKS_USAGE: &str = "FROM usage";

/// How the Spec page's `usage` example starts.
const USAGE_VIEW: &str = "CREATE VIEW usage AS";

/// A page of the docs, by its path in the repository.
struct Page {
    name: &'static str,
    markdown: &'static str,
}

/// A spec in a `sql` fence, and the line the fence opens on.
#[derive(Debug, PartialEq)]
struct Example {
    line: usize,
    text: String,
}

/// An example that failed to parse, named by file and fence line.
#[derive(Debug)]
struct ExampleError {
    name: String,
    line: usize,
    error: straum_notation::Error,
}

impl fmt::Display for ExampleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}: {}", self.name, self.line, self.error)
    }
}

/// An open `sql` fence: the line it opens on and the lines so far.
// Holds lines of the page, borrowed until the fence closes.
struct Fence<'page> {
    line: usize,
    lines: Vec<&'page str>,
}

/// The `sql` fences that hold a whole spec, each with the line its fence
/// opens on. Fragments, such as a lone `date_bin` call, are skipped.
fn specs(markdown: &str) -> Vec<Example> {
    let mut specs = Vec::new();
    for example in fences(markdown) {
        if example.text.trim_start().starts_with("CREATE VIEW") {
            specs.push(example);
        }
    }
    specs
}

/// The `sql` fences that ask the `usage` view, each with the line its
/// fence opens on.
fn asks(markdown: &str) -> Vec<Example> {
    let mut asks = Vec::new();
    for example in fences(markdown) {
        if example.text.contains(ASKS_USAGE) {
            asks.push(example);
        }
    }
    asks
}

/// Every `sql` fence of `markdown`, with the line it opens on.
fn fences(markdown: &str) -> Vec<Example> {
    let mut examples = Vec::new();
    let mut fence: Option<Fence> = None;
    for (index, line) in markdown.lines().enumerate() {
        let Some(open) = &mut fence else {
            if line.trim_end() == "```sql" {
                fence = Some(Fence {
                    line: index + 1,
                    lines: Vec::new(),
                });
            }
            continue;
        };
        if line.trim_end() != "```" {
            open.lines.push(line);
            continue;
        }
        examples.push(Example {
            line: open.line,
            text: open.lines.join("\n"),
        });
        fence = None;
    }
    examples
}

/// A week in seconds: a bound at a whole number of weeks from the epoch
/// is on a bucket edge at every width the `usage` view keeps.
const WEEK_SECONDS: i64 = 7 * 24 * 60 * 60;

/// One parameter for each `$n` of `query`, up to the highest `n`. Each is
/// `n` weeks, so a range's start falls before its end and on an edge.
fn parameters(query: &str) -> Vec<Literal> {
    let mut highest = 0;
    let mut number: Option<i64> = None;
    for character in query.chars() {
        let digit = character.to_digit(10);
        match (number, digit) {
            (Some(so_far), Some(digit)) => number = Some(so_far * 10 + i64::from(digit)),
            (Some(so_far), None) => {
                highest = highest.max(so_far);
                number = None;
            }
            (None, _) => {}
        }
        if character == '$' {
            number = Some(0);
        }
    }
    if let Some(so_far) = number {
        highest = highest.max(so_far);
    }
    let mut parameters = Vec::new();
    for number in 1..=highest {
        parameters.push(Literal::Int(number * WEEK_SECONDS));
    }
    parameters
}

/// The `usage` view of the Spec page, which the ask examples ask.
fn usage_spec() -> Spec {
    let spec_page = include_str!("../../../docs/src/spec.md");
    for example in specs(spec_page) {
        if example.text.trim_start().starts_with(USAGE_VIEW) {
            return straum_notation::parse(&example.text).unwrap();
        }
    }
    panic!("docs/src/spec.md has no {USAGE_VIEW} example");
}

/// An ask example that failed, named by file and fence line, with what
/// was wrong as Straum says it.
#[derive(Debug)]
struct AskExampleError {
    name: String,
    line: usize,
    errors: Vec<String>,
}

impl fmt::Display for AskExampleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}: {}",
            self.name,
            self.line,
            self.errors.join("; ")
        )
    }
}

/// Parses every ask of `usage` in `markdown` and checks it against `spec`:
/// how many, or the first failure named by file and fence line.
fn check_asks(name: &str, markdown: &str, spec: &Spec) -> Result<usize, AskExampleError> {
    let asks = asks(markdown);
    for example in &asks {
        if let Err(errors) = ask_errors(&example.text, spec) {
            return Err(AskExampleError {
                name: name.to_owned(),
                line: example.line,
                errors,
            });
        }
    }
    Ok(asks.len())
}

/// What is wrong with `query` as an ask of `spec`, as Straum says it.
fn ask_errors(query: &str, spec: &Spec) -> Result<(), Vec<String>> {
    let mut said = Vec::new();
    let ask = match straum_notation::parse_ask(query, &parameters(query)) {
        Ok(ask) => ask,
        Err(errors) => {
            for error in &errors {
                said.push(error.to_string());
            }
            return Err(said);
        }
    };
    let Err(errors) = spec.check_ask(&ask) else {
        return Ok(());
    };
    for error in &errors {
        said.push(describe_ask_error(error));
    }
    Err(said)
}

/// Parses every spec in `markdown`: how many, or the first failure named by
/// file and fence line.
fn check(name: &str, markdown: &str) -> Result<usize, ExampleError> {
    let specs = specs(markdown);
    for example in &specs {
        if let Err(error) = straum_notation::parse(&example.text) {
            return Err(ExampleError {
                name: name.to_owned(),
                line: example.line,
                error,
            });
        }
    }
    Ok(specs.len())
}

#[test]
fn every_doc_example_parses() {
    for page in DOCS {
        let parsed = match check(page.name, page.markdown) {
            Ok(parsed) => parsed,
            Err(error) => panic!("{error}"),
        };
        assert!(parsed > 0, "{} has no spec example", page.name);
    }
}

#[test]
fn every_doc_ask_parses_and_fits_the_usage_view() {
    let spec = usage_spec();
    for page in ASK_PAGES {
        let checked = match check_asks(page.name, page.markdown, &spec) {
            Ok(checked) => checked,
            Err(error) => panic!("{error}"),
        };
        assert!(checked > 0, "{} has no ask example", page.name);
    }
}

#[test]
fn a_broken_ask_is_named_by_file_and_line() {
    let markdown = "text\n\n```sql\nSELECT region, nothing FROM usage GROUP BY region\n```\n";
    let error = check_asks("docs/src/example.md", markdown, &usage_spec())
        .unwrap_err()
        .to_string();
    let Some(rest) = error.strip_prefix("docs/src/example.md:3: ") else {
        panic!("{error}");
    };
    assert!(rest.contains("nothing"), "{rest}");
}

#[test]
fn only_asks_of_usage_are_checked() {
    let markdown = "```sql\nSELECT pg_current_wal_lsn();\n```\n\n\
                    ```sql\nSELECT events FROM usage WHERE bucket >= $1 AND bucket < $12\n```\n";
    let found = asks(markdown);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].line, 5);
    assert_eq!(parameters(&found[0].text).len(), 12);
}

#[test]
fn every_spec_file_parses() {
    for file in SPEC_FILES {
        if let Err(error) = straum_notation::parse(file.markdown) {
            panic!("{}: {error}", file.name);
        }
    }
}

#[test]
fn fragments_and_other_fences_are_skipped() {
    let markdown = "```sql\nSELECT 1;\n```\n\n```sh\nCREATE VIEW x\n```\n\n\
                    ```sql\n\nCREATE VIEW v AS SELECT 1;\n```\n";
    assert_eq!(
        specs(markdown),
        [Example {
            line: 9,
            text: "\nCREATE VIEW v AS SELECT 1;".to_owned()
        }]
    );
}

#[test]
fn a_broken_example_is_named_by_file_and_line() {
    let markdown = "text\n\n```sql\nCREATE VIEW broken AS SELECT 1;\n```\n";
    let error = check("docs/src/example.md", markdown)
        .unwrap_err()
        .to_string();
    // The walker's own line numbers count from the fence, so both are kept.
    let Some(rest) = error.strip_prefix("docs/src/example.md:3: ") else {
        panic!("{error}");
    };
    assert!(
        rest.starts_with("line 1: ") && rest.contains("needs exactly one FROM table"),
        "{rest}"
    );
}

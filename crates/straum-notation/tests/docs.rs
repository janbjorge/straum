//! The spec examples in the README and the guide parse, so an example can't
//! drift from the grammar without a test saying which one. Each page's
//! `sql` fences that hold a whole spec are cut out and parsed. The spec
//! files the benchmarks and the demo run parse whole.

use std::fmt;

const DOCS: [Page; 6] = [
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
        name: "docs/src/aggregates.md",
        markdown: include_str!("../../../docs/src/aggregates.md"),
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
        let text = open.lines.join("\n");
        if text.trim_start().starts_with("CREATE VIEW") {
            specs.push(Example {
                line: open.line,
                text,
            });
        }
        fence = None;
    }
    specs
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

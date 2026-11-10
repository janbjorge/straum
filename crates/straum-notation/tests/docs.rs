//! The spec examples in the README and the guide parse, so an example can't
//! drift from the grammar without a test saying which one.

const DOCS: [(&str, &str); 3] = [
    ("README.md", include_str!("../../../README.md")),
    (
        "docs/src/spec.md",
        include_str!("../../../docs/src/spec.md"),
    ),
    (
        "docs/src/quick-start.md",
        include_str!("../../../docs/src/quick-start.md"),
    ),
];

/// The `sql` fences that hold a whole spec, each with the line its fence
/// opens on. Fragments, such as a lone `date_bin` call, are skipped.
fn specs(markdown: &str) -> Vec<(usize, String)> {
    let mut specs = Vec::new();
    let mut fence: Option<(usize, Vec<&str>)> = None;
    for (i, line) in markdown.lines().enumerate() {
        let Some((at, lines)) = &mut fence else {
            if line.trim_end() == "```sql" {
                fence = Some((i + 1, Vec::new()));
            }
            continue;
        };
        if line.trim_end() != "```" {
            lines.push(line);
            continue;
        }
        let text = lines.join("\n");
        if text.trim_start().starts_with("CREATE VIEW") {
            specs.push((*at, text));
        }
        fence = None;
    }
    specs
}

/// Parses every spec in `markdown`: how many, or the first failure named by
/// file and fence line.
fn check(name: &str, markdown: &str) -> Result<usize, String> {
    let specs = specs(markdown);
    for (line, spec) in &specs {
        straum_notation::parse(spec).map_err(|e| format!("{name}:{line}: {e}"))?;
    }
    Ok(specs.len())
}

#[test]
fn every_doc_example_parses() {
    for (name, markdown) in DOCS {
        let parsed = check(name, markdown).unwrap_or_else(|e| panic!("{e}"));
        assert!(parsed > 0, "{name} has no spec example");
    }
}

#[test]
fn fragments_and_other_fences_are_skipped() {
    let markdown = "```sql\nSELECT 1;\n```\n\n```sh\nCREATE VIEW x\n```\n\n\
                    ```sql\n\nCREATE VIEW v AS SELECT 1;\n```\n";
    assert_eq!(
        specs(markdown),
        [(9, "\nCREATE VIEW v AS SELECT 1;".to_owned())]
    );
}

#[test]
fn a_broken_example_is_named_by_file_and_line() {
    let markdown = "text\n\n```sql\nCREATE VIEW broken AS SELECT 1;\n```\n";
    let err = check("docs/src/example.md", markdown).unwrap_err();
    // The walker's own line numbers count from the fence, so both are kept.
    let Some(rest) = err.strip_prefix("docs/src/example.md:3: ") else {
        panic!("{err}");
    };
    assert!(
        rest.starts_with("line 1: ") && rest.contains("needs exactly one FROM table"),
        "{rest}"
    );
}

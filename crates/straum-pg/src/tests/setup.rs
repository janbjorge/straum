//! The setup check: at each start Straum compares PostgreSQL's catalog
//! (its system tables describing tables, columns and types) with the
//! spec. Each problem it finds names what is wrong and what to run;
//! `straum plan` prints the setup that leaves none.

use straum_app::{App, Lsn, Source, SourceError, Writer};

use super::references::*;
use super::support::*;
use crate::catalog::{self};
use crate::dsn::Dsn;
use crate::error::Error;
use crate::source::{Postgres, Replication};
use crate::wire::{Connection, ConnectionKind};

#[test]
fn says_to_set_wal_level_logical() {
    for tag in ["17", "18"] {
        let pg = postgres_at_replica(tag);
        let db = Db::new(&pg, "replica", TABLES);
        let error = db.pg().open(&db.spec(SPEC)).unwrap_err().to_string();
        assert!(
            error.contains("wal_level is replica; Straum needs logical, which takes a restart"),
            "{tag}: {error}"
        );
    }
}

#[test]
fn plan_sets_up_what_straum_needs() {
    let pg = postgres();
    let mut db = Db::new(&pg, "plan", &TABLES.replace("CREATE PUBLICATION", "-- "));
    db.run("ALTER TABLE $S.trips REPLICA IDENTITY DEFAULT");
    let spec = db.spec(SPEC);
    let sql = crate::plan(&spec, "plan", "straum_plan", None, Replication::Logical);
    assert_eq!(
        sql,
        "-- Straum setup: run once, as an owner of the tables; a superuser runs the lines said so.
-- A superuser: wal_level takes a restart. On Azure, set the server parameter instead.
ALTER SYSTEM SET wal_level = logical;
-- A superuser creates the role Straum connects as.
-- REPLICATION is a privilege of that role, not another user.
CREATE ROLE \"straum_plan\" LOGIN REPLICATION;
-- Then give \"straum_plan\" a password: \\password \"straum_plan\"
GRANT USAGE ON SCHEMA \"plan\" TO \"straum_plan\";
GRANT SELECT ON \"plan\".\"zones\", \"plan\".\"trips\", \"plan\".\"logins\" TO \"straum_plan\";
-- If a policy of row-level security hides rows of a table, a superuser runs:
-- ALTER ROLE \"straum_plan\" BYPASSRLS;
CREATE PUBLICATION \"plan\" FOR TABLE \"plan\".\"zones\", \"plan\".\"trips\", \"plan\".\"logins\";
-- fact trips: Straum needs the old row of every update and delete; inserts cost no more.
ALTER TABLE \"plan\".\"trips\" REPLICA IDENTITY FULL;
-- fact logins: Straum needs the old row of every update and delete; inserts cost no more.
ALTER TABLE \"plan\".\"logins\" REPLICA IDENTITY FULL;
-- dim zones: Straum holds its key and the columns the views read, and follows it.
ALTER TABLE \"plan\".\"zones\" REPLICA IDENTITY FULL;
"
    );
    // One statement at a time: ALTER SYSTEM can't run in a transaction.
    for statement in sql.lines() {
        if !statement.starts_with("--") {
            db.run(statement);
        }
    }
    db.run("ALTER ROLE straum_plan PASSWORD 'plan'");
    let dsn = pg.dsn.to_string().replace("postgres@", "straum_plan:plan@");
    let pg = Postgres::new(
        Dsn::parse(&dsn).unwrap(),
        "plan",
        ROW_LIMIT,
        MAX_RETAINED_WAL_BYTES,
        Replication::Logical,
    );
    let mut writer = Writer::new(App::new(spec, LIMITS), pg);
    writer.step().unwrap();
    db.run(&Rng::new(1).trip(1));
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");
}

/// The setup problems an open stopped at; none when it opened.
fn setup_problems(name: &str, opened: Result<Lsn, SourceError>) -> Vec<String> {
    let Err(error) = opened else {
        return Vec::new();
    };
    match error.downcast::<Error>() {
        Ok(error) => match *error {
            Error::Setup(problems) => crate::problem::lines(&problems),
            other => panic!("{name}: Ok({other:?})"),
        },
        Err(other) => panic!("{name}: Err({other:?})"),
    }
}

/// One case of the setup check: a schema `name` holding `tables`, the
/// spec opened over it, and the one problem expected, or none.
struct Case {
    name: &'static str,
    tables: String,
    spec: String,
    // `'static`: every name and problem is a literal written here.
    expected: Option<&'static str>,
}

/// A case of `name`, its `tables` and `spec`, expecting `expected`.
fn case(name: &'static str, tables: String, spec: &str, expected: Option<&'static str>) -> Case {
    Case {
        name,
        tables,
        spec: spec.to_owned(),
        expected,
    }
}

/// Opens each case's spec against its tables and checks the setup
/// problems found.
fn expect_problems(pg: &PostgresContainer, cases: Vec<Case>) {
    for case in cases {
        let name = case.name;
        let db = Db::new(pg, name, &case.tables);
        let found = setup_problems(name, db.pg().open(&db.spec(&case.spec)));
        assert_eq!(found, Vec::from_iter(case.expected), "{name}");
    }
}

#[test]
fn setup_problems_say_what_to_fix() {
    let pg = postgres();
    publication_problems(&pg);
    table_problems(&pg);
    column_type_problems(&pg);
    date_uuid_and_float_problems(&pg);
    padded_problems(&pg);
    aggregate_problems(&pg);
    filter_problems(&pg);
    enum_problems(&pg);
    dim_problems(&pg);
    dim_role_problems(&pg);
    row_security_problems(&pg);
    pair_problems(&pg);
    chain_problems(&pg);
}

/// The publication of the fact alone.
const PUBLISH: &str = "CREATE PUBLICATION $S FOR TABLE $S.f;";
/// An enum of two labels.
const MOOD: &str = "CREATE TYPE $S.mood AS ENUM ('ok', 'bad');";

/// A fact's tables and a spec over it.
struct Fact {
    table: String,
    spec: String,
}

/// A fact with `columns` besides its key and time, and a view selecting
/// `view` from it by the hour.
fn fact(columns: &str, view: &str) -> Fact {
    Fact {
        table: format!(
            "CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, {columns});
             ALTER TABLE $S.f REPLICA IDENTITY FULL;"
        ),
        spec: format!(
            "CREATE VIEW f WITH (buckets = '1 hour') AS
             SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket, {view}
             FROM $S.f AS t GROUP BY 1;"
        ),
    }
}

/// The fact most cases change one thing of.
fn summed_fact() -> Fact {
    fact("x int", "count(*) AS n, sum(t.x) AS total")
}

/// The fact with `columns`, published.
fn published(columns: &str) -> String {
    format!("{} {PUBLISH}", fact(columns, "").table)
}

/// A spec selecting `view` from the fact by the hour, grouped by
/// `group_by`.
fn grouped(view: &str, group_by: &str) -> String {
    fact("", view).spec.replace("GROUP BY 1", group_by)
}

/// A publication that is missing, publishes too little or leaves out a
/// table, a row or a column.
fn publication_problems(pg: &PostgresContainer) {
    let Fact { table, spec } = summed_fact();
    let cases = vec![
        case("ok", format!("{table} {PUBLISH}"), &spec, None),
        case(
            "nopub",
            table.clone(),
            &spec,
            Some("publication nopub does not exist; run the SQL straum plan prints"),
        ),
        case(
            "notrunc",
            format!(
                "{table} CREATE PUBLICATION $S FOR TABLE $S.f WITH (publish = 'insert, update, delete');"
            ),
            &spec,
            Some("publication notrunc must publish insert, update, delete and truncate"),
        ),
        case(
            "missing",
            "CREATE PUBLICATION $S;".into(),
            &spec,
            Some("f: table missing.f does not exist"),
        ),
        case(
            "unpub",
            format!("{table} CREATE TABLE $S.g (); CREATE PUBLICATION $S FOR TABLE $S.g;"),
            &spec,
            Some(
                "f: publication unpub leaves out unpub.f: ALTER PUBLICATION unpub ADD TABLE unpub.f",
            ),
        ),
        case(
            "partitioned",
            "CREATE TABLE $S.f (id int, at timestamptz, x int) PARTITION BY RANGE (id);
             CREATE PUBLICATION $S FOR TABLE $S.f;"
                .into(),
            &spec,
            Some("f: partitioned.f is not a plain table"),
        ),
        case(
            "inherited",
            format!("{table} CREATE TABLE $S.f_2026 () INHERITS ($S.f); {PUBLISH}"),
            &spec,
            Some("f: tables inherit from inherited.f; Straum reads a table without children"),
        ),
        case(
            "child",
            format!(
                "CREATE TABLE $S.parent (id int); {table} ALTER TABLE $S.f INHERIT $S.parent; \
                 {PUBLISH}"
            ),
            &spec,
            None,
        ),
        case(
            "filtered",
            format!("{table} CREATE PUBLICATION $S FOR TABLE $S.f WHERE (x > 0);"),
            &spec,
            Some("f: publication filtered filters the rows of filtered.f; Straum needs every row"),
        ),
        case(
            "listed",
            format!("{table} CREATE PUBLICATION $S FOR TABLE $S.f (id, at);"),
            &spec,
            Some("f: publication listed leaves out column x of listed.f"),
        ),
    ];
    expect_problems(pg, cases);
}

/// A column missing, a fact without a key, and one without the whole old
/// row.
fn table_problems(pg: &PostgresContainer) {
    let Fact { spec, .. } = summed_fact();
    let cases = vec![
        case(
            "nocolumn",
            format!("CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz); {PUBLISH}"),
            &spec,
            Some("f: nocolumn.f has no column x"),
        ),
        case(
            "nokey",
            format!(
                "CREATE TABLE $S.f (at timestamptz, x int); ALTER TABLE $S.f REPLICA IDENTITY FULL; {PUBLISH}"
            ),
            &spec,
            None,
        ),
        case(
            "identity",
            format!("CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, x int); {PUBLISH}"),
            &spec,
            Some(
                "fact f: Straum needs the whole old row of every update and delete: \
             ALTER TABLE identity.f REPLICA IDENTITY FULL",
            ),
        ),
    ];
    expect_problems(pg, cases);
}

/// A `character(n)` key, which PostgreSQL compares without its padding;
/// its null-ness and a `varchar` key are read.
fn padded_problems(pg: &PostgresContainer) {
    let cases = vec![
        case(
            "padded",
            published("x int, k char(5)"),
            &grouped("t.k AS k, count(*) AS n", "GROUP BY 1, 2"),
            Some(
                "f: column k of padded.f is character(5); PostgreSQL compares it without its \
                 trailing spaces, and Straum can't: make it text or varchar",
            ),
        ),
        case(
            "paddednull",
            published("x int, k char(5)"),
            &grouped("(t.k IS NULL) AS no_k, count(*) AS n", "GROUP BY 1, 2"),
            None,
        ),
        case(
            "varying",
            published("x int, k varchar(5)"),
            &grouped("t.k AS k, count(*) AS n", "GROUP BY 1, 2"),
            None,
        ),
    ];
    expect_problems(pg, cases);
}

/// A measure, time or key column of a type Straum doesn't read there.
fn column_type_problems(pg: &PostgresContainer) {
    let Fact { table, spec } = summed_fact();
    let cases = vec![
        case(
            "float",
            published("x float8"),
            &spec,
            Some(
                "f: column x of float.f is double precision; Straum reads no float type, \
             as its sums can't equal PostgreSQL's exactly: make it numeric(p, s) with p at most 18",
            ),
        ),
        case(
            "naive",
            format!("CREATE TABLE $S.f (id int PRIMARY KEY, at timestamp, x int); {PUBLISH}"),
            &spec,
            Some(
                "f: column at of naive.f is timestamp without time zone; the date_bin origin \
             timestamptz 'epoch' needs a timestamptz there; for a timestamp column, \
             write timestamp 'epoch'",
            ),
        ),
        case(
            "zoned",
            format!("{table} {PUBLISH}"),
            &spec.replace("timestamptz 'epoch'", "timestamp 'epoch'"),
            Some(
                "f: column at of zoned.f is timestamp with time zone; the date_bin origin \
             timestamp 'epoch' needs a timestamp there; for a timestamptz column, \
             write timestamptz 'epoch'",
            ),
        ),
        case(
            "notime",
            published("x int, k timestamptz"),
            &spec.replace("t.at", "t.x"),
            Some(
                "f: column x of notime.f is integer; the date_bin origin timestamptz 'epoch' \
             needs a timestamptz there; for a timestamp column, write timestamp 'epoch'",
            ),
        ),
        case(
            "timekey",
            published("x int, k timestamptz"),
            &grouped("t.k AS k, count(*) AS n", "GROUP BY 1, 2"),
            Some(
                "f: column k of timekey.f is timestamp with time zone; \
             Straum needs an integer type, numeric(p, s), boolean, text, date or uuid there",
            ),
        ),
        // count(DISTINCT t.x) reads text, but a sum of distinct values
        // takes numbers.
        case(
            "textdistinct",
            published("x text"),
            &fact("", "sum(DISTINCT t.x) AS xs").spec,
            Some(
                "f: column x of textdistinct.f is text; \
             Straum needs an integer type or numeric(p, s) with p at most 18 there",
            ),
        ),
        case(
            "naivekey",
            published("x int, k timestamp"),
            &grouped("t.k AS k, count(*) AS n", "GROUP BY 1, 2"),
            Some(
                "f: column k of naivekey.f is timestamp without time zone; \
             Straum needs an integer type, numeric(p, s), boolean, text, date or uuid there",
            ),
        ),
    ];
    expect_problems(pg, cases);
}

/// Floats refused whatever their use, dates and uuids read as keys but
/// never summed, and their filter literals spelled as PostgreSQL prints
/// them.
fn date_uuid_and_float_problems(pg: &PostgresContainer) {
    let Fact { spec, .. } = summed_fact();
    let cases = vec![
        case(
            "realkey",
            published("x int, k real"),
            &grouped("t.k AS k, count(*) AS n", "GROUP BY 1, 2"),
            Some(
                "f: column k of realkey.f is real; Straum reads no float type, \
             as its sums can't equal PostgreSQL's exactly: make it numeric(p, s) with p at most 18",
            ),
        ),
        case(
            "datesum",
            published("x date"),
            &spec,
            Some(
                "f: column x of datesum.f is date; \
             Straum needs an integer type or numeric(p, s) with p at most 18 there",
            ),
        ),
        case(
            "uuidmax",
            published("x uuid"),
            &fact("", "max(t.x) AS greatest_x").spec,
            Some(
                "f: column x of uuidmax.f is uuid; \
             Straum needs an integer type or numeric(p, s) with p at most 18 there",
            ),
        ),
        case(
            "datekeys",
            published("k date, u uuid"),
            &grouped(
                "t.k AS k, t.u AS u, count(DISTINCT t.u) AS us, count(t.k) AS ks, \
                 count(*) FILTER (WHERE t.k = '2024-01-15') AS n, \
                 count(*) FILTER (WHERE t.u = 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11') AS m",
                "GROUP BY 1, 2, 3",
            ),
            None,
        ),
        case(
            "filterdatespelling",
            published("k date"),
            &grouped(
                "t.k AS k, count(*) FILTER (WHERE t.k = '2024-1-15') AS n",
                "GROUP BY 1, 2",
            ),
            Some(
                "view f: measure n compares k (a date) with '2024-1-15'; \
                 write a date as YYYY-MM-DD, such as 2024-01-15",
            ),
        ),
        case(
            "filteruuidspelling",
            published("k uuid"),
            &grouped(
                "t.k AS k, count(*) FILTER (WHERE t.k = 'A0EE') AS n",
                "GROUP BY 1, 2",
            ),
            Some(
                "view f: measure n compares k (a uuid) with 'A0EE'; write a uuid as lowercase \
                 hex in groups of 8-4-4-4-12, such as a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            ),
        ),
    ];
    expect_problems(pg, cases);
}

/// A column counted or aggregated as a boolean.
fn aggregate_problems(pg: &PostgresContainer) {
    let cases = vec![
        case(
            "counted",
            published("x int, k timestamptz"),
            &fact("", "count(t.k) AS n").spec,
            None,
        ),
        case(
            "uncounted",
            published("x int, k interval"),
            &fact("", "count(t.k) AS n").spec,
            Some(
                "f: column k of uncounted.f is interval; Straum needs a type it reads there: \
             an integer type, numeric(p, s), boolean, text, timestamptz, timestamp, date or uuid",
            ),
        ),
        case(
            "boolean",
            published("x int, k boolean"),
            &fact("", "bool_and(t.k) AS every, bool_or(t.k) AS some").spec,
            None,
        ),
        case(
            "notboolean",
            published("x int, k int"),
            &fact("", "bool_or(t.k) AS some").spec,
            Some("f: column k of notboolean.f is integer; Straum needs boolean there"),
        ),
    ];
    expect_problems(pg, cases);
}

/// A `FILTER` or `WHERE` comparing a column with a literal of another
/// type.
fn filter_problems(pg: &PostgresContainer) {
    let by_key = "GROUP BY 1, 2";
    let cases = vec![
        case(
            "filter",
            published("k text"),
            &grouped("t.k AS k, count(*) FILTER (WHERE t.k = 1) AS n", by_key),
            Some("view f: measure n compares k (text) with 1"),
        ),
        case(
            "filtertext",
            published("k int"),
            &grouped("t.k AS k, count(*) FILTER (WHERE t.k = 'a') AS n", by_key),
            Some("view f: measure n compares k (an integer) with 'a'"),
        ),
        case(
            "filterbool",
            published("k text"),
            &grouped("t.k AS k, count(*) FILTER (WHERE t.k = true) AS n", by_key),
            Some("view f: measure n compares k (text) with true"),
        ),
        case(
            "filterscale",
            published("k numeric(6, 2)"),
            &grouped("t.k AS k, count(*) FILTER (WHERE t.k = 1) AS n", by_key),
            Some("view f: measure n compares k (a numeric of scale 2) with 1"),
        ),
        case(
            "filterwhole",
            published("k numeric(6, 0)"),
            &grouped("t.k AS k, count(*) FILTER (WHERE t.k = 1) AS n", by_key),
            None,
        ),
        case(
            "nullkeyok",
            published("k timestamp"),
            &grouped(
                "(t.k IS NULL) AS open, count(*) FILTER (WHERE t.k IS NOT NULL) AS n",
                by_key,
            ),
            None,
        ),
        case(
            "wheretype",
            published("k text"),
            &fact("", "t.k AS k, count(*) AS n")
                .spec
                .replace("AS t GROUP BY 1", "AS t WHERE t.k = 1 GROUP BY 1, 2"),
            Some("view f: WHERE compares k (text) with 1"),
        ),
    ];
    expect_problems(pg, cases);
}

/// An enum column is read as its label when written `::text` (ADR-0004).
fn enum_problems(pg: &PostgresContainer) {
    let cases = vec![
        case(
            "enumok",
            format!("{MOOD} {}", published("m $S.mood")),
            &grouped(
                "t.m::text AS m, (t.m::text IS NULL) AS moody, \
                 count(*) FILTER (WHERE t.m::text = 'ok') AS n",
                "GROUP BY 1, 2, 3",
            ),
            None,
        ),
        case(
            "enumbare",
            format!("{MOOD} {}", published("m $S.mood")),
            &grouped("t.m AS m, count(*) AS n", "GROUP BY 1, 2"),
            Some("f: column m of enumbare.f is enumbare.mood, an enum; write m::text"),
        ),
        case(
            "notenum",
            published("k text"),
            &grouped("t.k::text AS k, count(*) AS n", "GROUP BY 1, 2"),
            Some("f: column k of notenum.f is text, not an enum; drop the ::text"),
        ),
        case(
            "nolabel",
            format!("{MOOD} {}", published("m $S.mood")),
            &grouped(
                "t.m::text AS m, count(*) FILTER (WHERE t.m::text IN ('ok', 'meh')) AS n",
                "GROUP BY 1, 2",
            ),
            Some("view f: measure n compares m with 'meh', which is no label of nolabel.mood"),
        ),
    ];
    expect_problems(pg, cases);
}

/// A view joining the fact to dim `d` on its key `k`.
const DIM_SPEC: &str = "
        CREATE VIEW f WITH (buckets = '1 hour') AS
        SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
               t.d_id AS d_id, d.name AS name, count(*) AS n
        FROM $S.f AS t LEFT OUTER JOIN $S.d AS d ON d.k = t.d_id GROUP BY 1, 2, 3;";

/// A dim is held and followed, so it is published with the whole old
/// row, as a fact is. `dim` creates it; the fact joins it on `via`.
fn dim_tables(dim: &str, via: &str) -> String {
    format!(
        "CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, d_id {via});
         ALTER TABLE $S.f REPLICA IDENTITY FULL;
         {dim}
         ALTER TABLE $S.d REPLICA IDENTITY FULL;
         CREATE PUBLICATION $S FOR TABLE $S.f, $S.d;"
    )
}

/// A dim unpublished, published without its whole old row, filtered or
/// cut, with any or no key of its own, or of a type its join can't read.
fn dim_problems(pg: &PostgresContainer) {
    let keyed = dim_tables("CREATE TABLE $S.d (k int PRIMARY KEY, name text);", "int");
    let cases = vec![
        case("dimok", keyed.clone(), DIM_SPEC, None),
        case(
            "dimunpublished",
            keyed.replace(", $S.d;", ";"),
            DIM_SPEC,
            Some(
                "d: publication dimunpublished leaves out dimunpublished.d: \
             ALTER PUBLICATION dimunpublished ADD TABLE dimunpublished.d",
            ),
        ),
        case(
            "dimidentity",
            keyed.replace("ALTER TABLE $S.d REPLICA IDENTITY FULL;", ""),
            DIM_SPEC,
            Some(
                "dim d: Straum needs the whole old row of every update and delete: \
             ALTER TABLE dimidentity.d REPLICA IDENTITY FULL",
            ),
        ),
        case(
            "dimfiltered",
            keyed.replace("$S.f, $S.d;", "$S.f, $S.d WHERE (k > 0);"),
            DIM_SPEC,
            Some(
                "d: publication dimfiltered filters the rows of dimfiltered.d; Straum needs every row",
            ),
        ),
        case(
            "dimlisted",
            keyed.replace("$S.f, $S.d;", "$S.f, $S.d (k);"),
            DIM_SPEC,
            Some("d: publication dimlisted leaves out column name of dimlisted.d"),
        ),
        case(
            "dimunique",
            dim_tables("CREATE TABLE $S.d (k int UNIQUE, name text);", "int"),
            DIM_SPEC,
            None,
        ),
        case(
            "dimpk",
            dim_tables(
                "CREATE TABLE $S.d (s serial PRIMARY KEY, k int, name text);",
                "int",
            ),
            DIM_SPEC,
            None,
        ),
        case(
            "dimnounique",
            dim_tables(
                "CREATE TABLE $S.d (k int, name text); ALTER TABLE $S.d REPLICA IDENTITY FULL;",
                "int",
            ),
            DIM_SPEC,
            None,
        ),
        case(
            "dimtype",
            dim_tables("CREATE TABLE $S.d (k text PRIMARY KEY, name text);", "int"),
            DIM_SPEC,
            Some("view f: join d joins d_id (an integer) to k (text); they must read alike"),
        ),
    ];
    expect_problems(pg, cases);
}

/// A dim Straum's role may not read.
fn dim_role_problems(pg: &PostgresContainer) {
    // As the role that may read the fact but not the dim.
    let name = "dimnoselect";
    let tables = dim_tables("CREATE TABLE $S.d (k int PRIMARY KEY, name text);", "int").replace(
        "CREATE PUBLICATION",
        "CREATE ROLE $S LOGIN REPLICATION PASSWORD 'r'; GRANT USAGE ON SCHEMA $S TO $S; \
             GRANT SELECT ON $S.f TO $S; CREATE PUBLICATION",
    );
    let db = Db::new(pg, name, &tables);
    let dsn = pg
        .dsn
        .to_string()
        .replace("postgres@", &format!("{name}:r@"));
    let mut source = Postgres::new(
        Dsn::parse(&dsn).unwrap(),
        name,
        ROW_LIMIT,
        MAX_RETAINED_WAL_BYTES,
        Replication::Logical,
    );
    assert_eq!(
        setup_problems(name, source.open(&db.spec(DIM_SPEC))),
        ["d: Straum's role can't read dimnoselect.d: GRANT SELECT ON dimnoselect.d"],
        "{name}"
    );
}

/// A policy that hides rows from Straum's role, which no owner,
/// superuser or `BYPASSRLS` role is subject to.
fn row_security_problems(pg: &PostgresContainer) {
    let name = "policed";
    let Fact { table, spec } = summed_fact();
    let tables = format!(
        "{table} {PUBLISH}
         CREATE ROLE $S LOGIN REPLICATION PASSWORD 'r'; GRANT USAGE ON SCHEMA $S TO $S;
         GRANT SELECT ON $S.f TO $S;
         ALTER TABLE $S.f ENABLE ROW LEVEL SECURITY;
         CREATE POLICY positive ON $S.f USING (x > 0);"
    );
    let mut db = Db::new(pg, name, &tables);
    let dsn = pg
        .dsn
        .to_string()
        .replace("postgres@", &format!("{name}:r@"));
    let spec = db.spec(&spec);
    assert_eq!(
        setup_problems(name, logical_source(&dsn, name).open(&spec)),
        [
            "f: row-level security filters what policed reads of policed.f; \
             Straum needs every row: ALTER ROLE policed BYPASSRLS, as a superuser"
        ],
    );
    // Past the check, a read the policy would cut fails whole.
    let mut connection =
        Connection::connect(&Dsn::parse(&dsn).unwrap(), ConnectionKind::Sql).unwrap();
    let error = connection
        .rows(&db.sql("SELECT count(*) FROM $S.f"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("row-level security"), "{error}");
    db.run("ALTER ROLE $S BYPASSRLS");
    let opened = logical_source(&dsn, name).open(&spec);
    assert_eq!(setup_problems(name, opened), Vec::<String>::new());
}

/// A logical source over the publication `name`, signed in as `dsn` says.
fn logical_source(dsn: &str, name: &str) -> Postgres {
    Postgres::new(
        Dsn::parse(dsn).unwrap(),
        name,
        ROW_LIMIT,
        MAX_RETAINED_WAL_BYTES,
        Replication::Logical,
    )
}

/// A dim keyed by two columns, with or without a key of its own.
const PAIR_SPEC: &str = "
        CREATE VIEW f WITH (buckets = '1 hour') AS
        SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
               t.a AS a, t.b AS b, d.name AS name, count(*) AS n
        FROM $S.f AS t LEFT OUTER JOIN $S.d AS d ON d.x = t.a AND d.y = t.b
        GROUP BY 1, 2, 3, 4;";

/// `dim` creates the dim; the fact's second key column is of `b_type`.
fn pair_tables(dim: &str, b_type: &str) -> String {
    format!(
        "CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, a int, b {b_type});
         ALTER TABLE $S.f REPLICA IDENTITY FULL;
         {dim}
         ALTER TABLE $S.d REPLICA IDENTITY FULL;
         CREATE PUBLICATION $S FOR TABLE $S.f, $S.d;"
    )
}

/// A dim keyed by two columns in either order, by more, by a key of one
/// column, or by a column of another type.
fn pair_problems(pg: &PostgresContainer) {
    let cases = vec![
        case(
            "pairok",
            pair_tables(
                "CREATE TABLE $S.d (x int, y int, name text, PRIMARY KEY (x, y));",
                "int",
            ),
            PAIR_SPEC,
            None,
        ),
        case(
            "pairorder",
            pair_tables(
                "CREATE TABLE $S.d (x int, y int, name text, PRIMARY KEY (y, x));",
                "int",
            ),
            PAIR_SPEC,
            None,
        ),
        case(
            "pairfull",
            pair_tables(
                "CREATE TABLE $S.d (x int, y int, name text, UNIQUE (x, y));
                 ALTER TABLE $S.d REPLICA IDENTITY FULL;",
                "int",
            ),
            PAIR_SPEC,
            None,
        ),
        case(
            "pairsuper",
            pair_tables(
                "CREATE TABLE $S.d (x int, y int, name text, UNIQUE (x, y, name));
                 ALTER TABLE $S.d REPLICA IDENTITY FULL;",
                "int",
            ),
            PAIR_SPEC,
            None,
        ),
        case(
            "pairpk",
            pair_tables(
                "CREATE TABLE $S.d (x int PRIMARY KEY, y int, name text);",
                "int",
            ),
            PAIR_SPEC,
            None,
        ),
        case(
            "pairtype",
            pair_tables(
                "CREATE TABLE $S.d (x int, y text, name text, PRIMARY KEY (x, y));",
                "int",
            ),
            PAIR_SPEC,
            Some("view f: join d joins b (an integer) to y (text); they must read alike"),
        ),
    ];
    expect_problems(pg, cases);
}

/// A chain joins a dim's column to the next dim's key, and a semi join's
/// dim is published like any other.
const CHAIN_SPEC: &str = "
        CREATE VIEW f WITH (buckets = '1 hour') AS
        SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
               d.name AS name, e.label AS label, count(*) AS n
        FROM $S.f AS t
        LEFT OUTER JOIN $S.d AS d ON d.k = t.d_id
        LEFT OUTER JOIN $S.e AS e ON e.k = d.e_id
        WHERE EXISTS (SELECT 1 FROM $S.s AS s WHERE s.k = t.d_id)
        GROUP BY 1, 2, 3;";

/// The chain's tables, `e`'s key of `e_key`, `published` in the
/// publication.
fn chain_tables(e_key: &str, published: &str) -> String {
    format!(
        "CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, d_id int);
         CREATE TABLE $S.d (k int, name text, e_id int);
         CREATE TABLE $S.e (k {e_key}, label text);
         CREATE TABLE $S.s (k int);
         ALTER TABLE $S.f REPLICA IDENTITY FULL;
         ALTER TABLE $S.d REPLICA IDENTITY FULL;
         ALTER TABLE $S.e REPLICA IDENTITY FULL;
         ALTER TABLE $S.s REPLICA IDENTITY FULL;
         CREATE PUBLICATION $S FOR TABLE {published};"
    )
}

/// A chain whose far key is of another type, and a semi join's dim left
/// out of the publication.
fn chain_problems(pg: &PostgresContainer) {
    let cases = vec![
        case(
            "chainok",
            chain_tables("int", "$S.f, $S.d, $S.e, $S.s"),
            CHAIN_SPEC,
            None,
        ),
        case(
            "chaintype",
            chain_tables("text", "$S.f, $S.d, $S.e, $S.s"),
            CHAIN_SPEC,
            Some("view f: join e joins d.e_id (an integer) to k (text); they must read alike"),
        ),
        case(
            "semiunpublished",
            chain_tables("int", "$S.f, $S.d, $S.e"),
            CHAIN_SPEC,
            Some(
                "s: publication semiunpublished leaves out semiunpublished.s: \
             ALTER PUBLICATION semiunpublished ADD TABLE semiunpublished.s",
            ),
        ),
    ];
    expect_problems(pg, cases);
}

#[test]
fn reports_wal_level_and_read_access() {
    let pg = postgres();
    let mut db = Db::new(&pg, "access", TABLES);
    let spec = db.spec(SPEC);
    let mut catalog = catalog::query(&mut db.connection, &spec, "access").unwrap();
    assert!(!catalog.in_recovery);
    assert!(catalog::check(&spec, &catalog, "access", Replication::Logical).is_ok());
    let major: String = tag().chars().take_while(char::is_ascii_digit).collect();
    assert_eq!(
        catalog.server_version / 10_000,
        major.parse::<u32>().unwrap()
    );
    for (version, wal_level, needs) in [
        (170_000, "replica", Some("logical")),
        (170_000, "logical", None),
        (180_000, "replica", Some("logical")),
        (180_000, "logical", None),
        (190_000, "replica", None),
        (190_000, "logical", None),
        (190_000, "minimal", Some("replica or logical")),
        (200_000, "replica", None),
    ] {
        catalog.server_version = version;
        catalog.wal_level = wal_level.into();
        let mut expected = None;
        if let Some(needs) = needs {
            expected = Some(vec![format!(
                "wal_level is {wal_level}; Straum needs {needs}, which takes a restart"
            )]);
        }
        assert_eq!(
            catalog::check(&spec, &catalog, "access", Replication::Logical)
                .err()
                .as_deref()
                .map(crate::problem::lines),
            expected,
            "{version} {wal_level}"
        );
    }
    catalog.server_version = 160_009;
    catalog.wal_level = "logical".into();
    assert_eq!(
        catalog::check(&spec, &catalog, "access", Replication::Logical)
            .err()
            .as_deref()
            .map(crate::problem::lines),
        Some(vec![
            "PostgreSQL 16 is too old; Straum needs 17 or newer".to_owned()
        ])
    );
    // A server too old has its tables left unread, whose columns its
    // catalog may lack: the check names the version alone.
    // A standby is no source, whatever else it is.
    catalog.server_version = 180_000;
    catalog.in_recovery = true;
    let standby = catalog::check(&spec, &catalog, "access", Replication::Logical);
    assert_eq!(
        crate::problem::lines(&standby.unwrap_err()),
        ["the server is a standby in recovery; give Straum the primary's DSN"]
    );
    catalog.in_recovery = false;
    catalog.server_version = 160_009;
    let tables = std::mem::take(&mut catalog.classes);
    let too_old = catalog::check(&spec, &catalog, "access", Replication::Logical);
    assert_eq!(too_old.unwrap_err().len(), 1);
    catalog.server_version = 170_000;
    let unread = catalog::check(&spec, &catalog, "access", Replication::Logical);
    assert!(unread.unwrap_err().len() > 1);
    catalog.classes = tables;
    the_role_must_read_every_table(&mut db, &pg, &spec);
}

/// A role that may not read the tables can't open, and the error names
/// each table.
fn the_role_must_read_every_table(db: &mut Db, pg: &PostgresContainer, spec: &straum_domain::Spec) {
    db.run("CREATE ROLE reader LOGIN REPLICATION PASSWORD 'r'; GRANT USAGE ON SCHEMA $S TO reader");
    let dsn = pg.dsn.to_string().replace("postgres@", "reader:r@");
    let mut reader = Postgres::new(
        Dsn::parse(&dsn).unwrap(),
        "access",
        ROW_LIMIT,
        MAX_RETAINED_WAL_BYTES,
        Replication::Logical,
    );
    let error = reader.open(spec).unwrap_err().to_string();
    for table in ["zones", "trips", "logins"] {
        let line =
            format!("Straum's role can't read access.{table}: GRANT SELECT ON access.{table}");
        assert!(error.contains(&line), "{error}");
    }
}

//! `copy` reads the spec's tables from one database and writes them into
//! another: columns, enums and rows, in one snapshot. A role that can
//! only `SELECT` is enough. A missing table, a table it cannot read, or
//! a composite column stops it and names what.

use super::support::*;
use crate::copy::copy;
use crate::dsn::Dsn;
use crate::wire::{Connection, ConnectionKind};

const TABLES: &str = "
CREATE TYPE $S.kind AS ENUM ('a', 'b', 'o''brien');
CREATE DOMAIN $S.region AS text;
CREATE TABLE $S.accounts (id int, region $S.region);
CREATE TABLE $S.events (
    at timestamptz,
    kind $S.kind,
    kinds $S.kind[],
    account_id int,
    amount numeric(10, 2),
    doubled numeric GENERATED ALWAYS AS (amount * 2) STORED,
    note text
);
CREATE INDEX ON $S.events (account_id);
CREATE TABLE $S.other (id int);
INSERT INTO $S.accounts VALUES (1, 'NO'), (2, NULL);
INSERT INTO $S.events (at, kind, kinds, account_id, amount, note) VALUES
    ('2026-11-01 00:00:00+00', 'a', ARRAY['a', 'b']::$S.kind[], 1, 1.50, E'a\\\\b\\n'),
    ('2026-11-01 01:00:00+00', 'o''brien', NULL, NULL, NULL, NULL),
    ('2026-11-01 02:00:00+00', NULL, ARRAY[]::$S.kind[], 2, -2.25, NULL);
INSERT INTO $S.other VALUES (7);
CREATE ROLE reader LOGIN PASSWORD 'reader';
GRANT USAGE ON SCHEMA $S TO reader;
GRANT SELECT ON $S.events, $S.accounts TO reader;
";

const SPEC: &str = "
CREATE VIEW usage WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', e.at, timestamptz 'epoch') AS bucket,
       e.kind::text AS kind,
       e.account_id AS account_id,
       a.region AS region,
       count(*) AS events,
       sum(e.amount) AS amount
FROM $S.events AS e
LEFT OUTER JOIN $S.accounts AS a ON a.id = e.account_id
GROUP BY 1, 2, 3, 4;
";

#[test]
fn copies_the_specs_tables_as_a_reader() {
    let pg = postgres();
    let mut source = Db::new(&pg, "src", TABLES);
    let mut to = pg.dsn.clone();
    to.database = "copydest".into();
    source.run("CREATE DATABASE copydest");
    let mut from = pg.dsn.clone();
    from.user = "reader".into();
    from.password = Some("reader".into());
    copy(&source.spec(SPEC), &from, &to).unwrap();

    let mut dest = Connection::connect(&to, ConnectionKind::Sql).unwrap();
    same(
        &mut source,
        &mut dest,
        "SELECT at::text, kind::text, kinds::text, account_id::text, amount::text,
                doubled::text, note
           FROM $S.events ORDER BY at",
    );
    same(
        &mut source,
        &mut dest,
        "SELECT id::text, region::text FROM $S.accounts ORDER BY id",
    );
    assert_eq!(column(&mut dest, &source, "accounts", "region"), "text");
    assert_eq!(
        column(&mut dest, &source, "events", "amount"),
        "numeric(10,2)"
    );
    assert_eq!(column(&mut dest, &source, "events", "kind"), "src.kind");
    assert_eq!(column(&mut dest, &source, "events", "doubled"), "numeric");
    assert_eq!(
        count(
            &mut source,
            "SELECT count(*) FROM pg_index i
        JOIN pg_class c ON c.oid = i.indrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = '$S' AND c.relname = 'events'"
        ),
        "1"
    );
    assert_eq!(count_on(&mut dest, &source, "events"), "0");
    let other = dest
        .rows(&source.sql("SELECT to_regclass('$S.other')"))
        .unwrap();
    assert_eq!(other, vec![vec![None]]);
}

const BAD: &str = "
CREATE TABLE $S.secret (at timestamptz, n int);
CREATE TYPE $S.address AS (city text);
CREATE TABLE $S.places (at timestamptz, address $S.address);
CREATE ROLE blind LOGIN PASSWORD 'blind';
GRANT USAGE ON SCHEMA $S TO blind;
";

#[test]
fn copy_names_what_it_will_not_copy() {
    let pg = postgres();
    let mut source = Db::new(&pg, "src", BAD);
    let mut to = pg.dsn.clone();
    to.database = "copydest".into();
    source.run("CREATE DATABASE copydest");
    let mut blind = pg.dsn.clone();
    blind.user = "blind".into();
    blind.password = Some("blind".into());

    assert_eq!(
        stop(&source, MISSING, &pg.dsn, &to),
        "src.missing does not exist"
    );
    assert_eq!(
        stop(&source, SECRET, &blind, &to),
        "src.secret: the role cannot SELECT it"
    );
    assert_eq!(
        stop(&source, PLACES, &pg.dsn, &to),
        "src.places.address has type src.address, which a copy does not create"
    );
    let mut dest = Connection::connect(&to, ConnectionKind::Sql).unwrap();
    let schemas = dest
        .rows("SELECT count(*) FROM pg_namespace WHERE nspname = 'src'")
        .unwrap();
    assert_eq!(schemas, vec![vec![Some("0".into())]]);
}

const MISSING: &str = "
CREATE VIEW missing WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', m.at, timestamptz 'epoch') AS bucket, count(*) AS n
FROM $S.missing AS m
GROUP BY 1;
";

const SECRET: &str = "
CREATE VIEW secret WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', s.at, timestamptz 'epoch') AS bucket, count(*) AS n
FROM $S.secret AS s
GROUP BY 1;
";

const PLACES: &str = "
CREATE VIEW places WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', p.at, timestamptz 'epoch') AS bucket, count(*) AS n
FROM $S.places AS p
GROUP BY 1;
";

fn stop(source: &Db, spec: &str, from: &Dsn, to: &Dsn) -> String {
    copy(&source.spec(spec), from, to).unwrap_err().to_string()
}

fn same(source: &mut Db, dest: &mut Connection, sql: &str) {
    let expect = source.rows(sql);
    let got = dest.rows(&source.sql(sql)).unwrap();
    assert_eq!(got, expect, "{sql}");
}

fn column(dest: &mut Connection, source: &Db, table: &str, column: &str) -> String {
    let sql = source.sql(&format!(
        "SELECT format_type(a.atttypid, a.atttypmod)
           FROM pg_attribute a
           JOIN pg_class c ON c.oid = a.attrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = '$S' AND c.relname = '{table}' AND a.attname = '{column}'"
    ));
    dest.rows(&sql).unwrap()[0][0].clone().unwrap()
}

fn count(source: &mut Db, sql: &str) -> String {
    source.rows(sql)[0][0].clone().unwrap()
}

fn count_on(dest: &mut Connection, source: &Db, table: &str) -> String {
    let sql = source.sql(&format!(
        "SELECT count(*) FROM pg_index i
           JOIN pg_class c ON c.oid = i.indrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = '$S' AND c.relname = '{table}'"
    ));
    dest.rows(&sql).unwrap()[0][0].clone().unwrap()
}

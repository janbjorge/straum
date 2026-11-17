//! Random writes, dropped connections and restarts, the zones moving too.
//! After every step the writer's
//! answers must equal a fresh load of the database at the same LSN, dims
//! and facts.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use straum_app::{AnswerError, App, Error, Lsn, Step, Writer};
use straum_domain::{Ask, Condition, Datum, Literal};

use crate::common::{
    BOROUGHS, DAY, Harness, LIMITS, RANGE, RIDERS, aside, ask, int, spec, text, watched, zone,
};
use crate::fake::{Database, Fake, Row, Write, id};

/// splitmix64. `src/lag.rs` has its own: a unit test can't reach an
/// integration test's helpers, and the crate exports no test code.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut bits = self.0;
        bits = (bits ^ (bits >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        bits = (bits ^ (bits >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        bits ^ (bits >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        let bound = u64::try_from(bound).expect("a usize fits a u64");
        usize::try_from(self.next() % bound).expect("below a usize")
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn int(&mut self, low: i64, high: i64) -> i64 {
        let span = u64::try_from(high - low).expect("low below high");
        low + i64::try_from(self.next() % span).expect("below an i64 span")
    }

    /// An integer in `low..high`, or null one time in ten.
    fn maybe(&mut self, low: i64, high: i64) -> Datum {
        if self.chance(10) {
            Datum::Null
        } else {
            int(self.int(low, high))
        }
    }

    fn maybe_text(&mut self, choices: &[&str]) -> Datum {
        if self.chance(10) {
            Datum::Null
        } else {
            text(choices[self.below(choices.len())])
        }
    }

    // Generic: it picks keys and table names alike.
    fn pick<T: Clone>(&mut self, items: &[T]) -> Option<T> {
        if items.is_empty() {
            return None;
        }
        Some(items[self.below(items.len())].clone())
    }
}

fn random_trip(random: &mut Rng) -> Row {
    vec![
        int(random.int(0, RANGE)),
        random.maybe(1, 4),
        // Zones 7 and 8 never exist, so their attributes are null.
        random.maybe(1, 9),
        random.maybe(-50, 200),
        random.maybe_text(&RIDERS),
    ]
}

fn random_login(random: &mut Rng) -> Row {
    vec![int(random.int(0, RANGE)), random.maybe(1, 9)]
}

/// One transaction's writes. `next_key` hands out fact keys.
fn random_writes(random: &mut Rng, database: &Database, next_key: &mut i64) -> Vec<Write> {
    let mut transaction = Transaction::new(database, *next_key);
    let mut writes = Vec::new();
    for _ in 0..=random.below(4) {
        let Some(write) = transaction.random_write(random) else {
            continue;
        };
        transaction.record(&write);
        writes.push(write);
    }
    *next_key = transaction.next_key;
    writes
}

/// Keys as the transaction goes, so no write misses its row.
struct Transaction {
    keys: BTreeMap<&'static str, BTreeSet<Datum>>,
    /// The last fact key handed out.
    next_key: i64,
}

impl Transaction {
    fn new(database: &Database, next_key: i64) -> Transaction {
        let mut keys = BTreeMap::new();
        for table in ["zones", "trips", "logins"] {
            keys.insert(table, database.keys(table).into_iter().collect());
        }
        Transaction { keys, next_key }
    }

    fn new_key(&mut self) -> Datum {
        self.next_key += 1;
        int(self.next_key)
    }

    fn existing(&self, table: &str) -> Vec<Datum> {
        self.keys[table].iter().cloned().collect()
    }

    /// `None` when the write it drew has no row to change.
    fn random_write(&mut self, random: &mut Rng) -> Option<Write> {
        let write = match random.below(100) {
            0..40 => Write::Insert {
                table: "trips",
                key: self.new_key(),
                row: random_trip(random),
            },
            40..55 => self.trip_update(random)?,
            55..65 => Write::Delete {
                table: "trips",
                key: random.pick(&self.existing("trips"))?,
            },
            65..80 => Write::Insert {
                table: "logins",
                key: self.new_key(),
                row: random_login(random),
            },
            // Logins send no old rows; these make Straum start over.
            80..84 => self.login_change(random)?,
            84..87 => Write::Insert {
                table: "audit",
                key: self.new_key(),
                row: vec![text("x")],
            },
            // The zones move: relabelled, moved to a new key, gone, back.
            87..92 => self.zone_change(random)?,
            92..95 => Write::Truncate {
                table: random.pick(&["trips", "logins"]).unwrap(),
            },
            _ => Write::Alter {
                table: random.pick(&["zones", "trips", "logins"]).unwrap(),
            },
        };
        Some(write)
    }

    fn trip_update(&mut self, random: &mut Rng) -> Option<Write> {
        let key = random.pick(&self.existing("trips"))?;
        let new_key = if random.chance(20) {
            self.new_key()
        } else {
            key.clone()
        };
        Some(Write::Update {
            table: "trips",
            key,
            new_key,
            row: random_trip(random),
        })
    }

    fn login_change(&self, random: &mut Rng) -> Option<Write> {
        let key = random.pick(&self.existing("logins"))?;
        if random.chance(50) {
            return Some(Write::Delete {
                table: "logins",
                key,
            });
        }
        Some(Write::Update {
            table: "logins",
            new_key: key.clone(),
            key,
            row: random_login(random),
        })
    }

    fn zone_change(&self, random: &mut Rng) -> Option<Write> {
        let key = int(random.int(1, 9));
        let borough = text(random.pick(&BOROUGHS).unwrap());
        if !self.keys["zones"].contains(&key) {
            return Some(Write::Insert {
                table: "zones",
                row: vec![key.clone(), borough],
                key,
            });
        }
        if random.chance(30) {
            return Some(Write::Delete {
                table: "zones",
                key,
            });
        }
        let new_key = if random.chance(20) {
            int(random.int(1, 9))
        } else {
            key.clone()
        };
        if new_key != key && self.keys["zones"].contains(&new_key) {
            return None;
        }
        Some(Write::Update {
            table: "zones",
            row: vec![new_key.clone(), borough],
            key,
            new_key,
        })
    }

    fn record(&mut self, write: &Write) {
        match write {
            // audit keeps no keys here.
            Write::Insert { table, key, .. } => {
                if let Some(held) = self.keys.get_mut(table) {
                    held.insert(key.clone());
                }
            }
            Write::Update {
                table,
                key,
                new_key,
                ..
            } => {
                let held = self.keys.get_mut(table).unwrap();
                held.remove(key);
                held.insert(new_key.clone());
            }
            Write::Delete { table, key } => {
                self.keys.get_mut(table).unwrap().remove(key);
            }
            Write::Truncate { table } => self.keys.get_mut(table).unwrap().clear(),
            Write::Alter { .. } => {}
        }
    }
}

fn random_ask(random: &mut Rng) -> Ask {
    let (fact, measures, keys): (_, &[&str], &[&str]) = if random.chance(70) {
        (
            "trips",
            &["trips", "fare", "fare_max", "riders"],
            &["kind", "zone_id", "borough"],
        )
    } else {
        ("logins", &["logins"], &["borough"])
    };
    let mut chosen = some_of(random, measures, 50);
    if chosen.is_empty() {
        chosen.push(measures[0]);
    }
    let mut by = some_of(random, keys, 40);
    if random.chance(30) {
        by.push("bucket");
    }
    let mut ask = ask(fact, &chosen, &by);
    if random.chance(30) {
        let mut values = Vec::new();
        for borough in some_of(random, &BOROUGHS, 50) {
            values.push(Literal::Text(borough.to_owned()));
        }
        if !values.is_empty() {
            ask.filter.push(Condition {
                key: id("borough"),
                values,
            });
        }
    }
    // Sometimes one day of the range.
    if random.chance(30) {
        let from = DAY * random.int(0, 3);
        ask.from = Some(from);
        ask.to = Some(from + DAY);
    }
    ask
}

/// Each item, kept `percent` times in a hundred.
// The names are literals; the chosen ones stay borrowed, uncopied.
fn some_of<'a>(random: &mut Rng, items: &[&'a str], percent: usize) -> Vec<&'a str> {
    let mut chosen = Vec::new();
    for item in items {
        if random.chance(percent) {
            chosen.push(*item);
        }
    }
    chosen
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Seen {
    answered: usize,
    failed: usize,
    /// Failures from a change that no longer fit.
    unfit: usize,
    /// Asks made while a table loaded.
    aside: usize,
}

/// Checks, in the middle of a load, that asks answer not ready instead of
/// waiting on the writer.
fn check_aside(app: &App) -> usize {
    for fact in ["trips", "logins"] {
        let ask = ask(fact, &[fact], &["borough"]);
        assert_eq!(aside(app, ask), Err(AnswerError::NotReady), "{fact}");
    }
    2
}

/// Asks against a fresh load of the database at `lsn`.
fn check(app: &App, database: &Database, lsn: Lsn, random: &mut Rng, tally: &mut Seen) {
    let reference = database.store_at(&spec(), lsn);
    for _ in 0..4 {
        let ask = random_ask(random);
        let answered = match app.ask(&ask) {
            Ok(answered) => answered,
            Err(error) => panic!("{error} at {lsn} for {ask:?}"),
        };
        assert_eq!(answered.applied, lsn);
        assert_eq!(answered.answer, reference.ask(&ask).unwrap(), "{ask:?}");
        tally.answered += 1;
    }
}

/// A writer that checks asks aside in the middle of every load, and the
/// count of those asks.
struct Followed {
    harness: Harness,
    aside: Rc<RefCell<usize>>,
}

fn followed() -> Followed {
    let aside = Rc::new(RefCell::new(0));
    let count = Rc::clone(&aside);
    let app = App::new(spec(), LIMITS);
    let harness = watched(app, move |app, _| {
        *count.borrow_mut() += check_aside(app);
    });
    let mut zones = Vec::new();
    for key in 1..=6 {
        let borough = BOROUGHS[usize::try_from(key).unwrap() % BOROUGHS.len()];
        zones.push(zone(key, borough));
    }
    harness.database.borrow_mut().commit(zones);
    Followed { harness, aside }
}

/// Follows random writes, checking asks at every LSN.
fn follow(seed: u64, tally: &mut Seen) {
    let mut random = Rng(seed);
    let Followed { harness, aside } = followed();
    let Harness {
        database,
        mut writer,
    } = harness;
    let mut next_key = 0;
    let mut position = None;
    for _ in 0..150 {
        for _ in 0..random.below(4) {
            let transaction = random_writes(&mut random, &database.borrow(), &mut next_key);
            database.borrow_mut().commit(transaction);
        }
        if random.chance(5) {
            database.borrow_mut().drop_connection = true;
        }
        let latest = database.borrow().lsn();
        let mut steps = 0;
        while position != Some(latest) {
            steps += 1;
            assert!(steps < 100, "seed {seed}: stuck");
            position = step(&mut writer, tally);
            let Some(lsn) = position else {
                continue;
            };
            check(writer.app(), &database.borrow(), lsn, &mut random, tally);
        }
        assert!(writer.app().status().ready, "seed {seed}");
    }
    tally.aside += *aside.borrow();
}

/// One step: the LSN it reached, or `None` after counting its failure.
fn step(writer: &mut Writer<Fake>, tally: &mut Seen) -> Option<Lsn> {
    match writer.step() {
        Ok(Step::Started(lsn) | Step::Applied(lsn)) => Some(lsn),
        Err(error) => {
            tally.failed += 1;
            if let Error::Unfit(_) = error {
                tally.unfit += 1;
            }
            None
        }
    }
}

#[test]
fn follows_the_database() {
    let mut seen = Seen::default();
    for seed in 0..16 {
        follow(seed, &mut seen);
    }
    // The run went through every path it claims to test.
    assert!(seen.answered > 1_000, "{seen:?}");
    assert!(seen.failed > seen.unfit, "{seen:?}");
    assert!(seen.unfit > 0, "{seen:?}");
    assert!(seen.aside > 0, "{seen:?}");
}

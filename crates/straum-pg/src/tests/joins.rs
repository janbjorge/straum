//! Every join kind at once, inner, left, semi (`EXISTS`) and anti (`NOT
//! EXISTS`), over a shop's orders: dim rows come, move, double and go,
//! and each view still equals PostgreSQL's.

use straum_app::Step;

use super::references::*;
use super::support::*;

/// A shop with every join kind at once: orders joined to customers, which
/// may hold a key twice and lack a key, through them to tiers, kept by a
/// promo on their sku and dropped by a block on their customer, under a
/// standing `WHERE`; and a view of the same orders keeping every row.
const SHOP: &str = "
CREATE TABLE $S.customers (id int, country text, tier_id int);
CREATE TABLE $S.tiers (id int, label text);
CREATE TABLE $S.promos (sku int, note text);
CREATE TABLE $S.blocked (id int, reason text);
CREATE TABLE $S.orders (
    id int PRIMARY KEY,
    at timestamptz,
    customer_id int,
    sku int,
    qty int,
    status text,
    gift boolean
);
ALTER TABLE $S.customers REPLICA IDENTITY FULL;
ALTER TABLE $S.tiers REPLICA IDENTITY FULL;
ALTER TABLE $S.promos REPLICA IDENTITY FULL;
ALTER TABLE $S.blocked REPLICA IDENTITY FULL;
ALTER TABLE $S.orders REPLICA IDENTITY FULL;
CREATE PUBLICATION $S FOR TABLE $S.customers, $S.tiers, $S.promos, $S.blocked, $S.orders;
";

/// `c.id`, the key of an inner-joined dim, reads `o.customer_id`.
const PAID_SPEC: &str = "
CREATE VIEW paid WITH (buckets = '1 hour, 1 day') AS
SELECT date_bin('1 hour', o.at, timestamptz 'epoch') AS bucket,
       o.status AS status,
       c.id AS customer,
       c.country AS country,
       t.label AS label,
       count(*) AS orders,
       sum(o.qty) AS qty,
       max(o.qty) AS qty_max,
       avg(o.qty) AS qty_avg,
       count(DISTINCT o.qty) AS sizes,
       count(*) FILTER (WHERE c.country = 'NO') AS norwegian,
       bool_and(o.gift) AS all_gifts,
       bool_or(o.gift) AS any_gift
FROM $S.orders AS o
INNER JOIN $S.customers AS c ON c.id = o.customer_id
LEFT OUTER JOIN $S.tiers AS t ON t.id = c.tier_id
WHERE EXISTS (SELECT 1 FROM $S.promos AS p WHERE p.sku = o.sku)
  AND NOT EXISTS (SELECT 1 FROM $S.blocked AS b WHERE b.id = o.customer_id)
  AND o.status IN ('paid', 'shipped')
  AND c.country IN ('NO', 'SE', 'DK')
GROUP BY 1, 2, 3, 4, 5;
";

const OFFERS_SPEC: &str = "
CREATE VIEW offers WITH (buckets = '1 hour, 1 day') AS
SELECT date_bin('1 hour', o.at, timestamptz 'epoch') AS bucket,
       c.country AS country,
       p.note AS note,
       count(*) AS orders,
       sum(o.qty) AS qty,
       min(o.qty) AS qty_min,
       bool_or(o.gift) FILTER (WHERE c.country = 'NO') AS norwegian_gift
FROM $S.orders AS o
LEFT OUTER JOIN $S.customers AS c ON c.id = o.customer_id
LEFT OUTER JOIN $S.promos AS p ON p.sku = o.sku
GROUP BY 1, 2, 3;
";

/// The shop specs' references, the paid view's then the offers view's: each
/// by bucket, and the paid view by country and label over a window that
/// cuts the day bucket `START` begins, against the raw rows joined as the
/// view says.
fn shop_oracles() -> ShopReferences {
    const PAID: [&str; 8] = [
        "orders",
        "qty",
        "qty_max",
        "qty_avg",
        "sizes",
        "norwegian",
        "all_gifts",
        "any_gift",
    ];
    let paid = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, status, customer, country, label, orders,
                    qty, qty_max, round(qty_avg, 6), sizes, norwegian, all_gifts, any_gift
               FROM ({}) v",
            body(PAID_SPEC, "paid")
        ),
        ask: ask(
            "paid",
            &PAID,
            &["bucket", "status", "customer", "country", "label"],
        ),
        scales: vec![0; 13],
    };
    let offers = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, country, note, orders, qty, qty_min,
                    norwegian_gift
               FROM ({}) v",
            body(OFFERS_SPEC, "offers")
        ),
        ask: ask(
            "offers",
            &["orders", "qty", "qty_min", "norwegian_gift"],
            &["bucket", "country", "note"],
        ),
        scales: vec![0; 7],
    };
    let window = Reference {
        sql: format!(
            "SELECT c.country, t.label, count(*), sum(o.qty), max(o.qty), round(avg(o.qty), 6),
                    count(DISTINCT o.qty), count(*) FILTER (WHERE c.country = 'NO'),
                    bool_and(o.gift), bool_or(o.gift)
               FROM $S.orders o
               JOIN $S.customers c ON c.id = o.customer_id
               LEFT JOIN $S.tiers t ON t.id = c.tier_id
              WHERE EXISTS (SELECT 1 FROM $S.promos p WHERE p.sku = o.sku)
                AND NOT EXISTS (SELECT 1 FROM $S.blocked b WHERE b.id = o.customer_id)
                AND o.status IN ('paid', 'shipped')
                AND c.country IN ('NO', 'SE', 'DK')
                AND o.at >= to_timestamp({WINDOW_FROM}) AND o.at < to_timestamp({WINDOW_TO})
              GROUP BY 1, 2"
        ),
        ask: in_window(ask("paid", &PAID, &["country", "label"])),
        scales: vec![0; 10],
    };
    ShopReferences {
        paid: vec![paid, window],
        offers: vec![offers],
    }
}

/// The references of the paid view and of the offers view.
struct ShopReferences {
    paid: Vec<Reference>,
    offers: Vec<Reference>,
}

/// Every join kind answers as PostgreSQL's view does, through dim rows
/// that come, move, double and go.
#[test]
fn every_join_kind_matches_postgresql() {
    let pg = postgres();
    let mut db = Db::new(&pg, "shop", SHOP);
    let mut rng = Rng::new(31);
    fill_the_shop(&mut db, &mut rng);
    let mut paid = db.writer(PAID_SPEC);
    let mut offers = db.writer(OFFERS_SPEC);
    assert!(matches!(paid.step(), Ok(Step::Started(_))));
    assert!(matches!(offers.step(), Ok(Step::Started(_))));
    let references = shop_oracles();
    compare(&mut db, paid.app(), &references.paid).expect("ready");
    compare(&mut db, offers.app(), &references.offers).expect("ready");

    for change in shop_changes(&mut rng) {
        db.run(&change);
        applied(&mut paid);
        applied(&mut offers);
        compare_after(&mut db, paid.app(), &references.paid, &change);
        compare_after(&mut db, offers.app(), &references.offers, &change);
    }
    assert_eq!(paid.app().status().starts, 1);
    assert_eq!(offers.app().status().starts, 1);
}

/// Customer 7 is held twice, 8 not at all; tier 4 and sku 5 have no row;
/// customer 3 is blocked. Then 300 orders.
fn fill_the_shop(db: &mut Db, rng: &mut Rng) {
    db.run(
        "INSERT INTO $S.customers VALUES
           (1, 'NO', 1), (2, 'SE', 2), (3, 'NO', 3), (4, 'DK', 4), (5, 'FI', 1),
           (6, NULL, 2), (7, 'NO', 1), (7, 'SE', 3);
         INSERT INTO $S.tiers VALUES (1, 'bronze'), (2, 'silver'), (3, 'gold');
         INSERT INTO $S.promos VALUES (1, 'two for one'), (1, 'free shipping'), (2, 'sale'),
                                      (3, NULL), (4, 'sale');
         INSERT INTO $S.blocked VALUES (3, 'fraud')",
    );
    for id in 0..300 {
        db.run(&rng.order(id));
    }
}

/// Twenty new orders, then changes to every table, each one commit.
fn shop_changes(rng: &mut Rng) -> Vec<String> {
    let mut changes = Vec::new();
    for id in 300..320 {
        changes.push(rng.order(id));
    }
    let statements = [
        // A dim row moves, under an inner join and a standing WHERE.
        "UPDATE $S.customers SET country = 'SE' WHERE id = 1",
        // A key held once is held twice, then once again.
        "INSERT INTO $S.customers VALUES (2, 'DK', 3)",
        "DELETE FROM $S.customers WHERE id = 7 AND country = 'SE'",
        // The chain: the far dim changes, then the near dim's link.
        "UPDATE $S.tiers SET label = 'platinum' WHERE id = 3",
        "UPDATE $S.customers SET tier_id = 2 WHERE id = 4",
        "DELETE FROM $S.tiers WHERE id = 1",
        // Semi and anti: a promo for the sku without one, a doubled
        // promo gone whole, a customer blocked and one unblocked.
        "INSERT INTO $S.promos VALUES (5, 'late')",
        "DELETE FROM $S.promos WHERE sku = 1",
        "INSERT INTO $S.blocked VALUES (1, 'chargeback')",
        "DELETE FROM $S.blocked WHERE id = 3",
        // Fact rows move across the WHERE, the joins and out.
        "UPDATE $S.orders SET status = 'paid' WHERE id % 3 = 0",
        // The last false of a cell goes, and a true comes to be null.
        "UPDATE $S.orders SET gift = true WHERE id % 2 = 0 AND NOT gift",
        "UPDATE $S.orders SET gift = NULL WHERE id % 5 = 0 AND gift",
        "UPDATE $S.orders SET customer_id = 8 WHERE id % 7 = 0",
        "UPDATE $S.orders SET sku = 1 WHERE id % 5 = 0",
        "DELETE FROM $S.orders WHERE id % 11 = 0",
        // A dim key moves, a dim is emptied and filled again.
        "UPDATE $S.customers SET id = 8 WHERE id = 5",
        "TRUNCATE $S.tiers",
        "INSERT INTO $S.tiers VALUES (1, 'bronze'), (2, 'silver'), (2, 'silver again')",
    ];
    for statement in statements {
        changes.push(statement.to_owned());
    }
    changes
}

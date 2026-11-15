//! Every join kind, any cardinality, a join through another dim and the
//! view's `WHERE`, each against a reference that joins the live rows to
//! the dims as they are. `view.rs` builds the orders view and its store,
//! `reference.rs` answers an ask the slow, obvious way, `cases.rs` holds
//! hand-made cases, and `random.rs` runs random rows, dims and changes to
//! both, with buckets sealed under them, over each kind of cells an ask
//! may read.

#[path = "../common/mod.rs"]
mod common;

mod cases;
mod random;
mod reference;
mod view;

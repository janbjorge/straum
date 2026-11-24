//! What the store holds, as asks may read it. `App::held` reads every
//! view and dim under the store's lock into one `Held`: per view its
//! widths, keys, measures, how many cells it keeps and each width kept
//! only for a time with its horizon, per dim its rows
//! and the view keys that read it. `App::rows_ask` builds, from the spec
//! alone, the ask for every row a view holds.

use std::ops::Range;

use straum_domain::{Ask, AskKey, ColumnType, Fact, Identifier, Spec, Store, Table, Width};

use crate::app::App;

const IN_SPEC: &str = "the store holds every view and dim of its spec";

/// What [`App::held`] reports, read at one moment: every view and dim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub views: Vec<HeldView>,
    pub dims: Vec<HeldDim>,
}

/// A view as asks may read it, and how much of it the store holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldView {
    pub name: Identifier,
    /// The `date_bin` width, then the wider ones.
    pub widths: Vec<Width>,
    /// The view's time, by the name asks group and bound it by.
    pub time: HeldKey,
    /// Every name an ask may group or filter by, the time aside.
    pub keys: Vec<HeldKey>,
    pub measures: Vec<Identifier>,
    /// Keyed cells at the `date_bin` width.
    pub cells: usize,
    /// Bound cells at the `date_bin` width, for a view that keeps them.
    pub bound_cells: Option<usize>,
    /// Every bucket an ask without bounds reads, at any width, in Unix
    /// seconds; `None` for a view with no bucket.
    pub range: Option<Range<i64>>,
    /// Each width kept only for a time, narrowest first.
    pub kept: Vec<HeldKept>,
}

/// A width a view keeps only for a time, and where it keeps it from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldKept {
    pub width: Width,
    pub kept_for: Width,
    /// The start of the first bucket kept, in Unix seconds; `None`
    /// before the store is first advanced.
    pub kept_from: Option<i64>,
}

/// A key an ask may name, and the type of its values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldKey {
    pub name: Identifier,
    pub column_type: ColumnType,
}

/// A dim's table, how many rows of it the store holds, and the view keys
/// that read its columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldDim {
    pub table: Table,
    pub rows: usize,
    /// An ask never reads a dim; it reads these keys of the views instead.
    pub asked_as: Vec<ViewKey>,
}

/// A key of a view, as `view.key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewKey {
    pub view: Identifier,
    pub key: Identifier,
}

impl App {
    /// `None` until every table is loaded: only a load knows the types.
    pub fn held(&self) -> Option<Held> {
        let state = self.read();
        state.applied?;
        let store = &state.store;
        let mut views = Vec::new();
        for fact in store.spec().facts() {
            views.push(held_view(store, fact));
        }
        let mut dims = Vec::new();
        for dim in store.spec().dims() {
            dims.push(HeldDim {
                table: dim.table.clone(),
                rows: store.dim_rows(dim.name.as_str()).expect(IN_SPEC),
                asked_as: asked_as(store.spec(), &dim.name),
            });
        }
        Some(Held { views, dims })
    }

    /// The ask for every row the view holds: each measure by the time and
    /// every key, both ends of the range open.
    /// `None` for a name no view has.
    pub fn rows_ask(&self, view: &str) -> Option<Ask> {
        let fact = self.spec.fact(view)?;
        let mut keys = vec![AskKey::Key(fact.time.name.clone())];
        for key in fact.keys() {
            keys.push(AskKey::Key(key.clone()));
        }
        Some(Ask {
            view: fact.name.clone(),
            measures: measure_names(fact),
            keys,
            filter: Vec::new(),
            having: Vec::new(),
            from: None,
            to: None,
            bounded: None,
        })
    }
}

/// Each key a view reads from `dim` through a ref.
fn asked_as(spec: &Spec, dim: &Identifier) -> Vec<ViewKey> {
    let mut keys = Vec::new();
    for fact in spec.facts() {
        for join in &fact.refs {
            if join.dim != *dim {
                continue;
            }
            for attribute in &join.reads {
                keys.push(ViewKey {
                    view: fact.name.clone(),
                    key: attribute.name.clone(),
                });
            }
        }
    }
    keys
}

fn held_view(store: &Store, fact: &Fact) -> HeldView {
    let name = fact.name.as_str();
    let mut widths = vec![fact.time.width];
    widths.extend(fact.time.wider.iter().copied());
    let mut keys = Vec::new();
    for key in fact.keys() {
        keys.push(HeldKey {
            name: key.clone(),
            column_type: store.key_type(name, key).expect(IN_SPEC),
        });
    }
    HeldView {
        name: fact.name.clone(),
        widths,
        time: HeldKey {
            name: fact.time.name.clone(),
            column_type: store.time_type(name).expect(IN_SPEC),
        },
        keys,
        measures: measure_names(fact),
        cells: store.cells(name).expect(IN_SPEC),
        bound_cells: store.bound_cells(name),
        range: store.range(name),
        kept: held_kept(store, fact),
    }
}

/// Each width the view keeps only for a time, with how long the spec
/// keeps it and where the store keeps it from.
fn held_kept(store: &Store, fact: &Fact) -> Vec<HeldKept> {
    let mut kept = Vec::new();
    for horizon in store.horizons(fact.name.as_str()).expect(IN_SPEC) {
        for retention in &fact.time.retained {
            if retention.width == horizon.width {
                kept.push(HeldKept {
                    width: horizon.width,
                    kept_for: retention.kept_for,
                    kept_from: horizon.kept_from,
                });
            }
        }
    }
    kept
}

fn measure_names(fact: &Fact) -> Vec<Identifier> {
    let mut names = Vec::new();
    for measure in &fact.measures {
        names.push(measure.name.clone());
    }
    names
}

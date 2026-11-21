//! Merges each view's joins into the spec's dims. A dim is its table: the
//! first view that joins it makes it, and every later one must join on the
//! same key and spell each column alike. A view's refs come out with their
//! via columns in the dim's key order.

use straum_domain::{Dim, Identifier, Ref, Via};

use super::joins::key_position;
use super::{Cast, JoinedRef, comma_list, push_once};
use crate::sql::Walker;

impl Walker {
    /// Adds the view's joins to the dims, and returns its refs.
    pub(super) fn add_dims(
        &mut self,
        at: u64,
        dims: &mut Vec<Dim>,
        refs: Vec<JoinedRef>,
    ) -> Vec<Ref> {
        // A dim a later join reads its key from carries that column too.
        // Taken before any join's via is put in key order, so the columns
        // come in the order the view wrote them.
        let mut carried = vec![Vec::new(); refs.len()];
        for joined in &refs {
            for via in &joined.reference.via {
                if let Via::Attribute { through, column } = via {
                    carried[through.0].push(column.clone());
                }
            }
        }
        let mut fact_refs = Vec::new();
        for (index, mut joined) in refs.into_iter().enumerate() {
            self.add_dim(at, dims, &mut joined, &carried[index]);
            fact_refs.push(joined.reference);
        }
        fact_refs
    }

    /// A dim is its table. Every view that joins it must join on the same
    /// key columns, in any order, and its attributes are every column any
    /// view reads, a column another join reads its key from included. The
    /// join's via columns are put in the dim's key order, so a ref's via
    /// and its dim's key pair up by position.
    fn add_dim(
        &mut self,
        at: u64,
        dims: &mut Vec<Dim>,
        joined: &mut JoinedRef,
        carried: &[Identifier],
    ) {
        let mut found = None;
        for (index, dim) in dims.iter().enumerate() {
            if dim.table == joined.table {
                found = Some(index);
                break;
            }
        }
        let Some(index) = found else {
            let mut dim = new_dim(joined);
            add_attributes(&mut dim, carried);
            dims.push(dim);
            return;
        };
        let dim = &mut dims[index];
        if !same_columns(&dim.key, &joined.key) {
            self.error(
                at,
                format!(
                    "{}.{} is joined on {} here and on {} elsewhere",
                    joined.table.schema,
                    joined.table.name,
                    comma_list(&joined.key),
                    comma_list(&dim.key)
                ),
            );
            return;
        }
        joined.reference.via = via_in_key_order(&dim.key, joined);
        joined.key.clone_from(&dim.key);
        self.merge_reads(at, dim, joined);
        add_attributes(dim, carried);
    }

    /// Adds the columns this join reads to the dim. A column is one
    /// spelling in every view, since it is one type.
    fn merge_reads(&mut self, at: u64, dim: &mut Dim, joined: &JoinedRef) {
        for attribute in &joined.reference.reads {
            let column = &attribute.column;
            let cast = if joined.as_text.contains(column) {
                Cast::AsText
            } else {
                Cast::Plain
            };
            let read_before = dim.attributes.contains(column);
            let cast_before = dim.as_text.contains(column);
            if read_before && (cast == Cast::AsText) != cast_before {
                let clash = match cast {
                    Cast::AsText => "::text here and without it",
                    Cast::Plain => "without ::text here and with it",
                };
                self.error(
                    at,
                    format!(
                        "{}.{}.{column} is read {clash} in another view; write it alike",
                        joined.table.schema, joined.table.name
                    ),
                );
            }
            if !read_before {
                dim.attributes.push(column.clone());
            }
            if cast == Cast::AsText && !cast_before {
                dim.as_text.push(column.clone());
            }
        }
    }
}

/// The dim the first view to join a table makes of it.
fn new_dim(joined: &JoinedRef) -> Dim {
    let mut attributes = Vec::new();
    for attribute in &joined.reference.reads {
        push_once(&mut attributes, &attribute.column);
    }
    Dim {
        name: joined.reference.dim.clone(),
        table: joined.table.clone(),
        key: joined.key.clone(),
        attributes,
        as_text: joined.as_text.clone(),
    }
}

fn add_attributes(dim: &mut Dim, columns: &[Identifier]) {
    for column in columns {
        push_once(&mut dim.attributes, column);
    }
}

/// The same columns, in any order.
fn same_columns(first: &[Identifier], second: &[Identifier]) -> bool {
    if first.len() != second.len() {
        return false;
    }
    for column in first {
        if !second.contains(column) {
            return false;
        }
    }
    true
}

/// The join's via columns, reordered to pair with the dim's key.
fn via_in_key_order(key: &[Identifier], joined: &JoinedRef) -> Vec<Via> {
    let mut via = Vec::new();
    for column in key {
        if let Some(position) = key_position(joined, column) {
            via.push(joined.reference.via[position].clone());
        }
    }
    via
}

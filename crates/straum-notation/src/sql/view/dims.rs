use straum_domain::{Dim, Identifier, Ref, Via};

use super::{JoinedRef, comma_list};
use crate::sql::Walker;

impl Walker {
    pub(super) fn add_dims(
        &mut self,
        at: u64,
        dims: &mut Vec<Dim>,
        refs: Vec<JoinedRef>,
    ) -> Vec<Ref> {
        // A dim a later join reads its key from carries that column too.
        let mut carried: Vec<(usize, Identifier)> = Vec::new();
        for joined in &refs {
            for via in &joined.reference.via {
                if let Via::Attribute { through, column } = via {
                    carried.push((*through, column.clone()));
                }
            }
        }
        let mut fact_refs = Vec::new();
        for (index, mut joined) in refs.into_iter().enumerate() {
            if let Some(dim) = self.add_dim(at, dims, &mut joined) {
                for (through, column) in &carried {
                    if *through == index && !dim.attributes.contains(column) {
                        dim.attributes.push(column.clone());
                    }
                }
            }
            fact_refs.push(joined.reference);
        }
        fact_refs
    }

    /// A dim is its table. Every view that joins it must join on the same
    /// key columns, in any order, and its attributes are every column any
    /// view reads, a column another join reads its key from included. The
    /// join's via columns are put in the dim's key order, so a ref's via
    /// and its dim's key pair up by position.
    pub(super) fn add_dim<'d>(
        &mut self,
        at: u64,
        dims: &'d mut Vec<Dim>,
        joined: &mut JoinedRef,
    ) -> Option<&'d mut Dim> {
        let Some(index) = dims.iter().position(|dim| dim.table == joined.table) else {
            dims.push(Dim {
                name: joined.reference.dim.clone(),
                table: joined.table.clone(),
                key: joined.key.clone(),
                attributes: (joined.reference.reads.iter())
                    .map(|attribute| attribute.column.clone())
                    .collect(),
                as_text: joined.as_text.clone(),
            });
            return dims.last_mut();
        };
        let dim = &mut dims[index];
        let same = dim.key.len() == joined.key.len()
            && dim.key.iter().all(|column| joined.key.contains(column));
        if !same {
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
            return None;
        }
        let mut via = Vec::new();
        for column in &dim.key {
            let position = (joined.key.iter())
                .position(|joined_column| joined_column == column)
                .expect("the key sets are equal");
            via.push(joined.reference.via[position].clone());
        }
        joined.reference.via = via;
        joined.key.clone_from(&dim.key);
        // A column is one spelling in every view, since it is one type.
        for attribute in &joined.reference.reads {
            let column = &attribute.column;
            let cast = joined.as_text.contains(column);
            let read_before = dim.attributes.contains(column);
            let cast_before = dim.as_text.contains(column);
            if read_before && cast != cast_before {
                let (here, there) = if cast {
                    ("::text", "without it")
                } else {
                    ("without ::text", "with it")
                };
                self.error(
                    at,
                    format!(
                        "{}.{}.{column} is read {here} here and {there} in another view; \
                         write it alike",
                        joined.table.schema, joined.table.name
                    ),
                );
            }
            if !read_before {
                dim.attributes.push(column.clone());
            }
            if cast && !cast_before {
                dim.as_text.push(column.clone());
            }
        }
        Some(dim)
    }
}

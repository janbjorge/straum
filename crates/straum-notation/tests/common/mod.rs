use straum_domain::{Cond, Ident, Literal};

// 2024-01-01 and 2024-02-01, 00:00 UTC.
pub const JAN: i64 = 1_704_067_200;
pub const FEB: i64 = 1_706_745_600;

pub fn id(s: &str) -> Ident {
    Ident::new(s).unwrap()
}

pub fn cond(key: &str, values: Vec<Literal>) -> Cond {
    Cond {
        key: id(key),
        values,
    }
}

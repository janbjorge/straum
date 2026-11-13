//! Straum's feed adapter: the [`straum_app::Source`] over a Unix socket.
//!
//! [`Feed`] listens. A feeder, a program that holds the rows, connects
//! with [`Feeder`] and says hello; from there Straum drives. It asks for
//! each spec table by name with the columns the spec reads, and the feeder
//! answers with their types, the rows in batches and an end. After the
//! last table Straum says it is done and closes. Either side may say why
//! it fails before it closes. Nothing follows a load: a feed carries no
//! changes.
//!
//! The socket has no authentication. Whoever may connect gives Straum
//! every row it holds, so the socket's file is the owner's alone.

#![forbid(unsafe_code)]

mod error;
mod feeder;
mod frame;
mod source;

pub use error::Error;
pub use feeder::{Feeder, Request};
pub use source::Feed;

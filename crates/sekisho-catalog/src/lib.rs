//! Sekisho's catalog rules: what a sync, `sekisho check` and the API
//! accept. Nothing here touches the network or the database.

pub mod card;
pub mod check;
pub mod labels;
pub mod model;
pub mod names;
mod obot;
pub mod pins;
pub mod sekisho_format;
pub mod source;
pub mod tree;
pub mod unicode;

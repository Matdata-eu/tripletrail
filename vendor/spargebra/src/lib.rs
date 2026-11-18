#![cfg_attr(doc, doc = include_str!("../README.md"))]
#![doc(test(attr(deny(warnings))))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(html_favicon_url = "https://raw.githubusercontent.com/oxigraph/oxigraph/main/logo.svg")]
#![doc(html_logo_url = "https://raw.githubusercontent.com/oxigraph/oxigraph/main/logo.svg")]

pub mod algebra;
mod parser;
mod query;
pub mod term;
mod update;

pub use parser::{SparqlParser, SparqlSyntaxError};
pub use query::*;
pub use update::*;

/// tripletrail: the variables of the `INSERT … WHERE` that `COPY`, `MOVE` and `ADD` become
/// (#782). No query can name them (`-` is not allowed in a variable name), so an engine can
/// tell these operations from an update written the same way.
pub const COPY_VARIABLES: [&str; 3] = ["copy-s", "copy-p", "copy-o"];

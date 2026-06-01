//! Output rendering: color decisions, paging, table layout, and the human
//! renderers. Submodules are added as the command surface grows; the
//! stdout/stderr discipline itself lives on [`crate::cx::Cx`].

pub mod color;
pub mod json;
pub mod pager;
pub mod render;
pub mod table;

mod diff;
mod repository;
mod source;

pub use diff::{get_diff, hunk_line_targets};
pub use repository::{ChangedFile, FileStatus, LineStat, Repository};
pub use source::{Patch, Source};

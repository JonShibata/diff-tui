mod diff;
mod repository;

pub use diff::{get_diff, hunk_line_targets};
pub use repository::{ChangedFile, FileStatus, Repository};

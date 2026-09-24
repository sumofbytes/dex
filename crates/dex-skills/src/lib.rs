//! SKILL.md discovery and frontmatter parsing.
//!
//! A skill is a directory containing a `SKILL.md` with YAML frontmatter
//! (`name`, `description`). Discovery scans an ordered list of directories,
//! sorts by name, and keeps the first directory's entry on duplicate names
//! (duplicates warn to stderr). Sync and async paths share the frontmatter
//! rules so they cannot drift.

mod discovery;
mod parse;

pub use discovery::{
    discover_skills, discover_skills_async, discover_skills_fresh, discover_skills_fresh_async,
    skill_dirs,
};
pub use parse::unquote;

/// One discovered skill: its declared name and description plus the
/// `SKILL.md` path (the body is read on demand).
#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: std::path::PathBuf,
}

//! Patched `lru` — see the package comment in `Cargo.toml`.
//!
//! ratatui 0.29 requires `lru = "^0.12"`, whose `IterMut` violates Stacked
//! Borrows (GHSA-rhfx-m35p-ff5j, fixed upstream in 0.16.3). No fixed 0.12.x
//! was released, so this shim satisfies the `^0.12` requirement while
//! re-exporting the patched upstream implementation. ratatui only uses
//! `LruCache::new` and `LruCache::get_or_insert`, both unchanged since 0.12.

pub use lru_upstream::*;

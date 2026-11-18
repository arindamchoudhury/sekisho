//! Catalog syncs (plan 3): Sekisho reads each source's Git repository with
//! the server's `git`, hardened (D165), checks the folder with
//! `sekisho-catalog`, pins container images to digests and A2A agents'
//! cards (D163), and records the result in the store.
//!
//! Everything Sekisho fetches itself is address-checked each time and
//! connected to the checked address, without redirects (D178).

pub mod cards;
pub mod git;
pub mod git_mode;
pub mod github;
pub mod outbound;
pub mod registry;
pub mod sync;

use std::path::PathBuf;
use std::time::Duration;

/// How sources are synced (`SEKISHO_CATALOG_SYNC_INTERVAL`,
/// `SEKISHO_E2E_PLAIN_HTTP`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogSettings {
    /// How long after a sync the next one is due; Git mode's source is
    /// checked every minute regardless.
    pub sync_every: Duration,
    /// Lets sources, registries and agents be reached over plain HTTP:
    /// tests and the e2e build only.
    pub allow_plain_http: bool,
    /// The `git` to run.
    pub git_program: PathBuf,
}

impl Default for CatalogSettings {
    fn default() -> Self {
        Self {
            sync_every: Duration::from_secs(60 * 60),
            allow_plain_http: false,
            git_program: PathBuf::from("git"),
        }
    }
}

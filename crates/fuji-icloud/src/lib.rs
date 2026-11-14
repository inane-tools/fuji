//! Access to an iCloud Photos library.
//!
//! Apple has no public API for iCloud Photos, so this uses the private web
//! endpoints that icloud.com uses: Apple ID SRP sign-in and two-factor
//! authentication ([`auth`]), then CloudKit queries (not written yet). Until
//! the photo queries exist, [`MockLibrary`] provides fake assets for the UI.

mod asset;
pub mod auth;
mod mock;
pub mod srp;

pub use asset::{Asset, AssetId, AssetKind};
pub use auth::{Account, AuthClient, AuthError, SavedSession, SignInStep, TrustedPhone};
pub use mock::MockLibrary;

/// A source of photos and videos, newest first.
///
/// Implemented by [`MockLibrary`] now, and by the iCloud client later.
pub trait Library: Send + Sync {
    /// Total number of assets in the library.
    fn len(&self) -> usize;

    /// Whether the library has no assets.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The asset at `index` in timeline order (0 is the newest).
    fn asset(&self, index: usize) -> Option<Asset>;
}

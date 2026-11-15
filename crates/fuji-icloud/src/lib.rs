//! Access to an iCloud Photos library.
//!
//! Apple has no public API for iCloud Photos, so this uses the private web
//! endpoints that icloud.com uses: Apple ID SRP sign-in and two-factor
//! authentication ([`auth`]), then CloudKit queries ([`photos`]).

mod asset;
pub mod auth;
mod cookies;
pub mod photos;
mod profile;
pub mod srp;

pub use asset::{Asset, AssetId, AssetKind, Resource};
pub use auth::{Account, AuthClient, AuthError, SavedSession, SignInStep, TrustedPhone};
pub use photos::{Album, PhotosClient, PhotosError, UserAlbum};
pub use profile::account_picture;

//! Access to an iCloud Photos library.
//!
//! Apple has no public API for iCloud Photos, so this uses the private web
//! endpoints that icloud.com uses: Apple ID SRP sign-in and two-factor
//! authentication ([`auth`]), then CloudKit queries ([`photos`]).

mod asset;
pub mod auth;
pub mod bridge;
mod cookies;
pub mod details;
pub mod ids;
pub mod memories;
pub mod ngm;
pub mod passkey;
pub mod photos;
mod profile;
pub mod push;
pub mod skybridge;
pub mod spake;
pub mod srp;

pub use asset::{Asset, AssetId, AssetKind, Resource};
pub use auth::{
    Account, AuthClient, AuthError, BridgeConfig, SavedSession, SignInStep, TrustedPhone,
};
pub use bridge::BridgeApproval;
pub use details::{Location, PhotoDetails};
pub use memories::Memory;
pub use passkey::{PairingKey, PasskeyAssertion, PasskeyChallenge};
pub use photos::{
    Album, LibraryChanges, PhotosClient, PhotosError, SharedAlbum, SmartAlbum, UserAlbum,
};
pub use profile::account_picture;

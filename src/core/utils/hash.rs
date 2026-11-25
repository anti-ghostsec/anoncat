//! Password hashing and cryptographic digest utilities.
//!
//! Password helpers use Argon2id with a fresh random salt for each new hash.
//! The SHA-256 submodule provides byte-oriented digest helpers.

mod argon;

pub mod sha256;

pub use self::argon::{Cost, PhcString, is_phc_hash, password, verify_password};

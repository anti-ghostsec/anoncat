//! Handles media thumbnail requests.
//!
//! Request routing, source admission, dimensions, still generation, animation
//! policy, and animated encoding live in focused units below.

mod animate;
mod dimension;
#[cfg(feature = "media_thumbnail")]
mod encode;
#[cfg(feature = "media_thumbnail")]
mod generate;
#[cfg(feature = "media_thumbnail")]
mod pdq;
mod inspect;
mod request;
mod sniff;
#[cfg(all(test, feature = "media_thumbnail"))]
mod tests;

#[cfg(test)]
pub(super) use animate::ANIMATED_TYPES;
pub use animate::Animate;
pub use dimension::Dim;
pub use inspect::{Fingerprint, Inspected, pdq_distance};
#[cfg(all(test, feature = "media_thumbnail"))]
pub(super) use encode::encode_frames;
#[cfg(all(test, feature = "media_thumbnail"))]
pub(super) use generate::thumbnail_generate;
pub(super) use sniff::sequence;
#[cfg(test)]
pub(super) use sniff::{animated_type, animates};

//! Markers that fix a public type's auto traits ahead of the fields that
//! will need them.

use std::marker::PhantomData;

/// Opts the type holding it out of `UnwindSafe` and `RefUnwindSafe`, and
/// keeps it `Send + Sync + Unpin`.
///
/// The transport (#3) puts hyper's `Incoming` in `Body` and a pooled
/// hyper-util client in `Client`. Both hold shared state that is neither
/// `UnwindSafe` nor `RefUnwindSafe`, and removing an auto trait from a
/// public type is a breaking change, so the types opt out before that state
/// arrives. Adding the traits back later is not breaking.
#[derive(Clone, Copy, Default)]
pub(crate) struct NotUnwindSafe(PhantomData<Box<dyn Send + Sync>>);

//! Cordis, the meta-framework, in Rust.
//!
//! A port of [Cordis](https://github.com/cordiverse/cordis) 4.0.0-rc.8's core: contexts and
//! their services, a registry of plugins and their fibers, effects and their disposal, and
//! events — with the same lifecycle, ordering and error semantics, in a form any host
//! language can plug its values into ([`Host`]). [`native`] is the host for Rust itself;
//! `cordis-verve` is the one for JavaScript on Verve.

pub mod effect;
pub mod events;
pub mod fiber;
pub mod host;
pub mod list;
pub mod native;
pub mod reflect;
pub mod root;
pub mod stats;
pub mod task;

pub use effect::{Disposer, Effect, Pending};
pub use events::{Filter, OnOptions};
pub use fiber::{Epoch, Fiber, FiberState, PluginSpec, WeakFiber};
pub use host::{AsyncEffectIter, EffectIter, EffectValue, ErrorKind, Host, NativeFn};
pub use list::DisposableList;
pub use reflect::Lookup;
pub use root::{Callback, Hook, Impl, Prop, Root, RootInner, Runtime};
pub use task::{Deferred, LocalBoxFuture, Scope, Task};

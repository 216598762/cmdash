//! cmdash binary; event loop and crate glue.
//!
//! Every graphics output routes through [`dashcompositor`]. See
//! `AGENTS.md` §"Hard rule: one layer per instance" for the layer
//! invariant this crate upholds.
//!
//! Scaffolding stub.

#[doc(hidden)]
pub fn _cmdash_dep_smoke() {
    // Force the workspace `dashcompositor` dep to compile against the
    // declared feature flags so feature-flag typos surface during
    // `cargo check` rather than at later stages.
    let _ = core::mem::size_of::<dashcompositor::FrameBuffer>();
}

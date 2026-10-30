//! Higher-level operations built on the raw API client: provisioning,
//! placement, rebuilds, IP assignment, the OS catalog and diagnostics.

pub mod bandwidth;
pub mod catalog;
pub mod create;
pub mod diagnostics;
pub mod drift;
pub mod export;
pub mod guest;
pub mod ipassign;
pub mod limits;
pub mod placement;
pub mod rebuild;
pub mod tenant;

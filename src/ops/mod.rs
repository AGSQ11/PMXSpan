//! Higher-level operations built on the raw API client. These hold the
//! provisioning logic that used to live in the HostBill module's feature traits.

pub mod bandwidth;
pub mod create;
pub mod diagnostics;
pub mod guest;
pub mod placement;
pub mod rebuild;
pub mod tenant;

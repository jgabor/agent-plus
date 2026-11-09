//! Every check that needs `/dev/kvm` lives in this package, so that T0 can
//! leave them out.

pub mod guest_binary;
pub mod kernel;

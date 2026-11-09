//! The virtual machine monitor: KVM through hand-declared ioctls, with no
//! dependencies.

pub mod boot;
pub mod initramfs;
pub mod serial;
pub mod virtio;
pub mod vsock;

mod kvm;
mod memory;
mod sys;

pub use kvm::{Dtable, Ports, Regs, Segment, Sregs, Stop, Vcpu, Vm};
pub use memory::{GuestMemory, OutOfBounds};

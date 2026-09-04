pub mod after;
pub mod backend;
pub mod control;
pub mod events;
pub mod hosts;
pub mod logring;
/// The Linux backend and everything only it may use: namespaces, mounts,
/// loop devices, cgroups, capabilities, seccomp, the bridge, `ply exec`.
#[cfg(target_os = "linux")]
pub mod ns;
pub mod params_tree;
pub mod publish;
pub mod run;
pub mod state;
pub mod supervise;
/// The macOS backend and everything only it may use: Hypervisor.framework,
/// the device models, the guest contract, the userspace switch. Its portable
/// submodules compile everywhere so Linux CI tests them.
pub mod vm;

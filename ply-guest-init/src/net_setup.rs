//! Bringing the guest's one network interface up, with no userspace tools.
//!
//! There is no `ip`, no `ifconfig` and no DHCP client in this initramfs, and
//! there is nothing to ask for an address anyway: the parent decided this
//! instance's address before the machine booted and put it on the spec disk
//! (`ply_vm_proto::NetSpec`). So the init talks to the kernel directly —
//! `SIOCSIFADDR`/`SIOCSIFNETMASK`/`SIOCSIFFLAGS` for the interface, one raw
//! netlink `RTM_NEWROUTE` for the default route.
//!
//! Ported from the plyvm spike's `guest-init/src/net_setup.rs`, with its
//! hardcoded `10.0.2.x` replaced by the spec disk's values, its `unwrap`s
//! removed (a panic in PID 1 aborts with no diagnosis), and the interface
//! found rather than assumed.
//!
//! The decisions are pure functions with tests that run on any host; only
//! the syscalls below are Linux-only.

/// The netmask for a prefix length: `/16` → `255.255.0.0`.
///
/// Saturating rather than wrapping at both ends, because a bad prefix on the
/// spec disk must produce a usable interface and not a panic inside a VM:
/// `/0` is "no netmask at all" and `>= 32` is a host route.
pub fn netmask(prefix_len: u8) -> [u8; 4] {
    let bits = prefix_len.min(32) as u32;
    let mask: u32 = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    mask.to_be_bytes()
}

/// The interface to configure, given what is in `/sys/class/net`.
///
/// The kernel calls the first Ethernet device `eth0` and this guest has
/// exactly one, so `eth0` is the answer every time — but it is *found*
/// rather than assumed, because "the interface is missing" and "the
/// interface has a name I did not expect" produce the same silent
/// no-network guest, and only one of them is worth a different message.
/// `lo` is never it: it is always present and configuring it as the
/// instance's address would make every peer connection go nowhere.
pub fn pick_interface(names: &[String]) -> Option<&str> {
    names
        .iter()
        .map(String::as_str)
        .find(|name| *name == "eth0")
        .or_else(|| {
            names
                .iter()
                .map(String::as_str)
                .find(|name| *name != "lo" && !name.is_empty())
        })
}

#[cfg(target_os = "linux")]
pub use imp::{bring_up, interfaces, wait_for_interface};

#[cfg(target_os = "linux")]
mod imp {
    use std::mem::{size_of, zeroed};

    use libc::{
        c_char, c_int, c_short, c_void, close, in_addr, ioctl, nlmsghdr, sockaddr_in, socket,
        Ioctl, AF_INET, AF_NETLINK, IFF_RUNNING, IFF_UP, NETLINK_ROUTE, NLM_F_ACK, NLM_F_CREATE,
        NLM_F_EXCL, NLM_F_REQUEST, RTA_GATEWAY, RTM_NEWROUTE, RTN_UNICAST, RTPROT_BOOT,
        RT_SCOPE_UNIVERSE, RT_TABLE_MAIN, SOCK_DGRAM, SOCK_RAW,
    };

    // <linux/sockios.h>. Numeric because libc does not export them for
    // every target, and they are architecture-independent constants.
    //
    // Typed `libc::Ioctl`, not `c_int`: that alias is `c_ulong` against
    // glibc and `c_int` against musl, and this file is compiled for both —
    // the guest is a static musl binary, and Linux CI builds it for the
    // host's gnu target to run its tests.
    const SIOCSIFADDR: Ioctl = 0x8916;
    const SIOCSIFNETMASK: Ioctl = 0x891C;
    const SIOCSIFFLAGS: Ioctl = 0x8914;
    const SIOCGIFFLAGS: Ioctl = 0x8913;

    /// `struct ifreq` with an address in the union. The padding makes the
    /// whole struct the 40 bytes the kernel expects; a short one is an
    /// `EFAULT` at best and a read of this process's stack at worst.
    #[repr(C)]
    struct IfreqAddr {
        name: [c_char; 16],
        addr: sockaddr_in,
        _pad: [u8; 8],
    }

    #[repr(C)]
    struct IfreqFlags {
        name: [c_char; 16],
        flags: c_short,
        _pad: [u8; 22],
    }

    fn name16(name: &str) -> [c_char; 16] {
        let mut out = [0 as c_char; 16];
        // 15 bytes plus the NUL: IFNAMSIZ. A longer name is truncated
        // rather than overflowing, and the ioctl then fails by name.
        for (slot, byte) in out.iter_mut().take(15).zip(name.bytes()) {
            *slot = byte as c_char;
        }
        out
    }

    fn sockaddr(ip: [u8; 4]) -> sockaddr_in {
        // SAFETY: `sockaddr_in` is a plain C struct; an all-zero one is the
        // documented starting point for filling it in.
        let mut addr: sockaddr_in = unsafe { zeroed() };
        addr.sin_family = AF_INET as u16;
        addr.sin_addr = in_addr {
            s_addr: u32::from_ne_bytes(ip),
        };
        addr
    }

    /// Interface names the kernel currently has.
    pub fn interfaces() -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir("/sys/class/net")
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    /// Wait briefly for a network interface other than `lo` to appear.
    ///
    /// virtio-mmio devices are probed during kernel init with built-in
    /// drivers, so the NIC is normally there before init runs — but a device
    /// that is merely late must not read as a device that is missing, which
    /// is the same rule `wait_for_first_disk` follows for the disks.
    pub fn wait_for_interface() -> Option<String> {
        for _ in 0..40 {
            let names = interfaces();
            if let Some(found) = super::pick_interface(&names) {
                return Some(found.to_string());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        None
    }

    /// Give `name` this address, bring it and `lo` up, and route everything
    /// else through `gateway`.
    pub fn bring_up(
        name: &str,
        ip: [u8; 4],
        prefix_len: u8,
        gateway: [u8; 4],
    ) -> Result<(), String> {
        let last = || std::io::Error::last_os_error();
        // SAFETY: every call below is an ioctl on a socket this function
        // opened, with a correctly sized `struct ifreq`.
        unsafe {
            let fd = socket(AF_INET, SOCK_DGRAM, 0);
            if fd < 0 {
                return Err(format!("socket(AF_INET): {}", last()));
            }
            let mut request = IfreqAddr {
                name: name16(name),
                addr: sockaddr(ip),
                _pad: [0; 8],
            };
            if ioctl(fd, SIOCSIFADDR, &mut request) < 0 {
                let e = last();
                close(fd);
                return Err(format!("set {name} address: {e}"));
            }
            request.addr = sockaddr(super::netmask(prefix_len));
            if ioctl(fd, SIOCSIFNETMASK, &mut request) < 0 {
                let e = last();
                close(fd);
                return Err(format!("set {name} netmask: {e}"));
            }
            if let Err(e) = up(fd, name) {
                close(fd);
                return Err(e);
            }
            // Loopback too: an app that binds 127.0.0.1 — or resolves its
            // own hostname, which `/etc/hosts` points at loopback — finds
            // nothing at all without it.
            let _ = up(fd, "lo");
            close(fd);
            add_default_route(gateway)
        }
    }

    /// `flags |= IFF_UP | IFF_RUNNING`, read-modify-write so nothing the
    /// kernel already set is cleared.
    unsafe fn up(fd: c_int, name: &str) -> Result<(), String> {
        let mut flags = IfreqFlags {
            name: name16(name),
            flags: 0,
            _pad: [0; 22],
        };
        if ioctl(fd, SIOCGIFFLAGS, &mut flags) < 0 {
            return Err(format!(
                "get {name} flags: {}",
                std::io::Error::last_os_error()
            ));
        }
        flags.flags |= (IFF_UP | IFF_RUNNING) as c_short;
        if ioctl(fd, SIOCSIFFLAGS, &mut flags) < 0 {
            return Err(format!(
                "bring {name} up: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    #[repr(C)]
    #[derive(Default)]
    struct RtMsg {
        rtm_family: u8,
        /// 0 — the default route's prefix length, which is what makes this
        /// `0.0.0.0/0` and not a host route to the gateway.
        rtm_dst_len: u8,
        rtm_src_len: u8,
        rtm_tos: u8,
        rtm_table: u8,
        rtm_protocol: u8,
        rtm_scope: u8,
        rtm_type: u8,
        rtm_flags: u32,
    }

    /// `default via <gateway>`, as one netlink message.
    ///
    /// No output interface is named: the gateway is inside the prefix the
    /// interface just got, so the kernel resolves it on-link and picks the
    /// interface itself. That is also the check — if the address above did
    /// not take, this fails with `ENETUNREACH` instead of installing a route
    /// to nowhere.
    unsafe fn add_default_route(gateway: [u8; 4]) -> Result<(), String> {
        #[repr(C)]
        struct Request {
            header: nlmsghdr,
            route: RtMsg,
            /// One `RTA_GATEWAY` attribute: length, type, four bytes.
            attrs: [u8; 8],
        }

        let fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
        if fd < 0 {
            return Err(format!(
                "socket(AF_NETLINK): {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut request: Request = zeroed();
        let len = size_of::<nlmsghdr>() + size_of::<RtMsg>() + 8;
        request.header.nlmsg_len = len as u32;
        request.header.nlmsg_type = RTM_NEWROUTE;
        request.header.nlmsg_flags = (NLM_F_REQUEST | NLM_F_CREATE | NLM_F_EXCL | NLM_F_ACK) as u16;
        request.route.rtm_family = AF_INET as u8;
        request.route.rtm_table = RT_TABLE_MAIN;
        request.route.rtm_protocol = RTPROT_BOOT;
        request.route.rtm_scope = RT_SCOPE_UNIVERSE;
        request.route.rtm_type = RTN_UNICAST;
        request.attrs[0] = 8;
        request.attrs[1] = 0;
        request.attrs[2] = RTA_GATEWAY as u8;
        request.attrs[3] = 0;
        request.attrs[4..8].copy_from_slice(&gateway);
        let sent = libc::send(fd, (&raw const request).cast::<c_void>(), len, 0);
        if sent < 0 {
            let e = std::io::Error::last_os_error();
            close(fd);
            return Err(format!("send RTM_NEWROUTE: {e}"));
        }
        // Read the ACK the request asked for, so a refused route is
        // reported here rather than discovered as "the network does not
        // work" by the app.
        let mut buf = [0u8; 256];
        let n = libc::recv(fd, buf.as_mut_ptr().cast::<c_void>(), buf.len(), 0);
        close(fd);
        if n < size_of::<nlmsghdr>() as isize {
            return Ok(()); // no answer to read; the route was almost certainly taken
        }
        // NLMSG_ERROR is type 2, and its payload leads with a negative errno
        // (0 means "this is an ACK").
        let kind = u16::from_ne_bytes([buf[4], buf[5]]);
        if kind != 2 {
            return Ok(());
        }
        let at = size_of::<nlmsghdr>();
        if n < (at + 4) as isize {
            return Ok(());
        }
        let code = i32::from_ne_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
        if code == 0 {
            return Ok(());
        }
        Err(format!(
            "the kernel refused the default route via {}.{}.{}.{}: {}",
            gateway[0],
            gateway[1],
            gateway[2],
            gateway[3],
            std::io::Error::from_raw_os_error(-code)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_length_becomes_the_netmask_the_kernel_wants() {
        // /16 is the switch's own prefix, and the one every instance gets.
        assert_eq!(netmask(16), [255, 255, 0, 0]);
        assert_eq!(netmask(24), [255, 255, 255, 0]);
        assert_eq!(netmask(8), [255, 0, 0, 0]);
        assert_eq!(netmask(32), [255, 255, 255, 255]);
        // A shift of 32 is undefined in C and a panic in debug Rust; both
        // ends have to be answers, because this value came off a disk.
        assert_eq!(netmask(0), [0, 0, 0, 0]);
        assert_eq!(netmask(255), [255, 255, 255, 255]);
    }

    #[test]
    fn the_interface_is_the_one_that_is_not_loopback() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(pick_interface(&names(&["eth0", "lo"])), Some("eth0"));
        assert_eq!(pick_interface(&names(&["lo", "eth0"])), Some("eth0"));
        // A kernel that named it something else must still work: the guest
        // has exactly one card, and refusing to configure it because of its
        // name would be a network outage with no message.
        assert_eq!(pick_interface(&names(&["enp0s1", "lo"])), Some("enp0s1"));
        // Loopback alone is not a network: the NIC has not appeared yet.
        assert_eq!(pick_interface(&names(&["lo"])), None);
        assert_eq!(pick_interface(&[]), None);
    }
}

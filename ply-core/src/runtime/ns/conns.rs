//! Open connections on an instance's port, read from the instance's own
//! `/proc/<pid>/net/tcp` — its network namespace's socket table, which the
//! parent can read without entering it. In kernel-publish mode the relay
//! never sees a DNATed connection, so this is the sleeper's only view of
//! what is in flight.

use std::path::Path;

/// ESTABLISHED sockets whose local port is `port`, over `tcp` and `tcp6` of
/// the process `pid` (the instance's init). `None` when neither table can be
/// read: a missed sample, not a zero.
pub fn established(pid: i32, port: u16) -> Option<u64> {
    let mut total = None;
    for table in ["tcp", "tcp6"] {
        if let Ok(text) = std::fs::read_to_string(Path::new(&format!("/proc/{pid}/net/{table}"))) {
            *total.get_or_insert(0) += count_established(&text, port);
        }
    }
    total
}

/// The `st` column is `01` for ESTABLISHED; the local address column is
/// `hexaddr:hexport`, the address 8 hex digits for v4 and 32 for v6.
pub fn count_established(text: &str, port: u16) -> u64 {
    text.lines()
        .skip(1)
        .filter(|line| {
            let mut cols = line.split_whitespace();
            let (Some(_sl), Some(local), Some(_remote), Some(state)) =
                (cols.next(), cols.next(), cols.next(), cols.next())
            else {
                return false;
            };
            state == "01"
                && local
                    .rsplit_once(':')
                    .and_then(|(_, p)| u16::from_str_radix(p, 16).ok())
                    == Some(port)
        })
        .count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two established on 8080 (one v4, one v6), one on 8080 still in
    /// SYN_RECV, one established elsewhere, a listener: 2.
    #[test]
    fn only_established_sockets_on_the_port_count() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000000000000000 100 0 0 10 0\n\
   1: 0200500A:1F90 0100500A:C1A4 01 00000000:00000000 00:00000000 00000000     0        0 12346 1 0000000000000000 20 4 30 10 -1\n\
   2: 0200500A:1F90 0100500A:C1A5 03 00000000:00000000 00:00000000 00000000     0        0 12347 1 0000000000000000 20 4 30 10 -1\n\
   3: 0200500A:0016 0100500A:C1A6 01 00000000:00000000 00:00000000 00000000     0        0 12348 1 0000000000000000 20 4 30 10 -1\n";
        assert_eq!(count_established(tcp, 8080), 1);
        assert_eq!(count_established(tcp, 22), 1);
        assert_eq!(count_established(tcp, 9), 0);
        let tcp6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0000000000000000 100 0 0 10 0\n\
   1: 0000000000000000FFFF00000A50000A:1F90 0000000000000000FFFF00000A50000B:D2F0 01 00000000:00000000 00:00000000 00000000     0        0 2 1 0000000000000000 20 4 30 10 -1\n";
        assert_eq!(count_established(tcp6, 8080), 1);
        assert_eq!(count_established("", 8080), 0);
        assert_eq!(count_established("header only\n", 8080), 0);
    }
}

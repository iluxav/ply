# Kernel path for `--publish` — design

**Why.** `bench/RESULTS-2026-09-05.md`: the run parent relays every published
byte in user space (`publish.rs::relay`, two threads per connection) and
that costs ~3 cores at 430k rps — 0.62–0.72× Docker on published paths,
while direct-to-instance is equal. Docker's default port publishing is
kernel DNAT. ply has the same information Docker has (it owns the pool), so
it can hand the same job to the kernel.

## Scope

Rootful Linux, namespace backend, no `--network`, `nft` present. Everything
else keeps the relay unchanged: rootless (no netfilter), the macOS VM
backend (instances behind the userspace switch), and traffic to `127.0.0.1`
on the host (DNAT from loopback needs `route_localnet` tricks; Docker uses
its proxy there too). The parent still binds the port: that reserves it,
detects conflicts, and is where traffic lands when the pool is empty — the
relay's "no backend, EOF" behaviour survives untouched.

## Mechanism

One nftables **base chain pair per published port**, in the existing
`ip ply` table, named `pub_<port>_p<parent pid>_pre` (hook prerouting,
priority dstnat) and `..._out` (hook output, priority dstnat). Each holds
one rule:

```
<match> ip protocol tcp dnat ip addr . port to numgen inc mod N map { 0 : A . P, 1 : B . P, … }
```

one backend → `<match> dnat to A:P`; zero backends → no rule (falls through
to the listener). `<match>` by scope:

| scope | match |
|---|---|
| public | `ip daddr != 127.0.0.0/8 fib daddr type local tcp dport H` |
| internal | `ip daddr 10.77.0.1 tcp dport H` |
| addr A (not loopback) | `ip daddr A tcp dport H` |
| addr 127.x | no kernel path — relay |

Pool change (launch, death, roll) → one `nft -f -` batch: `flush chain`
+ `add rule` for both chains — atomic, and conntrack keeps existing flows
on their old backend. Round-robin per new connection, like the relay.

**Hairpin.** A bridge client dialling `10.77.0.1:5432` is DNATed back onto
the bridge; the reply would go client-wards directly and break conntrack.
One base chain `pub_hairpin` (hook postrouting, priority srcnat):
`ip saddr 10.77.0.0/16 ip daddr 10.77.0.0/16 ct status dnat masquerade`.
The backend sees `10.77.0.1` as the source — what it sees from the relay
today.

**Lifecycle.** Chains are created when the pool is first synced and deleted
when the parent exits (`delete chain`, both). Because the name carries the
pid, a parent starting up GCs `pub_*_p<pid>_*` chains whose pid is dead or
whose port is its own (a crashed predecessor). Failure to talk to `nft` at
any point → warn once, keep the relay; the listener never stopped.

**Failover.** The relay retries the next backend on a refused connect; DNAT
cannot. Between an instance dying and the parent's rewrite (sub-second) a
new connection may be refused. Docker has the same window. Accepted.

## Drain before stop (the 131 errors)

`stop_instance` signals first and removes from the pool on `Drop`, so new
connections keep landing on a process that is shutting down. New order:
remove from every pool → wait `DRAIN` (1 s) → signal → drop. Applies to
rolls, scale-down and restarts, on both the kernel path and the relay. The
app still owes a graceful shutdown for the connections it already holds;
`bench/api` gets `http.Server.Shutdown` on SIGTERM so the bench measures
ply's share.

## Code shape

- `ply-core/src/runtime/ns/kpublish.rs` (new): `KernelPublish { host_port,
  instance_port, scope, pid }` with pure renderers (`chain names`,
  `match_expr`, `dnat_expr`, `sync_script(backends)`, `teardown_script`,
  `hairpin_script`, `stale_chains(json, alive)`) and thin `apply`/`teardown`/
  `gc_stale` that shell out to `nft`.
- `publish.rs::Pool`: optional mirror (`Arc<dyn PoolMirror>`); `insert`/
  `remove` call `mirror.sync(&addrs)` after mutating. `KernelPublish`
  implements `PoolMirror`.
- `backend.rs::Backend::kernel_publish(&self) -> bool` (default false; ns →
  true). `run.rs`: when `kernel_publish() && !facts.loopback &&
  opts.network.is_none() && has_nft()` and the scope is not loopback →
  `pool.mirror(KernelPublish::new(...))`; teardown at parent exit;
  `gc_stale()` at parent start.
- `run.rs::stop_instance`: `drain_then(pools, n, DRAIN, || stop)`.
- Ignored test `write_publish_sample_script` → `target/publish-sample.nft`
  for `sudo nft -c -f`.

## Verification

Unit: renderers, scope table, empty/one/many backends, stale-chain
selection, drain ordering. Syntax: `cargo test -p ply-core
write_publish_sample -- --ignored && sudo nft -c -f target/publish-sample.nft`.
Live: `sudo bench/run.sh` — published-lan `/ping` ≥ 0.9× Docker, runtime CPU
in published cells ≈ direct cells, rolling restart 0 errors with the
graceful bench app, `internal:` DB read ≈ DB-direct. Docs: `docs/running.md`
publish paragraph, `docs/security.md` host-table note (the `ip ply` table
now carries per-port chains).

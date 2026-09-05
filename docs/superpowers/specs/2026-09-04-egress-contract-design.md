# The egress contract: declared, overridable, enforced outbound network policy

**Date:** 2026-09-04 · **Status:** spec for review · **Scope:** rootful Linux, per instance. Rootless Linux ignores the policy with one warning (a rootless stack shares one namespace, so per-member attribution needs the per-instance network the macOS-style switch brings later). The macOS VM backend inherits the policy through `InstanceSpec` and enforces it in its switch (plan 2, not here).

## Goal

An instance may only talk to what its author declared and its operator allowed, and ply can show, at any time, what it actually talked to. Concretely:

- a keg's or app's manifest declares its outbound destinations (a **claim**, published with the keg and visible on the registry page);
- the operator's stack file (or `ply run` flags) sets the **policy**: the mode, and optionally a list that replaces the claim;
- the runtime **enforces** the effective list and writes an **audit log** in every mode that is not `off`;
- `ply egress <app>` reads the log; violations become events.

Motivation: supply-chain compromises phone home from inside containers (npm, PyPI, hijacked maintainers). Docker users need Falco, Cilium, or a vendor to see or stop that. ply owns each instance's network, so it can do it by construction.

## Non-goals (this version)

- Payload or URL inspection (TLS hides them; ply never intercepts certificates).
- Per-package attribution inside a container; the instance is the unit.
- IPv6 policy: enforce mode refuses IPv6 egress outright; audit logs nothing for it.
- Ingress policy (published ports already state exposure).
- Host-wide policy defaults (`/etc/ply/policy.toml`): a follow-up; this spec leaves one hook (the mode default) for it.
- The macOS switch implementation.
- Rootless enforcement or audit. A rootless `ply run`/`ply up` with a non-`off` effective policy prints `ply: egress policy needs a network per instance — rootless runs unenforced and unobserved (use a rootful host to audit or enforce)` once and continues exactly as today.

## The contract

### Manifest: the claim

```toml
[network]
egress = ["api.stripe.com", "*.amazonaws.com", "140.82.112.0/20", "1.1.1.1"]
```

`egress` is a list of **entries**:

| entry | matches |
|---|---|
| `host.example` | that name exactly (case-insensitive, trailing dot ignored) |
| `*.example` | any name ending in `.example`, not `example` itself |
| `1.2.3.4` | that IPv4 address |
| `10.0.0.0/8` | that IPv4 range |
| `*` | everything; the list is "unrestricted". `ply up --plan` and `ply run` print `ply: <app> declares unrestricted egress` |

Anything else (a URL, a port suffix, a bare word with spaces, IPv6 for now) is a manifest error at `ply build` and `ply up`, with the entry and the four accepted forms named. An empty list, `egress = []`, is a valid claim: "this talks to nobody". The absence of `[network]` is not a claim: `ply inspect` shows `egress: not declared`.

### Stack file: the policy

```toml
[[app]]
run    = "postgres@17"
egress = { mode = "enforce" }                       # enforce the keg's declared list
egress = { mode = "enforce", allow = [] }           # override: nothing at all
egress = { mode = "audit",   allow = ["*.stripe.com"] }
egress = "off"                                      # shorthand for { mode = "off" }
```

`allow`, when present, **replaces** the manifest's list for that member. `mode` is one of `off`, `audit`, `enforce`. The same two knobs exist for a standalone run: `ply run --egress <mode>` and `--egress-allow <entry>` (repeatable; any occurrence replaces the manifest list). `ply up` passes them to each member's `ply run` child exactly as it passes `--publish` and `--after`. Deployment files (`ply reconcile`) carry the stack file's member table, so they need no new syntax.

### Effective policy

| manifest `[network] egress` | operator `egress` | effective mode | effective list |
|---|---|---|---|
| absent | absent | `off` | none |
| present | absent | `audit` | the manifest's |
| absent | `{ mode = M }` | M | none, i.e. `enforce` blocks everything undeclared, `audit` logs everything as undeclared |
| present | `{ mode = M }` | M | the manifest's |
| any | `{ mode = M, allow = L }` | M | L |

Rule of thumb: the operator's word wins, the author's claim fills in. The default `audit`-when-declared means shipping a claim never changes what an app can do; it only makes what it does visible.

### Always allowed, in every mode

- loopback (`lo`), which is where the forwarder listens;
- the bridge subnet (`10.77.0.0/16`, the constant `publish::GATEWAY` belongs to): stack members, `<name>.ply`, and the host's `internal` published ports;
- established and related traffic (replies to the above and to allowed connections);
- UDP port 53 to the forwarder's upstream resolvers **from the forwarder's own fixed source port** (35353, which the forwarder holds for the instance's life so the app cannot bind it). An app that ignores `resolv.conf` and queries an upstream directly is treated like any other destination: logged, and blocked in `enforce`.

What the bridge-subnet accept actually costs, said plainly: `10.77.0.0/16` reaches **every other ply instance on the host**, and — through `10.77.0.1` — **any host listener bound on `0.0.0.0` or on the gateway address**. A member's contract is therefore only as strong as its neighbours and as the host's own listeners: an enforced member sitting next to an unenforced one that will proxy for it is not contained, and neither is one on a host running a resolver or an HTTP proxy on `0.0.0.0`. The claim that enforce closes DNS as an exfiltration channel holds only when no resolver or proxy listens there — and, as already stated, only outside the subdomains of whatever wildcard the policy declares. Check 35 in the droplet checklist is the audit of that surface for a given host.

## Mechanism

### Where the rules live

Inside the instance's own network namespace, as one nftables table owned by the supervisor. Not in the host's forward chain. Consequences that make this the right place:

- the app has no `CAP_NET_ADMIN` (the default bounding set is empty; the `oci` preset does not include it either), so it cannot see past or edit its own policy; `--privileged` can, and is already documented as never-production;
- but the `oci` preset DOES keep `CAP_NET_RAW`, and an app with a raw socket writes its own packets: the contract is written against the ordinary socket path, and NET_RAW steps around the parts of it that matter (it can also forge the forwarder's source port and get the upstream rule's accept). So `enforce` refuses to launch an app whose keep set contains `CAP_NET_RAW`, `CAP_NET_ADMIN` or `CAP_SYS_ADMIN`, or that runs `--privileged`:

  ```
  egress policy: enforce needs an app without CAP_NET_RAW/CAP_NET_ADMIN — web keeps CAP_NET_RAW (capabilities = "oci" keeps CAP_NET_RAW); list capabilities without it, or run with --egress audit
  ```

  `audit` warns once (`ply: warning: egress: web keeps CAP_NET_RAW — an app with it can bypass observation`) and runs, because audit promises observation and never containment. The practical consequence: an imported Docker image cannot be enforced until its `capabilities` list drops NET_RAW — which is a one-line manifest edit, and almost always correct (a database does not ping);
- every instance owns its table, so there is nothing to clean up on the host: the table vanishes with the namespace, on every exit path including a crashed supervisor;
- nothing else on the host is touched: no sysctl, no host chain, no listener two supervisors could fight over. The only host requirement is the `nft` binary (the `nftables` package, present on Debian and Ubuntu; ply already prefers it for NAT).

### The egress thread

`NsBackend::launch` spawns one thread per instance after `clone()` and before the child is released (the child is parked on the sync pipe, so its first `connect()` sees the finished rules). The thread:

1. `setns()` into the child's network namespace (`netns::open_ns(pid)` + `enter`; a thread's namespace is its own, the supervisor's main thread stays on the host).
2. Binds the forwarder on `127.0.0.53:53`, UDP and TCP.
3. Installs the table (below) with `nft -f -` spawned from this thread, so the child process inherits the namespace.
4. Signals readiness over a channel; `launch` releases the child only after it.
5. Serves DNS, polls the audit sets every two seconds, writes the log, pins addresses, emits events. Ends when the instance ends (a `Drop` on the instance closes the channel; the thread exits its loop).

If any step fails: `enforce` → the launch fails with the reason (`egress policy: nft not found — install nftables or run with --egress audit`); `audit` → one warning and the instance runs unobserved. A policy in mode `off` spawns no thread and installs nothing; the container's `resolv.conf` stays exactly what it is today.

### The table

Rendered by a pure function `egress::nft_script(policy: &Policy, upstreams: &[Ipv4Addr]) -> String`, tested as golden text. Names use the app identity (`egress_db` for member `db`). Requires nftables 1.0 or newer (Debian 12, Ubuntu 22.04 and later) for dynamic sets with per-element counters; `ply` checks `nft --version` once and reports an older one as unsupported in `enforce`, a warning in `audit`.

```
table inet egress_<app> {
  set allow_static { type ipv4_addr; flags interval; elements = { <IPs and CIDRs from the list> } }
  set allow_dns { type ipv4_addr; flags timeout; size 4096; }     # pinned by the forwarder, per-element timeout
  set upstream { type ipv4_addr; elements = { <forwarder upstreams> } }
  set allowed { type ipv4_addr . inet_proto . inet_service; flags dynamic,timeout; timeout 24h; size 65535; counter; }
  set blocked { type ipv4_addr . inet_proto . inet_service; flags dynamic,timeout; timeout 24h; size 65535; counter; }
  chain output {
    type filter hook output priority filter; policy accept;
    oif "lo" accept
    ip daddr 10.77.0.0/16 accept
    ct state established,related accept
    ip daddr @upstream udp sport 35353 udp dport 53 accept
    meta nfproto ipv6 <verdict>                                   # enforce: drop; audit: accept
    ip daddr @allow_static ct state new update @allowed { ip daddr . meta l4proto . th dport }
    ip daddr @allow_static accept
    ip daddr @allow_dns ct state new update @allowed { ip daddr . meta l4proto . th dport }
    ip daddr @allow_dns accept
    ct state new update @blocked { ip daddr . meta l4proto . th dport }
    <verdict>                                                    # enforce: drop; audit: accept
  }
}
```

Unrestricted (`*` in the list) renders `allow_static` as `0.0.0.0/0`. In `audit` the two sets still record what would have been blocked, which is the whole point of the mode.

Two details that look cosmetic and are not:

- **Each verdict has its own counter.** The accept path updates `@allowed` and the fall-through updates `@blocked`, so a packet is counted in exactly one set and neither number is a difference of two others read at two different instants. (An earlier shape counted every new packet into a `seen` set and subtracted; a read landing between the two snapshots then read as traffic that got through.)
- **The verdict is never on the same line as the `update`.** `update` on a FULL set returns break, so `ct state new update @blocked { … } drop` would fall through to the chain's `policy accept` the moment the set filled — a container that sprays destinations could unlock its own egress by exhausting the audit set. On its own line, an exhausted `@blocked` costs the record and never the enforcement. The `size` bounds are what make exhaustion reachable at all, and are the reason the kernel's memory cannot be grown without limit from inside the container.

### The forwarder

A small DNS proxy: parse just enough of the query to read the question name and type, refuse anything but a single-question message, forward the raw datagram to the first reachable upstream (2 s timeout, then the next) from one UDP socket bound to source port 35353 for the instance's life, parse the answer's A records and TTLs, return the raw answer. TCP from the app is length-prefixed and handled the same way, at most eight connections per loop turn with a 500 ms read timeout so a silent client cannot starve the audit poll; upstream is always UDP. No caching, no recursion, no EDNS handling beyond passing bytes through. Every per-instance map (resolved names, allowed and blocked destinations, the DNS-record damping, event throttles) is capped at 10 000 entries and swept on the 24 h horizon the nft sets use; `nft` invocations are bounded at 5 s.

Decision per query, from the effective policy:

| mode | declared name | undeclared name |
|---|---|---|
| `audit` | forward; pin A records into `allow_dns` (TTL, floor 300 s); log `resolved` (declared) | forward; log `resolved` (undeclared); no pin (irrelevant in audit) |
| `enforce` | forward; pin; log `resolved` | reply `REFUSED` without forwarding; log `refused` |

Refusing at the resolver is what closes DNS itself as an exfiltration channel in enforce mode. AAAA answers are passed through unpinned (IPv6 is dropped by the chain in enforce) — and still logged as a `resolved` record with an empty address list, so the audit trail shows what the app was reaching for. Names in the bridge's `.ply` zone never reach the forwarder: they resolve through `/etc/hosts`, as today.

Pins are refreshed by **delete + add in one batch**, not by adding again: `nft add element … timeout` does not refresh an existing element's expiry before kernel 6.10 / nft 1.1 (it errors or is a no-op), so an instance that keeps resolving a declared name would watch its own pin decay to zero and then be blocked for a name it declared. The forwarder lists `allow_dns`, and issues one `nft -f -` that deletes the addresses already present and adds all of them with the fresh TTL; one transaction means the address is never absent in between. If the listing or the batch fails, it falls back to the plain add and warns once.

A declared name that resolves to a **private or link-local address is not pinned** unless the address is also declared: loopback, `169.254.0.0/16`, RFC 1918, CGNAT (`100.64.0.0/10`), multicast, broadcast, `0.0.0.0/8` and the bridge subnet. Otherwise a declared `*.nip.io` would pin `169.254.169.254` (the cloud metadata service) or a neighbour instance on the bridge — DNS rebinding through the operator's own allow list. The record is still written; only the pin is withheld.

Upstreams: the same resolvers `resolv_conf_for_instance` computes today (the host's, or systemd-resolved's upstream file when the host uses a loopback stub). The container's `resolv.conf` becomes `nameserver 127.0.0.53` plus the host's `search`/`options` lines (`resolv_conf_via` already renders that shape).

The forwarder keeps an address→name map from its own answers (one hour), so connection records can carry the name.

### The audit log

One file per instance, `data_dir()/egress/<app>.<n>.log`, JSON lines, capped like the log ring (512 KiB, one rotation). Record shapes:

```json
{"t":"2026-09-04T21:03:11Z","app":"web","n":1,"kind":"resolved","name":"api.stripe.com","declared":true,"addrs":["54.187.174.169"],"ttl":60,"count":1}
{"t":"…","app":"web","n":1,"kind":"refused","name":"evil.example","declared":false,"count":417}
{"t":"…","app":"web","n":1,"kind":"allowed","proto":"tcp","dst":"54.187.174.169","port":443,"name":"api.stripe.com","count":12}
{"t":"…","app":"web","n":1,"kind":"blocked","proto":"tcp","dst":"203.0.113.9","port":8443,"name":null,"count":3}
```

`allowed`/`blocked` come from the two dynamic sets, polled every two seconds (`nft -j list set …`); each is read from its OWN counter — `@allowed` grew → an `allowed` line, `@blocked` grew → a `blocked` line, neither → silence. A record is written when an element first appears and again when its counter grows, at most once per minute per element. `count` is the set's packet counter for the element: **packets in conntrack state `new`, not connections** — only `ct state new` updates the sets, and a blocked TCP connect retransmits its SYN several times, so one refused connection shows up as several. `ply egress` names that column `NEW PKTS` for the same reason.

`resolved` and `refused` are damped the same way — one record a minute per `(name, kind)`, carrying `count`: the queries it stands for. Written per query they were the one thing a container could produce at will, and a `getent` loop over made-up names rotated the 512 KiB log in seconds, taking the connection evidence with it. Old lines without `count` still parse (as 0).

Events: the first `blocked` record for a destination emits `events::emit(app, "egress-blocked", "tcp 203.0.113.9:8443")`, then at most once per hour per destination. In `audit` mode the same destination emits `egress-undeclared` under the same throttle, so the dashboard shows undeclared traffic before anyone flips to enforce.

### Command surface

- `ply egress <app> [--follow] [--blocked] [--json]`: a table over the instance logs of `app` (all its instances): destination, name, port, protocol, new packets (`NEW PKTS`), first seen, last seen, verdict. `--blocked` filters to `blocked`+`refused`; `--follow` tails; `--json` prints the records.
- `ply up --plan`: one line per member after the env block: `egress: enforce, 3 entries (manifest)` / `egress: audit, 0 entries (override)` / `egress: off` / the unrestricted warning.
- `ply run`: prints `ply: egress enforce, 3 entries (manifest)` at start (the same renderer as the plan line), preceded by the unrestricted warning when it applies; `--egress`, `--egress-allow` flags.
- `ply inspect`: an `egress:` line listing the declared entries or `not declared`.
- `ply check <img>` (the existing manifest check) validates the entry grammar.

### Data model

- `manifest::Network { egress: Option<Vec<EgressEntry>> }` on `Manifest.network: Option<Network>`; `EgressEntry` is an enum `{ Name(String), Wildcard(String), Addr(Ipv4Addr), Cidr(Ipv4Addr, u8), Any }` with `FromStr`/`Display` round-tripping the TOML strings, validated at manifest parse.
- `stack::Member.egress: Option<EgressOverride { mode: Option<Mode>, allow: Option<Vec<EgressEntry>> }>`; `"off"` shorthand parses to `{ mode: Some(Off), allow: None }`.
- `egress::Policy { mode: Mode, allow: Vec<EgressEntry>, source: Manifest | Override | None }` computed by `egress::effective(manifest: Option<&Network>, override: Option<&EgressOverride>) -> Policy` (pure; the table above is its test).
- `backend::InstanceSpec.egress: Option<egress::Policy>` (None for `off`), consumed by `NsBackend::launch`; the VM backend later consumes the same field.
- New module `ply-core/src/egress/`: `mod.rs` (types, `effective`), `entry.rs` (grammar), `nft.rs` (script rendering, set parsing of `nft -j` output), `dns.rs` (message parsing, `REFUSED` reply, forwarder loop), `log.rs` (records, writer, reader for the CLI). The thread lives in `runtime/ns/egress.rs` because it does `setns` and spawns `nft`.

### Testing

Unit (all hosts): entry grammar (accepted forms, rejected forms with their messages), `effective` (the five-row table), nft script golden strings for `enforce`/`audit`/unrestricted/empty, `nft -j list set` parsing on a captured sample, DNS query-name and A-record parsing on captured packets, the `REFUSED` reply bytes, TCP framing, log record serialization and the `ply egress` table on a fixture log, the plan and inspect lines.

Droplet (rootful, the real gate): a two-member stack where `web` enforces `["registry.plybox.sh"]`; inside `web`, `curl https://registry.plybox.sh/` succeeds, `curl https://example.com/` fails fast with a refused resolution, `curl http://93.184.216.34/` (a bare IP) hangs and is dropped; `ply egress web` shows the three destinations with the right verdicts; `ply events` shows one `egress-blocked`; `db` (no policy) is unaffected; a rolling restart keeps the log; stopping the stack leaves no `egress_*` table anywhere (`nft list tables` on the host is unchanged throughout, because the tables were never in the host namespace).

Rootless (the owner's box): the same stack prints the one-line rootless warning per member and behaves exactly as before this feature.

### Rollout

1. Ship the runtime and CLI. No keg changes: nothing declares, so nothing changes.
2. Declare `egress = []` in the postgres and redis kegs' manifests (a database talks to nobody) and re-push. Every stack using them starts logging; anything they reach shows up as undeclared, which is the demonstration.
3. Docs: `docs/security.md` gets the section; `docs/ply-vs-docker.md` gets the row; the registry page shows the claim (a separate, small web change).

### Open questions settled here

- **Why not the host forward chain?** Handles to clean up per instance, a shared host table two supervisors race on, kernel-log sysctls for visibility. In-namespace has none of those, and the same code will serve the per-instance switch later.
- **Why not rootless now?** A rootless stack is one namespace; members are indistinguishable on the wire. A stack-wide union was designed and rejected: it needs a shared table, a shared forwarder, and policy files for the union, for a mode of operation the owner does not run in production. It returns with per-instance networks.
- **Why refuse at DNS in enforce instead of resolving and dropping?** A dropped SYN is a 30-second hang and a confusing error; `REFUSED` is instant and explicit, and it removes DNS as a data channel. Audit keeps resolving so the operator sees names.
- **Why dynamic sets instead of logging?** No log infrastructure, no sysctl, works in any namespace, gives counters for free, and polling every two seconds is cheap. The cost is coarse timing (two-second granularity), acceptable for an audit trail.
- **Why a thread and not a process?** The forwarder and the poller need the instance's namespace and the supervisor's policy; a thread has both, and dies with the instance.

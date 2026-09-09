---
title: Sealed secrets
description: Secrets that live in your repo as ciphertext and open only on the host, at launch, in memory.
section: Guides
order: 15
---

# Sealed secrets

A deployment file, a stack file or a manifest can carry a secret as
ciphertext that opens on exactly one host:

```toml
[env]
DATABASE_URL = "enc:v1:9hf2…"
```

The value is sealed for that host's key. It can sit in a public git repo,
travel through CI, and be read by anyone; only the run parent on that
host opens it, while composing the app's environment, on its way into the
process. On Linux the plaintext exists nowhere but that process's memory
and the child's environment.

This is the shape Kamal and SOPS users already trust. It is not Vault:
there are no leases, no rotation, no identity-based access. For those,
run Vault and inject via env.

## The host's key

```sh
sudo ply secret hostkey        # apps that run as root (systemd units, deployments)
ply secret hostkey             # apps you run as yourself
```

The first call makes the key and prints its public half, a short string
starting with `ply-host-`. The secret half is a 0600 file under ply's data
directory (`/var/lib/ply/host.key` for root) that never leaves the host.
`sudo ply setup` makes root's key too, so a freshly installed server is
ready before its first deployment file arrives.

The key belongs to the user that runs the app. Root's apps open values
sealed for root's key; your rootless apps open values sealed for yours.

## Sealing

On your laptop, or anywhere with the public key:

```sh
ply secret seal DATABASE_URL=postgres://app:s3cret@db/app --for ply-host-…
# DATABASE_URL = "enc:v1:…"
```

Paste the line into `[env]` of the manifest, the deployment file, or the
stack member. `--env` prints `KEY=enc:v1:…` for an env file instead. A
value of `-` reads from stdin, which keeps it out of shell history:

```sh
printf '%s' "$PASSWORD" | ply secret seal DB_PASSWORD=- --for ply-host-…
```

Without `--for`, the value is sealed for the host you are on: the
single-server case, where you seal and run in the same place.

**From the dashboard.** The [dashboard](/docs/dashboard/) can seal too —
its deploy page has a "seal a secret" box, and the notify page can seal the
Telegram token. It uses only the host's public key (`ply setup` writes it
to the granted config dir), so it creates sealed values and, by design, can
never read one back. The private key never leaves the host.

A value is sealed **under its name**. `DATABASE_URL = "enc:v1:…"` opens
only as `DATABASE_URL`: moving the ciphertext to another variable, or
another host, fails with a message that names the variable and never the
value. That is the property that stops a sealed database password from
being re-labelled as something an app prints.

## What happens at launch

Sealed values can arrive from any source the run parent composes:
`[env]` in the manifest, `-e`, `--env-file`, a stack member's `env`, a
deployment's `[env]` or `env_file`. After everything is merged, the run
parent opens what is sealed and says which names it opened, never the
values:

```
ply: unsealed DATABASE_URL, SMTP_PASSWORD for api
```

The same line goes to the events journal, so `ply why` shows it. `ply ps
--json` carries no environment at all.

A sealed value on a host with no key, or one sealed for another host, is
an error **before** anything launches. An app started with `enc:v1:…` as
its database URL is not a useful app.

**macOS.** The microVM backend hands the environment to the guest through
a spec disk in the instance directory, a 0600 file that lives as long as
the instance. Sealed values are opened before it is written, so the
plaintext is on that disk for the instance's lifetime. Fine for a laptop;
say so to yourself before relying on it for more.

## Rotating a host key

There is no rotation command. Make a new key by moving the old file
aside and running `ply secret hostkey`, seal every value again for the
new public key, and deploy. Values sealed for the old key stop opening,
loudly, which is the point.

## The fleet flow

A fleet repo ([Deployments & CD](/docs/deployments/#gitops-fleet)) holds
deployment files for every host, and can now hold their secrets too,
sealed per host: `hosts/web-1/api.toml` carries values sealed for
`web-1`'s key. The repo stays publishable, and a leaked clone is
ciphertext. `.env/<name>.env` files still work for anything you would
rather keep off the repo entirely.

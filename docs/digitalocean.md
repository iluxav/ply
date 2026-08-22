---
title: Running on DigitalOcean
description: Push-to-deploy from GitHub to a droplet — two actions, zero downtime, no orchestrator.
section: Guides
order: 19
---

# Running on DigitalOcean

The complete production loop on a $6 droplet: `git push` → GitHub Actions
builds your image → ships it over SSH → the droplet rolls instances one at
a time behind a published port, health-gated. First run bootstraps the
host by itself; there is nothing to install in advance.

Nothing here is DigitalOcean-specific — any Ubuntu host you can SSH into
works the same. DO is just the worked example.

## One-time setup

**1. A dedicated deploy keypair** — generated on your machine (the private
half's home is a GitHub secret; the droplet only ever sees the public half):

```sh
ssh-keygen -t ed25519 -f ~/.ssh/myapp_deploy -N "" -C "myapp-ci"
```

**2. A droplet** — smallest Ubuntu LTS x64 is plenty. Add the *public* key
(`myapp_deploy.pub`) as its SSH key at creation, note the IP.

**3. Repo secret + variable:**

```sh
gh secret set DEPLOY_SSH_KEY < ~/.ssh/myapp_deploy
gh variable set DROPLET_HOST --body "<droplet-ip>"
```

## The workflow

`.github/workflows/deploy.yml` — build and deploy are the published ply
actions; only the binary build and the verify step are yours:

```yaml
name: deploy
on:
  push:
    branches: [main]

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      # build your app however you build it (Go shown; anything works)
      - uses: actions/setup-go@v5
        with: { go-version: "1.23" }
      - run: CGO_ENABLED=0 GOARCH=amd64 go build -o myapp .

      - name: build image
        id: img
        uses: iluxav/ply@v1
        with:
          dir: "."
          upload: "false"        # shipping over SSH, not GitHub Releases

      - name: ship + roll
        uses: iluxav/ply/deploy@v1
        with:
          image: ${{ steps.img.outputs.image }}
          host: ${{ vars.DROPLET_HOST }}
          ssh-key: ${{ secrets.DEPLOY_SSH_KEY }}
          scale: "2"
          publish: "80:3000"     # host:80 → instances' :3000, pool-balanced

      - name: verify from outside
        run: |
          for i in $(seq 1 20); do
            curl -fsS -m 5 "http://${{ vars.DROPLET_HOST }}/health" && exit 0
            sleep 3
          done
          exit 1
```

## What actually happens

**The first push bootstraps the host.** The deploy action installs ply
(one static binary), places the image at `/srv/<app>/` with a
`current.img` symlink, emits a systemd unit
(`ply systemd … --scale 2 --publish 80:3000`), and starts it. The droplet
fetches the app's dependency closure from the registry — pinned by the
lockfile, verified by hash — and the run parent binds port 80,
load-balancing the pool.

**Every later push is a rolling deploy.** `ply deploy` signals the running
parent; instances restart one at a time from the new image, each gated by
`[health]` before the next one moves. Port 80 never blips — the parent
holds the listener across the roll. A failed health gate aborts and
reverts that slot.

**Version bumps are reboot-safe.** The systemd unit points at
`current.img`; every ship re-links it, so parent restarts and reboots
always come back on the latest deployed image.

Watch a roll happen under live traffic:

```sh
watch -n1 curl -sm2 http://<droplet-ip>/health   # in one terminal
git commit -am "change something visible" && git push   # in another
```

The response changes with zero failed requests in between.

## Manual deploys (no CI)

The actions are convenience, not magic — the same moves by hand:

```sh
ply build .
scp myapp-1.2.0-linux-x64.img root@<ip>:/srv/myapp/
ssh root@<ip> ply deploy /srv/myapp/myapp-1.2.0-linux-x64.img
ssh root@<ip> ply ps
```

## Hardening

The workflow above authenticates as root with trust-on-first-use host
keys — fine for getting started, and each step of the ladder is
independent:

- **Pin the host key**: `ssh-keyscan <ip>` once, store the line as a repo
  variable, pass it as the deploy action's `host-key` input — CI then
  refuses an impostor host.
- **A dedicated deploy user** instead of root (the action's `user` input),
  with ply access via sudo rules.
- A forced-command deploy shell (leaked CI key = can deploy, cannot log
  in) is on the roadmap.

And the standing rule regardless: the private key lives only in the GitHub
secret; a server never holds the credential that opens itself.

# gitea stack

Self-hosted Git (a lightweight GitHub alternative) as a ply stack: gitea +
Postgres, wired in one file. gitea composes the `git` layer and, on first
boot, generates its config, runs migrations, and creates an admin from env.

## Run locally
    ply up

## Deploy on a host
Drop `deploy/gitea-db.toml` and `deploy/gitea.toml` into the deployments
dir (they share `stack = "gitea"`; gitea reaches the db at `gitea-db.ply`).
Add `domain = ["git.example.com"]` + `GITEA_ROOT_URL` to serve over TLS.
Bonus: gitea can host your ply fleet repo — ply hosting its own control plane.

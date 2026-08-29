# Building the gitea keg

The `gitea` binary is a downloaded artifact (gitignored). Fetch it, then build:

    V=1.27.2
    curl -sL "https://github.com/go-gitea/gitea/releases/download/v${V}/gitea-${V}-linux-amd64" -o gitea
    chmod +x gitea
    ply build .

Produces `gitea-<V>-linux-x64.img` — the keg (Go binary + the `git` layer).

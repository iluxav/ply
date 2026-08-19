# dummy_registry

Simulates the origin images are installed from. Any dumb file host is a
first-class ply source — this one is `python3 -m http.server`.

```sh
./build.sh    # downloads alpine minirootfs + node (musl), packages both into registry/
./serve.sh    # serves registry/ at http://127.0.0.1:8321
```

Then in an app's `ply.toml`:

```toml
[dependencies]
base = "alpine@3.20"
node = "24"

[sources]
default = "http://127.0.0.1:8321"
```

`registry/index.json` is the whole version-listing API: a JSON array of image
filenames (filenames are self-describing).

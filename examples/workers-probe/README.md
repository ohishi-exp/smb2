# workers-probe

A Cloudflare Worker that runs smb2 inside workerd. Each request opens an SMB connection over a Workers TCP socket, logs
in with NTLM, connects a share, lists it recursively, reads the first file, and answers:

```json
{"files": 3, "first": {"name": "top.txt", "bytes": 4}}
```

`?path=docs/a.txt` reads that file instead. A failure answers 502 with smb2's error.

The only glue is `WorkerSockets` in `src/lib.rs`: a `smb2::transport::TransportFactory` that opens a `worker::Socket`
and frames SMB2 messages over it. A Workers VPC binding's socket goes through `Socket::from` the same way.

It's a standalone crate (its own `[workspace]` and lockfile, excluded from the repo's workspace) because it only builds
for `wasm32-unknown-unknown`. It is never deployed: `wrangler dev` runs it locally.

## Running it against the Docker fixture

Start the NTLM fixture (`testuser` / `testpass`, share `private`), and put a file or two in it:

```sh
crates/smb2/tests/docker/start.sh internal smb-auth
docker exec <container> sh -c 'echo hi > /shares/private/hello.txt && chmod a+rw /shares/private/hello.txt'
```

Then, from this directory (needs `worker-build` 0.8.7 and Node):

```sh
cp .dev.vars.example .dev.vars   # gitignored; defaults match the fixture
worker-build --release
npx wrangler@4.144.0 dev
curl localhost:8787/
```

# Packaging Lightagent

Lightagent is the agent harness — a terminal CLI plus an HTTP + SSE API — that
drives a running inference gateway. It is a **separate product surface** from the
`hermes`/`lightweight` gateway binaries, so it is packaged on its own: its own
archive script and its own systemd unit, none of which touch the gateway's
packaging.

Everything here is additive. No existing crate, workflow, or gateway packaging
file is changed by shipping Lightagent this way.

## The binary archive

`scripts/package-lightagent.sh` builds a self-contained archive of the
`lightagent` binary, named by Rust target triple. It mirrors
`scripts/package-cli.sh` (the gateway's archive) exactly in structure, so the two
release and install the same way.

```sh
# from the repository root
cargo build --release -p lightagent                 # host build, or
cargo build --release -p lightagent --target <triple>   # cross build
bash ./scripts/package-lightagent.sh [<triple>]     # -> dist-cli/lightagent-<version>-<triple>.(tar.gz|zip)
```

The archive holds the `lightagent` binary, the `LICENSE`, a short `README.md`,
and — on any checkout that has them — the systemd unit and env example below. The
inference engine is **not** in it: Lightagent needs a gateway to talk to, not a
model of its own.

## Installing

```sh
tar -xzf lightagent-<version>-<triple>.tar.gz
install -Dm755 lightagent-<version>-<triple>/lightagent ~/.local/bin/lightagent
lightagent init                 # scaffolds ~/.lightagent and a first profile
lightagent config set inference.base_url http://127.0.0.1:11434
lightagent doctor               # confirms the home, the profile and the gateway
```

`~/.local/bin` on the `PATH` is all that is needed; nothing is installed
system-wide and nothing is code-signed. Verify the download against `SHA256SUMS`
from the same release.

## Running the API as a service (Linux)

`packaging/systemd/lightagent.service` runs `lightagent serve` as a **user**
service. The unit reads every machine-specific value from an environment file
outside the repository, so the repository carries no addresses or credentials of
its own.

```sh
mkdir -p ~/.config/lightagent
cp packaging/systemd/lightagent.env.example ~/.config/lightagent/env
chmod 600 ~/.config/lightagent/env        # it may hold an API key
$EDITOR ~/.config/lightagent/env
mkdir -p ~/.config/systemd/user
cp packaging/systemd/lightagent.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now lightagent
```

`loginctl enable-linger $USER` keeps it running after logout. Run `lightagent
init` once before enabling the service, so the isolated home and a profile exist.

### Binding safely

A loopback bind (`127.0.0.1`, `localhost`, `::1`) is open and needs no key. Any
other bind is reachable from another machine and **requires** a key: set
`LIGHTAGENT_API_KEY` in the env file and add `--key-env LIGHTAGENT_API_KEY` to
`LIGHTAGENT_SERVE_ARGS`. `lightagent serve` refuses to start a non-loopback bind
without one. Do not add `--key-env` while the key is empty — an empty key is
still a key, and would lock the API behind a value nobody can send.

## macOS and Windows

No launchd or Task Scheduler wrapper is shipped, because none has been tested.
Running `lightagent serve` in the foreground works everywhere and needs none of
this. The archive script produces a `.zip` on Windows triples and a `.tar.gz`
elsewhere.

## What is not yet proven

The archive script and the systemd unit are exercised by
`scripts/smoke-lightagent-package.sh` (structure of the archive, and that the
unpacked binary runs `--version`, `doctor` and `banner --preview`). The unit
itself is a static example: it is validated for shape, not run under a live
`systemd --user` here, matching how the gateway's own unit is treated.

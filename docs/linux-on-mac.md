# Linux sandboxes on a Mac: Docker Desktop or the smolvm engine

A Mac runs at most two macOS guests, and `mac` instances are for Apple-platform
work. For everything else a Mac host can run Warren's Linux `default`
instances too, through any engine that speaks the Docker Engine API:

- **Docker Desktop**: the usual Linux VM with containers inside; `runsc`
  (gVisor) is not available there, so instances run under `runc`.
- **The smolvm engine** ([smol-machines/smolvm](https://github.com/smol-machines/smolvm)):
  every "container" is its own libkrun microVM with its own kernel, booted on
  Hypervisor.framework. No Docker Desktop, no shared kernel between instances,
  about a second to boot. This is the recommended engine for Linux instances
  on a Mac.

Warren treats both as the `docker` runtime; the instance's `runtime` field
says `docker`, and `warren doctor` names the engine.

## Published ports

A macOS host has no route to Docker's bridge networks, so the daemon cannot
reach a container at its bridge address. On macOS hosts Warren therefore
starts every Docker instance with its gateway port (3737) and terminal port
(7681) published on a free `127.0.0.1` port each (`--docker-publish-ports`,
on by default on macOS, off on Linux) and proxies through those. The
instance object carries the mapping as `published_ports`, for example
`{"3737": 54321, "7681": 54322}`. Consequences:

- `exec`, files, the Agent API, the browser terminal and `warren run` work
  as on Linux.
- Preview URLs for other ports (`<id>-<port>.<base>`) answer
  `400 not_supported` on such hosts, because only those two ports are
  published. Publish-on-demand is a roadmap item.

## Using the smolvm engine

1. Install smolvm (its release tarball or `brew`), then give Warren its own
   engine instance so your Docker context stays untouched:

   ```sh
   smolvm --state-dir "$HOME/Library/Application Support/Warren/smolvm" engine
   ```

   It prints the socket, `…/smolvm/.smolvm/run/docker.sock`, and registers a
   launchd socket that starts the engine on demand.

2. Load the Warren images into it. The images are built on Linux
   (`make images`); move them with `docker save | docker load`, or pull them
   from a registry you publish them to:

   ```sh
   export DOCKER_HOST=unix://$HOME/Library/Application\ Support/Warren/smolvm/.smolvm/run/docker.sock
   zstd -dc warren-images.tar.zst | docker load
   ```

3. Start the daemon with that socket. `warren up` does this for you when it
   finds no Docker daemon but finds `smolvm`; by hand:

   ```sh
   DOCKER_HOST=unix://… warrend --runtime auto --image-tag dev --mac-base-vm warren-macos-26 …
   ```

   `--runtime auto` picks the engine's only runtime, `smolvm`, when it offers
   neither `runsc` nor `runc`.

## Image architecture

The engine runs the image's architecture: an `amd64` image on an Apple silicon
Mac boots under emulation and is noticeably slower (a `warren-claude` instance
took 23 s to create here against 8 s for a smaller native boot). Build the
Warren images on an arm64 Linux host (or with `docker buildx --platform
linux/arm64`) for Mac hosts; the release workflow's arm64 leg is the natural
place to produce them.

## What is and is not isolated

With the smolvm engine each instance is a hardware-isolated microVM with its
own Linux kernel, which is a stronger boundary than a container and a weaker
operational record than Firecracker (see [compare.md](compare.md)). The egress
rules Warren installs with iptables on Linux hosts do not exist on a Mac; use
`network: allowlist` and the broker for per-instance egress control. Memory
checkpoints and live branching are smolvm features the Docker API does not
expose; sleep stops the microVM and keeps its disk, as on Docker Desktop.

## Measured here

On a MacBook Pro (Apple silicon, 16 GiB) with smolvm 1.36.1: `docker run
alpine` through the engine 1.3 s warm from the native CLI, about 9 s for the
first container through the Docker API adapter (its on-demand start), the
`warren/base` image booting as a microVM in 7.6 s including the engine
adapter start.

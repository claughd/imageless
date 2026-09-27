# imageless microVMs (prototype)

Run an imageless release or flake as a microVM instead of a container.

This is step 1 of three:

1. **Cold boot from a closure image** (this directory).
2. A boot snapshot built as a Nix derivation (needs a KVM builder).
3. A snapshot field in the signed release manifest, and farm publishing.

## Why a block device

The container runtime gives the workload a store path as its root and binds
`/nix/store` in. Firecracker exposes no host filesystem to the guest (no
virtio-fs, no 9p), so here the closure travels as a block device:

```text
reference ──(node policy, signatures, memo, GC roots: the same resolver)──► root store path P
closure(P) + closure(init) ──(mkfs.erofs, once per P per node)──► read-only erofs image
microVM:
  vmlinux (tinyconfig + virtio-mmio, erofs, overlay, vsock; shared by all VMs)
  vda = the closure image, mounted by the kernel as /
  vdb = a 64 KiB config drive: the root and its process, as JSON
  PID 1 = imageless-vm-init: overlay(lower = P, upper = tmpfs) as the workload root,
          the image's /nix/store bound in read-only, pivot, run the process, reap,
          reboot when it exits
```

A store path's closure never changes, so the image is cached under a key of
(root, init, image format) and built once per node. A release that shares a
root with another shares its image.

## Usage

```sh
nix build .#imageless-vm            # the host tool, wrapped with its kernel, init and tools

# A signed release: the node policy decides, exactly as for a container.
imageless-vm run acme/api@sha256:… --memory 512 --vcpus 2

# A flake the node policy allows, with an explicit process.
imageless-vm run path:/src/app#rootfs -- /bin/app --port 8080

# Without KVM: the same kernel, image and drive under QEMU's microvm machine (TCG).
imageless-vm qemu store:/nix/store/…-rootfs -- /bin/app

# Only write the files, and print the Firecracker config it would boot.
imageless-vm prepare acme/api@sha256:…
```

`--env NAME=VALUE` and `--cwd DIR` add to the process that a release's
manifest declares. An argv after `--` replaces it, as a pod's command would.
Images and VM files go under `--state` (default `/var/lib/imageless-vm`).

## What is verified, and what is not

- **Verified:** `nix build .#imageless-vm-boot-smoke`, which is part of
  `nix flake check`. It boots the guest kernel under QEMU `microvm` with TCG
  (no KVM) inside the Nix sandbox. The workload checks, and reports on the
  serial console, that:
  - init is PID 1;
  - its root is an overlay and `/nix/store` is erofs, read-only;
  - the root is writable;
  - argv, env and cwd arrived from the config drive.

  The check then confirms that one cached image was built.
- **Not yet run:** Firecracker itself. The development environment this was
  written in has no `/dev/kvm`. QEMU `microvm` uses the same device model
  (virtio-mmio, announced on the kernel command line) and the same kernel and
  image, so the remaining risk is in the Firecracker config. The first
  `imageless-vm run` on a KVM host will show it.
- **Not built yet:**
  - guest networking (tap devices, addresses);
  - vsock control;
  - snapshots;
  - a daemon or API for many VMs;
  - aarch64.

## Limits of the prototype

- The image is built by copying the closure into a staging tree and running
  `mkfs.erofs` over it. That is simple, but needs twice the closure's size on
  disk while it runs.
- The config drive carries only the process. Secrets and per-instance
  identity would go over vsock, which step 2 needs anyway for readiness and
  post-restore reseeding.

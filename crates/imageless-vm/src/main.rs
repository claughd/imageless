//! imageless-vm: run an imageless release or flake as a microVM (prototype).
//!
//! The container runtime hands the workload a store path as its root and
//! binds /nix/store in. A Firecracker guest can see no host filesystem, so
//! here the closure travels as a block device instead:
//!
//! 1. Resolve the reference exactly as a container create would, through the
//!    node policy: signed releases, allow-listed flakes, the evaluation memo.
//! 2. Pack the root's closure, plus the guest init's, into a read-only erofs
//!    image. A store path's closure never changes, so the image is cached
//!    under a key of (root, init, format) and built once per node.
//! 3. Write the process onto a small config drive, and a VM config that
//!    boots the shared guest kernel with the image as its root device.
//!
//! `run` hands that to Firecracker, which needs KVM; `qemu` boots the same
//! kernel, image and drive under QEMU's `microvm` machine, with or without
//! KVM, which is how this is tested where no KVM exists.

use imageless::{
    manifest_digest, resolve_in_process, Materialize, PolicySource, ReleaseReference,
    ResolvePurpose, ResolveRequest, PROTOCOL_VERSION,
};
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Bumped when the image layout changes, so older cached images stop
/// matching instead of being booted by an init that expects something else.
const IMAGE_FORMAT: &str = "imageless.vm-image.v1";
const CONFIG_DRIVE_BYTES: usize = 64 * 1024;

fn usage() -> ! {
    eprintln!(
        "usage:
  imageless-vm prepare <reference> [options] [-- ARGV...]   write the VM's files, print its config
  imageless-vm run     <reference> [options] [-- ARGV...]   boot it under Firecracker (KVM)
  imageless-vm qemu    <reference> [options] [-- ARGV...]   boot it under QEMU microvm (KVM or TCG)

  <reference>  issuer/name@sha256:<digest>  a signed release (node policy decides)
               path:/dir#output, github:…   a flake the node policy allows
               store:/nix/store/<path>      an already realised root (operator use)
options:
  --state DIR       where images and VM files go (default /var/lib/imageless-vm)
  --memory MIB      guest memory (default 256)
  --vcpus N         guest vCPUs (default 1)
  --closure-file F  store paths to pack, one per line, instead of asking nix-store
  --env NAME=VALUE  add to the process environment (repeatable)
  --cwd DIR         the process working directory
environment:
  IMAGELESS_VM_KERNEL (a directory holding vmlinux), IMAGELESS_VM_INIT (the
  imageless-vm-init store path), IMAGELESS_VM_MKFS_EROFS, IMAGELESS_VM_FIRECRACKER,
  IMAGELESS_VM_QEMU, IMAGELESS_NIX_STORE, IMAGELESS_POLICY"
    );
    std::process::exit(2)
}

struct Options {
    command: String,
    reference: String,
    state: PathBuf,
    memory: u32,
    vcpus: u32,
    closure_file: Option<PathBuf>,
    env: Vec<String>,
    cwd: Option<String>,
    argv: Vec<String>,
}

fn parse(arguments: Vec<String>) -> Options {
    let mut iter = arguments.into_iter();
    let command = iter.next().unwrap_or_else(|| usage());
    let reference = iter.next().unwrap_or_else(|| usage());
    let mut options = Options {
        command,
        reference,
        state: PathBuf::from("/var/lib/imageless-vm"),
        memory: 256,
        vcpus: 1,
        closure_file: None,
        env: Vec::new(),
        cwd: None,
        argv: Vec::new(),
    };
    while let Some(argument) = iter.next() {
        let mut value = || iter.next().unwrap_or_else(|| usage());
        match argument.as_str() {
            "--state" => options.state = PathBuf::from(value()),
            "--memory" => options.memory = value().parse().unwrap_or_else(|_| usage()),
            "--vcpus" => options.vcpus = value().parse().unwrap_or_else(|_| usage()),
            "--closure-file" => options.closure_file = Some(PathBuf::from(value())),
            "--env" => {
                let entry = value();
                if !entry.contains('=') {
                    usage();
                }
                options.env.push(entry);
            }
            "--cwd" => options.cwd = Some(value()),
            "--" => {
                options.argv = iter.collect();
                break;
            }
            _ => usage(),
        }
    }
    options
}

fn main() {
    let options = parse(std::env::args().skip(1).collect());
    if !["prepare", "run", "qemu"].contains(&options.command.as_str()) {
        usage();
    }
    let vm = match prepare(&options) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("imageless-vm: {error}");
            std::process::exit(1);
        }
    };
    match options.command.as_str() {
        "prepare" => println!("{}", vm.firecracker_config.display()),
        "run" => {
            let error = Command::new(tool("IMAGELESS_VM_FIRECRACKER", "firecracker"))
                .args(["--no-api", "--config-file"])
                .arg(&vm.firecracker_config)
                .exec();
            eprintln!("imageless-vm: firecracker: {error}");
            std::process::exit(127);
        }
        "qemu" => {
            let error = qemu_command(&vm, &options).exec();
            eprintln!("imageless-vm: qemu: {error}");
            std::process::exit(127);
        }
        _ => unreachable!(),
    }
}

struct PreparedVm {
    kernel: PathBuf,
    image: PathBuf,
    config_drive: PathBuf,
    boot_args: String,
    firecracker_config: PathBuf,
}

fn tool(variable: &str, fallback: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(fallback))
}

fn required(variable: &str) -> io::Result<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other(format!("{variable} is not set")))
}

/// The resolved root and the process to run in it.
struct Workload {
    root: String,
    argv: Vec<String>,
    env: Vec<String>,
    cwd: String,
}

fn prepare(options: &Options) -> io::Result<PreparedVm> {
    let kernel = required("IMAGELESS_VM_KERNEL")?.join("vmlinux");
    let init = required("IMAGELESS_VM_INIT")?;
    let vm_directory = options.state.join("vms").join(format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&vm_directory)?;

    // GC roots for the root while its image is built; the image holds a copy,
    // so nothing needs the roots once it exists.
    let roots = vm_directory.join("roots");
    std::fs::create_dir_all(&roots)?;
    let workload = resolve(options, &roots)?;
    let image = closure_image(options, &workload.root, &init)?;
    let _ = std::fs::remove_dir_all(&roots);

    let config_drive = vm_directory.join("config.img");
    write_config_drive(&config_drive, &workload)?;
    let init_program = init.join("bin/imageless-vm-init");
    let boot_args = format!(
        "console=ttyS0 reboot=k panic=-1 pci=off rootfstype=erofs init={} imageless.config=/dev/vdb",
        init_program.display()
    );
    let firecracker_config = vm_directory.join("firecracker.json");
    let config = serde_json::json!({
        "boot-source": { "kernel_image_path": kernel, "boot_args": boot_args },
        // Firecracker attaches drives in this order: vda, then vdb.
        "drives": [
            { "drive_id": "closure", "path_on_host": image, "is_root_device": true, "is_read_only": true },
            { "drive_id": "config", "path_on_host": config_drive, "is_root_device": false, "is_read_only": true },
        ],
        "machine-config": { "vcpu_count": options.vcpus, "mem_size_mib": options.memory },
    });
    std::fs::write(
        &firecracker_config,
        serde_json::to_vec_pretty(&config).map_err(io::Error::other)?,
    )?;
    Ok(PreparedVm {
        kernel,
        image,
        config_drive,
        boot_args,
        firecracker_config,
    })
}

fn resolve(options: &Options, roots: &Path) -> io::Result<Workload> {
    if let Some(root) = options.reference.strip_prefix("store:") {
        imageless::validate_store_path("store reference", root).map_err(io::Error::other)?;
        return workload(root.to_string(), None, options);
    }
    let materialize = if options.reference.contains('@') && !options.reference.contains(':') {
        Materialize::Release(ReleaseReference::parse(&options.reference).map_err(io::Error::other)?)
    } else {
        Materialize::Flake(options.reference.clone())
    };
    let request = ResolveRequest {
        version: PROTOCOL_VERSION,
        purpose: ResolvePurpose::Runtime,
        materialize,
        bundle_path: roots.to_path_buf(),
        timeout_ms: 600_000,
        container_name: None,
    };
    let policy = std::env::var_os("IMAGELESS_POLICY").map(PathBuf::from);
    let success =
        resolve_in_process(&PolicySource::File(policy), &request).map_err(io::Error::other)?;
    let resolution = success.resolution;
    workload(resolution.rootfs, resolution.process, options)
}

fn workload(
    root: String,
    process: Option<imageless::ProcessMetadata>,
    options: &Options,
) -> io::Result<Workload> {
    let argv = &options.argv;
    let mut env = vec!["PATH=/bin:/usr/bin".to_string()];
    let mut cwd = "/".to_string();
    let mut command = Vec::new();
    if let Some(process) = process {
        command.extend(process.entrypoint.unwrap_or_default());
        command.extend(process.default_args.unwrap_or_default());
        env.extend(
            process
                .environment
                .into_iter()
                .map(|entry| format!("{}={}", entry.name, entry.value)),
        );
        if let Some(directory) = process.working_directory {
            cwd = directory;
        }
    }
    // An explicit argv replaces the release's, as a pod's command would.
    if !argv.is_empty() {
        command = argv.to_vec();
    }
    env.extend(options.env.iter().cloned());
    if let Some(directory) = &options.cwd {
        cwd = directory.clone();
    }
    if command.is_empty() {
        return Err(io::Error::other(
            "no process: the release declares none; pass one after --",
        ));
    }
    Ok(Workload {
        root,
        argv: command,
        env,
        cwd,
    })
}

/// The closure image for `root`, built once per (root, init, format).
fn closure_image(options: &Options, root: &str, init: &Path) -> io::Result<PathBuf> {
    let key = manifest_digest(format!("{IMAGE_FORMAT}\n{root}\n{}\n", init.display()).as_bytes());
    let images = options.state.join("images");
    std::fs::create_dir_all(&images)?;
    let image = images.join(format!("{key}.erofs"));
    if image.exists() {
        return Ok(image);
    }
    let closure = match &options.closure_file {
        Some(file) => std::fs::read_to_string(file)?,
        None => {
            let output = Command::new(tool("IMAGELESS_NIX_STORE", "nix-store"))
                .args(["--query", "--requisites", root])
                .arg(init)
                .output()?;
            if !output.status.success() {
                return Err(io::Error::other(format!(
                    "nix-store could not compute the closure: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
    };
    let staging = images.join(format!(".{key}.{}.staging", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let built = build_image(&closure, &staging, &image);
    let _ = Command::new("chmod")
        .args(["-R", "u+w"])
        .arg(&staging)
        .status();
    let _ = std::fs::remove_dir_all(&staging);
    built.map(|()| image)
}

/// Stages the closure under nix/store with the guest's mountpoints beside it,
/// and packs that tree as erofs: timestamps pinned and ownership root, so the
/// same closure gives the same image.
fn build_image(closure: &str, staging: &Path, image: &Path) -> io::Result<()> {
    let store = staging.join("nix/store");
    std::fs::create_dir_all(&store)?;
    for mountpoint in ["proc", "sys", "dev", "run", "tmp"] {
        std::fs::create_dir(staging.join(mountpoint))?;
    }
    for path in closure
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        imageless::validate_store_path("closure path", path).map_err(io::Error::other)?;
        let status = Command::new("cp").args(["-a", path]).arg(&store).status()?;
        if !status.success() {
            return Err(io::Error::other(format!("could not stage {path}")));
        }
    }
    let pending = image.with_extension(format!("erofs.{}.pending", std::process::id()));
    let status = Command::new(tool("IMAGELESS_VM_MKFS_EROFS", "mkfs.erofs"))
        .args(["-T0", "--all-root", "-Uclear"])
        .arg(&pending)
        .arg(staging)
        .status()?;
    if !status.success() {
        let _ = std::fs::remove_file(&pending);
        return Err(io::Error::other("mkfs.erofs failed"));
    }
    std::fs::rename(&pending, image)
}

fn write_config_drive(path: &Path, workload: &Workload) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "root": workload.root,
        "args": workload.argv,
        "env": workload.env,
        "cwd": workload.cwd,
    }))
    .map_err(io::Error::other)?;
    if bytes.len() >= CONFIG_DRIVE_BYTES {
        return Err(io::Error::other(
            "the process does not fit the config drive",
        ));
    }
    bytes.resize(CONFIG_DRIVE_BYTES, 0);
    let mut file = std::fs::File::create(path)?;
    file.write_all(&bytes)?;
    file.sync_all()
}

fn qemu_command(vm: &PreparedVm, options: &Options) -> Command {
    let kvm = Path::new("/dev/kvm").exists();
    let mut command = Command::new(tool("IMAGELESS_VM_QEMU", "qemu-system-x86_64"));
    command
        // No ACPI, like a Firecracker guest: QEMU then announces the
        // virtio-mmio devices on the kernel command line, which is the only
        // way this kernel finds them.
        .args(["-M", "microvm,acpi=off,isa-serial=on,rtc=on"])
        .args(["-accel", if kvm { "kvm" } else { "tcg" }])
        .args(["-cpu", if kvm { "host" } else { "max" }])
        .args(["-m", &options.memory.to_string()])
        .args(["-smp", &options.vcpus.to_string()])
        .args([
            "-nodefaults",
            "-no-user-config",
            "-nographic",
            "-no-reboot",
            "-serial",
            "stdio",
        ])
        .arg("-kernel")
        .arg(&vm.kernel)
        .args(["-append", &format!("{} root=/dev/vda ro", vm.boot_args)]);
    for (id, file) in [("closure", &vm.image), ("config", &vm.config_drive)] {
        command
            .arg("-drive")
            .arg(format!(
                "id={id},file={},format=raw,if=none,readonly=on",
                file.display()
            ))
            .args(["-device", &format!("virtio-blk-device,drive={id}")]);
    }
    command
}

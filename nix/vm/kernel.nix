# The guest kernel for imageless microVMs: tinyconfig plus exactly what a
# Firecracker guest (or QEMU's `microvm` machine) running one Nix closure
# needs, built into the image with no modules and no initrd.
#
# The output is an uncompressed vmlinux with a PVH entry point, which both
# Firecracker and `qemu-system-x86_64 -M microvm` boot directly. Devices are
# virtio-mmio, announced on the kernel command line
# (VIRTIO_MMIO_CMDLINE_DEVICES) the way both hypervisors do it.
{ stdenv, lib, linux, flex, bison, bc, perl, elfutils, openssl }:

let
  fragment = builtins.toFile "imageless-vm.config" ''
    CONFIG_64BIT=y
    CONFIG_SMP=y
    CONFIG_PRINTK=y
    CONFIG_BUG=y
    CONFIG_TTY=y
    CONFIG_SERIAL_8250=y
    CONFIG_SERIAL_8250_CONSOLE=y
    CONFIG_HYPERVISOR_GUEST=y
    CONFIG_PARAVIRT=y
    CONFIG_KVM_GUEST=y
    CONFIG_PVH=y
    CONFIG_BINFMT_ELF=y
    CONFIG_BINFMT_SCRIPT=y
    CONFIG_MULTIUSER=y
    CONFIG_FUTEX=y
    CONFIG_EPOLL=y
    CONFIG_SIGNALFD=y
    CONFIG_TIMERFD=y
    CONFIG_EVENTFD=y
    CONFIG_SHMEM=y
    CONFIG_AIO=y
    CONFIG_FILE_LOCKING=y
    CONFIG_POSIX_TIMERS=y
    CONFIG_ADVISE_SYSCALLS=y
    CONFIG_MEMBARRIER=y
    CONFIG_RSEQ=y
    CONFIG_UNIX98_PTYS=y
    CONFIG_PROC_FS=y
    CONFIG_SYSFS=y
    CONFIG_DEVTMPFS=y
    CONFIG_TMPFS=y
    CONFIG_TMPFS_POSIX_ACL=y
    CONFIG_BLOCK=y
    CONFIG_BLK_DEV=y
    CONFIG_VIRTIO_MENU=y
    CONFIG_VIRTIO=y
    CONFIG_VIRTIO_MMIO=y
    CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y
    CONFIG_VIRTIO_BLK=y
    CONFIG_HW_RANDOM=y
    CONFIG_HW_RANDOM_VIRTIO=y
    CONFIG_MISC_FILESYSTEMS=y
    CONFIG_EROFS_FS=y
    CONFIG_OVERLAY_FS=y
    CONFIG_NET=y
    CONFIG_INET=y
    CONFIG_UNIX=y
    CONFIG_PACKET=y
    CONFIG_NETDEVICES=y
    CONFIG_NET_CORE=y
    CONFIG_VIRTIO_NET=y
    CONFIG_VSOCKETS=y
    CONFIG_VIRTIO_VSOCKETS=y
    CONFIG_CGROUPS=y
    CONFIG_NAMESPACES=y
    CONFIG_SECCOMP=y
  '';
in
stdenv.mkDerivation {
  pname = "imageless-vm-kernel";
  inherit (linux) version src;
  nativeBuildInputs = [ flex bison bc perl elfutils openssl ];
  enableParallelBuilding = true;
  # The kernel's own build, not nixpkgs' kernel machinery: no modules, no
  # config introspection at evaluation time.
  configurePhase = ''
    runHook preConfigure
    patchShebangs scripts
    make tinyconfig
    scripts/kconfig/merge_config.sh -m .config ${fragment}
    make olddefconfig
    # merge_config silently drops what Kconfig refuses; refuse to build a
    # kernel missing any of it.
    missing=0
    while IFS= read -r line; do
      case $line in CONFIG_*=y)
        grep -qx "$line" .config || { echo "not applied: $line" >&2; missing=1; } ;;
      esac
    done < ${fragment}
    [ $missing = 0 ]
    runHook postConfigure
  '';
  buildPhase = ''
    runHook preBuild
    make -j$NIX_BUILD_CORES vmlinux
    runHook postBuild
  '';
  installPhase = ''
    mkdir -p $out
    cp vmlinux $out/vmlinux
    cp .config $out/config
  '';
  dontStrip = true;
  meta.platforms = [ "x86_64-linux" ];
}

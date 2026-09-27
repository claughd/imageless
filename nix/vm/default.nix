# imageless microVMs (prototype): the guest kernel, the guest init, and the
# host tool wrapped with everything it drives. See crates/imageless-vm.
{ lib, pkgs, imageless }:

let
  kernel = pkgs.callPackage ./kernel.nix { };

  # Its own derivation, not a binary out of the imageless package: init's
  # closure is packed into every VM image, and the package's closure carries
  # Nix and runc. This one is glibc and the Rust runtime libraries.
  init = pkgs.rustPlatform.buildRustPackage {
    pname = "imageless-vm-init";
    inherit (imageless) version src cargoDeps;
    cargoBuildFlags = [ "-p" "imageless-vm-init" ];
    cargoTestFlags = [ "-p" "imageless-vm-init" ];
    meta.mainProgram = "imageless-vm-init";
  };

  tool = pkgs.runCommand "imageless-vm-${imageless.version}"
    {
      nativeBuildInputs = [ pkgs.makeWrapper ];
      passthru = { inherit kernel init; };
      meta.mainProgram = "imageless-vm";
    } ''
    makeWrapper ${imageless}/bin/imageless-vm $out/bin/imageless-vm \
      --set-default IMAGELESS_VM_KERNEL ${kernel} \
      --set-default IMAGELESS_VM_INIT ${init} \
      --set-default IMAGELESS_VM_MKFS_EROFS ${pkgs.erofs-utils}/bin/mkfs.erofs \
      --set-default IMAGELESS_VM_FIRECRACKER ${pkgs.firecracker}/bin/firecracker \
      --set-default IMAGELESS_VM_QEMU ${pkgs.qemu_kvm}/bin/qemu-system-x86_64
  '';

  # A root with a shell script for a workload: it proves the image, the
  # config drive, init's overlay and pivot, and the process spec reach the
  # guest intact, and says so on the serial console.
  smokeRoot = pkgs.runCommand "imageless-vm-smoke-root" { } ''
    mkdir -p $out/bin
    cat > $out/bin/smoke <<SCRIPT
    #!${pkgs.busybox}/bin/sh
    set -e
    PATH=${pkgs.busybox}/bin
    test "\$(cat /proc/1/comm)" = imageless-vm-in
    test "\$IMAGELESS_SMOKE" = from-config-drive
    test "\$(pwd)" = /tmp
    grep -q ' / overlay ' /proc/mounts
    grep -q ' /nix/store erofs ro' /proc/mounts
    mkdir /scratch && touch /scratch/written-by-the-workload
    echo "imageless-vm-smoke-ok \$1"
    SCRIPT
    chmod +x $out/bin/smoke
  '';

  bootSmoke = pkgs.runCommand "imageless-vm-boot-smoke"
    {
      nativeBuildInputs = [ tool ];
      closure = pkgs.closureInfo { rootPaths = [ smokeRoot init ]; };
    } ''
    # A store reference skips resolution: the check boots a root it built.
    # There is no KVM in the build sandbox, so QEMU runs the guest under TCG.
    printf '{"system":"x86_64-linux","issuers":{}}' > policy.json
    chmod 600 policy.json
    export IMAGELESS_POLICY=$PWD/policy.json
    timeout 600 imageless-vm qemu store:${smokeRoot} \
      --state $PWD/state --closure-file $closure/store-paths --memory 256 \
      --env IMAGELESS_SMOKE=from-config-drive --cwd /tmp \
      -- /bin/smoke argv-arrives < /dev/null > console.log 2>&1 || true
    cat console.log
    grep -q 'imageless-vm-smoke-ok argv-arrives' console.log
    grep -q 'workload exited with status 0' console.log
    # The image is cached by (root, init, format): one image, reused.
    test $(ls state/images/*.erofs | wc -l) = 1
    touch $out
  '';
in
{
  inherit kernel init tool bootSmoke;
}

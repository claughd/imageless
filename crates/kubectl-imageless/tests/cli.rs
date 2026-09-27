//! The binary's stream contract: stdout carries exactly the pod manifest so
//! it pipes into `kubectl apply -f -`; digests and notices go to stderr.

use std::path::PathBuf;
use std::process::Command;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kubectl-imageless"))
}

fn seed(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kubectl-imageless-cli-{label}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("flake.nix"), "{ outputs = _: { }; }\n").unwrap();
    root
}

#[test]
fn stdout_is_exactly_the_pod_manifest() {
    let root = seed("manifest");
    let output = binary()
        .arg("run")
        .arg(&root)
        .args([
            "--repo",
            "registry.example/team/app",
            "--dry-run",
            "--",
            "/bin/server",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // One JSON document and nothing else: from_str fails on trailing content.
    let stdout = String::from_utf8(output.stdout).unwrap();
    let pod: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(pod["kind"], "Pod");
    let image = pod["spec"]["containers"][0]["image"].as_str().unwrap();
    assert!(
        image.starts_with("registry.example/team/app@sha256:"),
        "{image}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("manifest sha256:"), "{stderr}");
    assert!(stderr.contains("flake.lock"), "{stderr}");
    std::fs::remove_dir_all(root).unwrap();
}

/// A pinned reference, so the tests exercise the mode rather than the refusal.
const PINNED: &str = "github:acme/agent/0123456789abcdef0123456789abcdef01234567";

#[test]
fn an_external_pod_names_the_reference_and_a_placeholder_image() {
    let output = binary()
        .args(["run", "--external", PINNED])
        .args([
            "--repo",
            "registry.example/team/agent",
            "--dry-run",
            "--",
            "/bin/agent",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pod: serde_json::Value = serde_json::from_str(&String::from_utf8(output.stdout).unwrap())
        .expect("stdout is exactly one JSON document");
    assert_eq!(
        pod["metadata"]["annotations"]["run.imageless.source"], PINNED,
        "the pod must name the reference, not the packed-seed source"
    );
    // The name comes from the repository; a revision would make two deploys of
    // the same flake differ only in 40 hex digits.
    assert_eq!(pod["metadata"]["name"], "agent");
    let image = pod["spec"]["containers"][0]["image"].as_str().unwrap();
    assert!(
        image.starts_with("registry.example/team/agent@sha256:"),
        "{image}"
    );

    let stderr = String::from_utf8(output.stderr).unwrap();
    for expected in [
        "moves trust from the image to the node",
        // The narrowest boundary-terminated prefix: any revision of that
        // repository, and nothing else.
        "`github:acme/agent/`",
        "run.imageless.* through",
        "the image is a placeholder",
    ] {
        assert!(stderr.contains(expected), "missing {expected:?}: {stderr}");
    }
}

#[test]
fn an_unpinned_reference_is_refused_before_anything_is_pushed() {
    let output = binary()
        .args(["run", "--external", "github:acme/agent"])
        .args(["--repo", "registry.example/team/agent", "--", "/bin/agent"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "a refused run must print no pod manifest"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("is not pinned"), "{stderr}");
    assert!(stderr.contains("--unpinned"), "{stderr}");
}

#[test]
fn image_mode_uses_the_reference_verbatim_and_stays_offline() {
    let output = binary()
        .args(["run", "--external", PINNED])
        // No --dry-run and no --repo: with --image there is nothing to push,
        // so this run must complete without touching the network at all.
        .args(["--image", "registry.k8s.io/pause:3.10", "--", "/bin/agent"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pod: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(
        pod["spec"]["containers"][0]["image"],
        "registry.k8s.io/pause:3.10"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("used verbatim"), "{stderr}");
    assert!(stderr.contains("not digest-pinned"), "{stderr}");
    assert!(
        !stderr.contains("manifest sha256:"),
        "nothing is assembled in --image mode: {stderr}"
    );
}

#[test]
fn a_reference_typed_without_the_flag_is_a_missing_directory_with_a_hint() {
    let output = binary()
        .args(["run", "github:acme/agent"])
        .args(["--repo", "registry.example/team/agent", "--", "/bin/agent"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    // The mode never switches on the argument's shape — it fails as a path,
    // and only then suggests the flag.
    assert!(stderr.contains("No such file or directory"), "{stderr}");
    assert!(stderr.contains("pass --external"), "{stderr}");
}

#[test]
fn requested_help_lands_on_stdout_and_succeeds() {
    for arguments in [&["help"][..], &["--help"], &["run", "--help"]] {
        let output = binary().args(arguments).output().unwrap();
        assert!(output.status.success(), "{arguments:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("Usage:"), "{arguments:?}: {stdout}");
    }
}

/// A catalog whose `refs/agent/stable` names `DIGEST`.
fn catalog(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kubectl-imageless-catalog-cli-{label}-{}",
        std::process::id()
    ));
    let channels = root.join("refs/agent");
    std::fs::create_dir_all(&channels).unwrap();
    std::fs::write(channels.join("stable"), format!("{DIGEST}\n")).unwrap();
    root
}

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn pin_prints_only_the_pinned_reference_so_it_composes() {
    let root = catalog("compose");
    let output = binary()
        .args(["pin", "example/agent", "--catalog"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Exactly the reference and a newline: this is what gets substituted into a
    // manifest, so anything else on stdout would corrupt it.
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("example/agent@sha256:{DIGEST}\n")
    );
    assert!(String::from_utf8(output.stderr).unwrap().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_mistyped_channel_lists_the_ones_that_exist() {
    let root = catalog("mistyped");
    let output = binary()
        .args(["pin", "example/agent:nightly", "--catalog"])
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not published"), "{stderr}");
    assert!(stderr.contains("stable"), "{stderr}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn pin_without_a_catalog_is_a_usage_error_not_a_guess() {
    // There is no node policy on a client to read an issuer's catalog from,
    // and guessing one would resolve against a catalog nobody named.
    let output = binary().args(["pin", "example/agent"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--catalog is required"), "{stderr}");
}

#[test]
fn a_release_pod_records_the_digest_the_channel_pointed_at() {
    let root = catalog("release");
    let output = binary()
        .args(["run", "--release", "example/agent", "--catalog"])
        .arg(&root)
        .args(["--image", "localhost/placeholder:v1", "--", "/bin/agent"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pod: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // The pinned digest, never the channel: a node resolves digests only.
    assert_eq!(
        pod["metadata"]["annotations"]["imageless.run/release-v1"],
        format!("example/agent@sha256:{DIGEST}")
    );
    assert!(pod["metadata"]["annotations"]["run.imageless.source"].is_null());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("republishing the channel"), "{stderr}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn release_and_external_together_are_refused_by_the_parser() {
    // SPEC §3 makes the two annotation families mutually exclusive, and a node
    // refuses a pod carrying both — so this never reaches a cluster to find out.
    let output = binary()
        // Both are boolean flags over one positional, so this is the shape a
        // user actually types when they mean both.
        .args([
            "run",
            "--release",
            "--external",
            "example/agent",
            "--repo",
            "registry.example/team/app",
            "--",
            "/bin/agent",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not both"), "{stderr}");
}

#[test]
fn a_release_without_a_catalog_names_what_is_missing() {
    let output = binary()
        .args([
            "run",
            "--release",
            "example/agent",
            "--image",
            "localhost/placeholder:v1",
            "--",
            "/bin/agent",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--release needs --catalog"), "{stderr}");
}

/// The generated seed has to be an ordinary seed — that is the whole claim of
/// shebang mode. So this emits one and then feeds it back through the *packed
/// directory* path, which knows nothing about shebangs.
///
/// `--unpinned` because a `cargo test` binary has no vendored nixpkgs baked in
/// and refuses to invent a `narHash`; under `nix build` it does, and the lock
/// contents are covered by the generator's own tests either way.
#[test]
fn a_shebang_script_desugars_into_a_seed_the_packer_accepts() {
    let root = std::env::temp_dir().join(format!(
        "kubectl-imageless-cli-shebang-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let script_body = "#!/usr/bin/env nix\n\
                       #! nix shell nixpkgs#python3 --command python3\n\
                       print('hello')\n";
    let script = root.join("hello.py");
    std::fs::write(&script, script_body).unwrap();
    let emitted = root.join("seed");

    let output = binary()
        .args(["run", script.to_str().unwrap(), "--unpinned"])
        .arg("--emit-seed")
        .arg(&emitted)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let flake = std::fs::read_to_string(emitted.join("flake.nix")).unwrap();
    assert!(flake.contains("pkgs.\"python3\""), "{flake}");
    assert!(flake.contains("rootfs ="), "{flake}");
    // The shebang lines are blanked rather than kept or removed: a `#!`
    // continuation line is a syntax error in node, and deleting the lines
    // would renumber every traceback the author reads.
    let packed = std::fs::read_to_string(emitted.join("hello.py")).unwrap();
    assert_eq!(packed, "\n\nprint('hello')\n");
    assert_eq!(packed.lines().count(), script_body.lines().count());

    let repacked = binary()
        .arg("run")
        .arg(&emitted)
        .args([
            "--repo",
            "registry.example/team/app",
            "--dry-run",
            "--",
            "/bin/python3",
        ])
        .output()
        .unwrap();
    assert!(
        repacked.status.success(),
        "{}",
        String::from_utf8_lossy(&repacked.stderr)
    );
    let pod: serde_json::Value =
        serde_json::from_str(&String::from_utf8(repacked.stdout).unwrap()).unwrap();
    assert_eq!(pod["kind"], "Pod");

    // The strong form of the claim: packing the script and packing the seed it
    // emitted are separate code paths — `pack_generated` builds entries from
    // memory, `pack_source` walks a directory — and they must agree to the
    // byte. If they ever diverge, `--emit-seed` stops being a faithful account
    // of what was pushed, which is the only reason it exists.
    let direct = binary()
        .args([
            "run",
            script.to_str().unwrap(),
            "--unpinned",
            "--repo",
            "registry.example/team/app",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(
        direct.status.success(),
        "{}",
        String::from_utf8_lossy(&direct.stderr)
    );
    let layer = |stderr: &[u8]| {
        String::from_utf8_lossy(stderr)
            .lines()
            .find(|line| line.starts_with("layer "))
            .expect("a layer digest line")
            .to_string()
    };
    assert_eq!(layer(&direct.stderr), layer(&repacked.stderr));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_file_that_is_neither_a_seed_nor_a_shebang_says_so() {
    let root = std::env::temp_dir().join(format!(
        "kubectl-imageless-cli-plain-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let script = root.join("plain.sh");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();

    let output = binary()
        .args([
            "run",
            script.to_str().unwrap(),
            "--repo",
            "registry.example/team/app",
            "--dry-run",
            "--",
            "/bin/sh",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("nix shebang"), "{stderr}");
    assert!(stderr.contains("flake.nix"), "{stderr}");
    std::fs::remove_dir_all(root).unwrap();
}

fn scratch(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kubectl-imageless-cli-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn shebang_script(root: &std::path::Path) -> PathBuf {
    let script = root.join("hello.py");
    std::fs::write(
        &script,
        "#!/usr/bin/env nix\n#! nix shell nixpkgs#python3 --command python3\nprint('hello')\n",
    )
    .unwrap();
    script
}

#[test]
fn emit_seed_refuses_flags_it_would_otherwise_ignore() {
    let root = scratch("emit-flags");
    let script = shebang_script(&root);
    for extra in [
        &["--tag", "v1"][..],
        &["--plain-http"],
        &["--dry-run"],
        &["--writable", "/tmp"],
    ] {
        let output = binary()
            .args(["run", script.to_str().unwrap(), "--unpinned", "--emit-seed"])
            .arg(root.join("seed"))
            .args(extra)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{extra:?} was accepted");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(extra[0]), "{stderr}");
        assert!(!root.join("seed").exists(), "{extra:?} still wrote a seed");
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn emit_seed_never_writes_through_a_dangling_symlink() {
    let root = scratch("emit-dangling");
    let script = shebang_script(&root);
    let seed = root.join("seed");
    std::fs::create_dir(&seed).unwrap();
    let outside = root.join("outside.nix");
    std::os::unix::fs::symlink(&outside, seed.join("flake.nix")).unwrap();
    let output = binary()
        .args(["run", script.to_str().unwrap(), "--unpinned", "--emit-seed"])
        .arg(&seed)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already exists"));
    assert!(!outside.exists(), "the symlink's target was written");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_symlink_to_a_seed_directory_packs_that_directory() {
    let root = scratch("symlinked-seed");
    let real = seed("symlinked-seed-target");
    let link = root.join("app");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let output = binary()
        .arg("run")
        .arg(&link)
        .args([
            "--repo",
            "registry.example/team/app",
            "--dry-run",
            "--",
            "/bin/server",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(real).unwrap();
}

#[test]
fn writable_paths_reach_the_pod_manifest() {
    let root = seed("writable");
    let output = binary()
        .arg("run")
        .arg(&root)
        .args([
            "--repo",
            "registry.example/team/app",
            "--dry-run",
            "--writable",
            "/tmp",
            "--",
            "/bin/server",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let pod: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        pod["spec"]["containers"][0]["volumeMounts"][0]["mountPath"],
        "/tmp"
    );
    assert_eq!(pod["spec"]["volumes"][0]["emptyDir"], serde_json::json!({}));
    std::fs::remove_dir_all(root).unwrap();
}

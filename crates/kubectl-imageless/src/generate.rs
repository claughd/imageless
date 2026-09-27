//! Generation of a seed flake from a `nix`-shebang script.
//!
//! The output is an ordinary seed tree — `flake.nix`, `flake.lock`, and the
//! script — so everything downstream of here (packing, pushing, the pod, the
//! node, the spec) is the path a hand-written seed already takes. Nothing in
//! the runtime knows a shebang was involved.
//!
//! The client has no Nix, which decides most of the design. It cannot evaluate,
//! so the flake is emitted as text rather than built; it cannot hash, so the
//! lock it writes can only name an input whose `narHash` was known at *this
//! binary's* build time. That is the pin the flake below carries: the same
//! nixpkgs `imageless` itself locks, baked in by `nix/package.nix`. A node that
//! has ever built imageless already has that input, so the generated seed
//! resolves offline there — the same argument `examples/nginx-embedded` makes
//! for pinning to this repo's rev by hand.

use crate::pack::GeneratedFile;
use crate::shebang::Shebang;

/// Where the script lands in the materialized rootfs. Not `bin/`: the script
/// is data that an interpreter reads, and `bin/` is `buildEnv`'s to populate.
const SCRIPT_DIRECTORY: &str = "share/imageless";

/// The nixpkgs this binary can lock, baked in at build time.
///
/// `None` for a plain `cargo build`, which has no way to learn a `narHash`.
/// Such a binary can still generate an unpinned seed on request, and refuses
/// to generate a pinned one rather than emitting a lock it made up.
pub struct Pin {
    pub rev: &'static str,
    pub nar_hash: &'static str,
    pub last_modified: &'static str,
}

pub fn vendored_pin() -> Option<Pin> {
    Some(Pin {
        rev: option_env!("IMAGELESS_VENDORED_NIXPKGS_REV")?,
        nar_hash: option_env!("IMAGELESS_VENDORED_NIXPKGS_NARHASH")?,
        last_modified: option_env!("IMAGELESS_VENDORED_NIXPKGS_LAST_MODIFIED")?,
    })
}

pub struct GeneratedSeed {
    pub files: Vec<GeneratedFile>,
    /// What the pod runs: the interpreter from the environment, the shebang's
    /// own arguments, then the script.
    pub command: Vec<String>,
}

/// `--arch` speaks OCI; a flake speaks Nix. The two vocabularies are small and
/// the mapping is the whole of it.
pub fn nix_system(arch: &str) -> Result<&'static str, String> {
    match arch {
        "amd64" => Ok("x86_64-linux"),
        "arm64" => Ok("aarch64-linux"),
        other => Err(format!(
            "--arch {other}: a generated seed has to name a Nix system, and only amd64 and \
             arm64 map to one"
        )),
    }
}

/// The script's name has to survive being a tar entry, a Nix path, and a shell
/// word in the generated derivation. Rather than quote for three grammars at
/// once, refuse anything that is not plainly all three.
pub fn validate_script_name(name: &str) -> Result<(), String> {
    let legal = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+');
    if name.is_empty() || name.starts_with('.') || !name.chars().all(legal) {
        return Err(format!(
            "{name}: a shebang script's file name may use letters, digits, and .-_+ only, and \
             may not start with a dot — rename it, or write the seed directory by hand"
        ));
    }
    // The script shares a directory with the two files this generates. Packing
    // catches the collision (a duplicated tar name), but `--emit-seed` writes
    // in list order and would silently leave the script *as* the flake — the
    // one path here that can destroy the thing it just produced.
    if matches!(name, "flake.nix" | "flake.lock") {
        return Err(format!(
            "{name}: the generated seed writes its own {name} alongside the script, so the \
             script cannot be named that — rename it"
        ));
    }
    Ok(())
}

/// Replace the shebang lines with empty ones — neither kept nor removed.
///
/// Keeping them is wrong. `#!` is a comment in bash, Python, and Perl, so the
/// first design copied the script verbatim and let the pod name the
/// interpreter. Node disagrees: it strips a *leading* shebang and nothing
/// else, so `nixpkgs#nodejs --command node` produced a rootfs that built
/// cleanly and then died on `SyntaxError` at the `#! nix shell …` line.
///
/// Removing them is worse than blanking. It shifts every line number in the
/// file, so the first traceback the author reads points at the wrong line —
/// a debugging cost paid forever to save two blank lines once.
fn without_shebang(script: &str, lines: usize) -> String {
    let mut out = String::with_capacity(script.len());
    for (index, line) in script.split_inclusive('\n').enumerate() {
        if index >= lines {
            out.push_str(line);
        } else if line.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

pub fn seed(
    script_name: &str,
    script: &str,
    shebang: &Shebang,
    system: &str,
    pin: Option<&Pin>,
) -> Result<GeneratedSeed, String> {
    validate_script_name(script_name)?;

    let mut files = vec![
        GeneratedFile {
            name: "flake.nix".to_string(),
            mode: 0o644,
            data: flake_nix(script_name, shebang, system, pin).into_bytes(),
        },
        GeneratedFile {
            name: script_name.to_string(),
            mode: 0o755,
            data: without_shebang(script, shebang.shebang_lines).into_bytes(),
        },
    ];
    if let Some(pin) = pin {
        files.push(GeneratedFile {
            name: "flake.lock".to_string(),
            mode: 0o644,
            data: flake_lock(pin).into_bytes(),
        });
    }

    let mut command = vec![format!("/bin/{}", shebang.interpreter)];
    command.extend(shebang.interpreter_arguments.iter().cloned());
    command.push(format!("/{SCRIPT_DIRECTORY}/{script_name}"));
    Ok(GeneratedSeed { files, command })
}

/// `haskellPackages.3d-graphics-examples` → `"haskellPackages"."3d-graphics-examples"`.
///
/// Quoted rather than bare because a nixpkgs attribute is not required to be a
/// Nix identifier, and the two disagree in ways that are silent rather than
/// loud. Nix lexes `.3` as the start of a float, so bare `pkgs.haskellPackages
/// .3d-graphics-examples` parses as something else entirely and dies on an
/// identifier the author never wrote; a keyword component like `.if` is a
/// parse error outright. `haskellPackages` really does carry `2captcha`,
/// `3d-graphics-examples` and `4Blocks`, so refusing these would reject
/// installables that `nix shell` accepts — the same false rejection the
/// `--command` name check was demoted to a warning to avoid.
///
/// Safe to emit unescaped because `shebang::attribute_path` has already
/// confined the string to `[A-Za-z0-9-_.']`: no quote, backslash, or `$` can
/// reach here to close the string or open an interpolation.
fn quoted_attribute_path(attribute: &str) -> String {
    attribute
        .split('.')
        .map(|component| format!("\"{component}\""))
        .collect::<Vec<_>>()
        .join(".")
}

fn flake_nix(script_name: &str, shebang: &Shebang, system: &str, pin: Option<&Pin>) -> String {
    let url = match pin {
        Some(pin) => format!("github:nixos/nixpkgs/{}", pin.rev),
        // Deliberately a branch and not a tag: an unpinned seed is asking for
        // "whatever is current", and pretending otherwise with a stale tag
        // would be the worst of both.
        None => "github:nixos/nixpkgs/nixos-unstable".to_string(),
    };
    let paths = shebang
        .packages
        .iter()
        .map(|package| format!("          pkgs.{}\n", quoted_attribute_path(package)))
        .collect::<String>();
    // A plain `cp`: the seed already holds the script the container runs, with
    // its shebang lines blanked client-side (see `without_shebang`). Doing it
    // there rather than with `sed` here keeps what a reader of the seed sees
    // and what the container executes the same file.
    format!(
        "# Generated from the shebang of {script_name} by `kubectl imageless run`. It is a\n\
         # normal seed flake — nothing downstream knows a shebang produced it — and once\n\
         # written out with --emit-seed it is yours to commit and hand-edit.\n\
         {{\n\
        \x20 inputs.nixpkgs.url = \"{url}\";\n\
         \n\
        \x20 outputs = {{ self, nixpkgs }}:\n\
        \x20   let\n\
        \x20     system = \"{system}\";\n\
        \x20     pkgs = nixpkgs.legacyPackages.${{system}};\n\
        \x20     environment = pkgs.buildEnv {{\n\
        \x20       name = \"imageless-shebang-environment\";\n\
        \x20       paths = [\n\
         {paths}\
        \x20       ];\n\
        \x20     }};\n\
        \x20   in\n\
        \x20   {{\n\
        \x20     # SPARSE, like every imageless rootfs: the interpreter and its closure stay\n\
        \x20     # in /nix/store, which the node binds read-only at container-create.\n\
        \x20     rootfs = pkgs.runCommand \"imageless-shebang-rootfs\" {{ }} ''\n\
        \x20       mkdir -p $out/bin $out/dev $out/etc $out/nix/store $out/proc \\\n\
        \x20         $out/{SCRIPT_DIRECTORY} $out/sys $out/tmp\n\
        \x20       ln -s ${{environment}}/bin/* $out/bin/\n\
        \x20       cp ${{./{script_name}}} $out/{SCRIPT_DIRECTORY}/{script_name}\n\
        \x20       # The runtime binds hostname/hosts/resolv.conf over the readonly root, so\n\
        \x20       # the mountpoints have to exist; getpwnam must resolve for anything that\n\
        \x20       # looks up its own user.\n\
        \x20       printf 'root:x:0:0:root:/tmp:/bin/{interpreter}\\n' > $out/etc/passwd\n\
        \x20       printf 'root:x:0:\\n' > $out/etc/group\n\
        \x20       touch $out/etc/hostname $out/etc/hosts $out/etc/resolv.conf\n\
        \x20     '';\n\
        \x20   }};\n\
         }}\n",
        interpreter = shebang.interpreter,
    )
}

/// Hand-written rather than serialized through `serde_json`, because the shape
/// is fixed: one input, one root, version 7. `original` carries the rev too —
/// the URL above pins it, and a lock whose `original` disagreed with the flake
/// would be re-locked by the node on first evaluation, quietly discarding the
/// only pin the client was able to supply.
fn flake_lock(pin: &Pin) -> String {
    format!(
        "{{\n\
        \x20 \"nodes\": {{\n\
        \x20   \"nixpkgs\": {{\n\
        \x20     \"locked\": {{\n\
        \x20       \"lastModified\": {last_modified},\n\
        \x20       \"narHash\": \"{nar_hash}\",\n\
        \x20       \"owner\": \"nixos\",\n\
        \x20       \"repo\": \"nixpkgs\",\n\
        \x20       \"rev\": \"{rev}\",\n\
        \x20       \"type\": \"github\"\n\
        \x20     }},\n\
        \x20     \"original\": {{\n\
        \x20       \"owner\": \"nixos\",\n\
        \x20       \"repo\": \"nixpkgs\",\n\
        \x20       \"rev\": \"{rev}\",\n\
        \x20       \"type\": \"github\"\n\
        \x20     }}\n\
        \x20   }},\n\
        \x20   \"root\": {{\n\
        \x20     \"inputs\": {{\n\
        \x20       \"nixpkgs\": \"nixpkgs\"\n\
        \x20     }}\n\
        \x20   }}\n\
        \x20 }},\n\
        \x20 \"root\": \"root\",\n\
        \x20 \"version\": 7\n\
         }}\n",
        last_modified = pin.last_modified,
        nar_hash = pin.nar_hash,
        rev = pin.rev,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shebang(script: &str) -> Shebang {
        crate::shebang::parse(script).expect("parses")
    }

    const SCRIPT: &str = "#!/usr/bin/env nix\n\
                          #! nix shell nixpkgs#python3 --command python3 -u\n\
                          print('hi')\n";

    fn pin() -> Pin {
        Pin {
            rev: "8c3cede7ddc26bd659d2d383b5610efbd2c7a16e",
            nar_hash: "sha256-rppURzHviaQN131F+nLiLdGfcb0uCd9gGP0E5+iw9MI=",
            last_modified: "1780930886",
        }
    }

    fn file<'a>(seed: &'a GeneratedSeed, name: &str) -> &'a GeneratedFile {
        seed.files
            .iter()
            .find(|file| file.name == name)
            .unwrap_or_else(|| panic!("{name} is in the seed"))
    }

    #[test]
    fn a_pinned_seed_carries_the_flake_the_script_and_a_lock() {
        let seed = seed(
            "hello.py",
            SCRIPT,
            &shebang(SCRIPT),
            "x86_64-linux",
            Some(&pin()),
        )
        .expect("generates");
        let mut names: Vec<_> = seed.files.iter().map(|file| file.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["flake.lock", "flake.nix", "hello.py"]);
    }

    /// Blanked, not stripped: node treats a `#!` continuation line as a syntax
    /// error, and removing the lines instead would renumber every traceback.
    #[test]
    fn the_packed_script_keeps_its_line_numbers_and_loses_its_shebang() {
        let seed = seed(
            "hello.py",
            SCRIPT,
            &shebang(SCRIPT),
            "x86_64-linux",
            Some(&pin()),
        )
        .expect("generates");
        let packed = String::from_utf8(file(&seed, "hello.py").data.clone()).expect("utf-8");
        assert_eq!(packed.lines().count(), SCRIPT.lines().count());
        assert!(!packed.contains("#!"), "{packed:?}");
        assert_eq!(packed, "\n\nprint('hi')\n");
    }

    #[test]
    fn a_script_with_no_trailing_newline_survives_blanking() {
        let script = "#!/usr/bin/env nix\n#! nix shell nixpkgs#bash --command bash\necho hi";
        let seed = seed(
            "s.sh",
            script,
            &shebang(script),
            "x86_64-linux",
            Some(&pin()),
        )
        .expect("generates");
        let packed = String::from_utf8(file(&seed, "s.sh").data.clone()).expect("utf-8");
        assert_eq!(packed, "\n\necho hi");
    }

    #[test]
    fn the_pod_command_names_the_interpreter_its_flags_and_the_script() {
        let seed = seed(
            "hello.py",
            SCRIPT,
            &shebang(SCRIPT),
            "x86_64-linux",
            Some(&pin()),
        )
        .expect("generates");
        assert_eq!(
            seed.command,
            ["/bin/python3", "-u", "/share/imageless/hello.py"]
        );
    }

    #[test]
    fn the_generated_flake_pins_the_vendored_rev_and_names_every_package() {
        let script = "#!/usr/bin/env nix\n\
                      #! nix shell nixpkgs#python3 nixpkgs#curl --command python3\n";
        let seed = seed(
            "s.py",
            script,
            &shebang(script),
            "x86_64-linux",
            Some(&pin()),
        )
        .expect("generates");
        let flake = String::from_utf8(file(&seed, "flake.nix").data.clone()).expect("utf-8");
        assert!(
            flake.contains("github:nixos/nixpkgs/8c3cede7ddc26bd659d2d383b5610efbd2c7a16e"),
            "{flake}"
        );
        assert!(flake.contains("pkgs.\"python3\"\n"), "{flake}");
        assert!(flake.contains("pkgs.\"curl\"\n"), "{flake}");
        assert!(flake.contains("system = \"x86_64-linux\";"), "{flake}");
        // The interpolation Nix must see, not one the formatter ate.
        assert!(
            flake.contains("nixpkgs.legacyPackages.${system}"),
            "{flake}"
        );
        assert!(flake.contains("cp ${./s.py}"), "{flake}");
    }

    #[test]
    fn an_unpinned_seed_writes_no_lock_and_tracks_a_branch() {
        let seed =
            seed("hello.py", SCRIPT, &shebang(SCRIPT), "x86_64-linux", None).expect("generates");
        assert!(seed.files.iter().all(|file| file.name != "flake.lock"));
        let flake = String::from_utf8(file(&seed, "flake.nix").data.clone()).expect("utf-8");
        assert!(flake.contains("nixos-unstable"), "{flake}");
    }

    #[test]
    fn the_lock_agrees_with_the_url_so_the_node_does_not_relock() {
        let lock = flake_lock(&pin());
        assert_eq!(lock.matches(pin().rev).count(), 2, "locked and original");
        assert!(lock.contains("\"version\": 7"), "{lock}");
        assert!(lock.contains("\"lastModified\": 1780930886"), "{lock}");
    }

    #[test]
    fn a_script_name_that_would_need_quoting_is_refused() {
        for name in ["a b.py", "a;b", ".hidden", "", "sc$ript"] {
            assert!(validate_script_name(name).is_err(), "{name}");
        }
        // Not a quoting problem: these two would collide with the files the
        // seed generates, and `--emit-seed` writes in order — the script would
        // land on top of the flake it was supposed to be built by.
        for name in ["flake.nix", "flake.lock"] {
            assert!(validate_script_name(name).is_err(), "{name}");
        }
        for name in ["hello.py", "deploy-1.sh", "a_b+c"] {
            assert!(validate_script_name(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn only_architectures_with_a_nix_system_are_accepted() {
        assert_eq!(nix_system("amd64").expect("maps"), "x86_64-linux");
        assert_eq!(nix_system("arm64").expect("maps"), "aarch64-linux");
        assert!(nix_system("riscv64").is_err());
    }

    #[test]
    fn every_attribute_component_is_quoted_in_the_generated_flake() {
        assert_eq!(quoted_attribute_path("python3"), "\"python3\"");
        assert_eq!(
            quoted_attribute_path("haskellPackages.3d-graphics-examples"),
            "\"haskellPackages\".\"3d-graphics-examples\""
        );
    }

    #[test]
    fn a_digit_leading_attribute_reaches_the_flake_as_valid_nix() {
        // Bare, `pkgs.haskellPackages.3d-graphics-examples` lexes `.3` as a
        // float and dies on `undefined variable 'd-graphics-examples'` — an
        // identifier no author ever wrote — on the node, after the push.
        let shebang = crate::shebang::parse(
            "#!/usr/bin/env nix\n\
             #! nix shell nixpkgs#haskellPackages.3d-graphics-examples --command bash\n",
        )
        .expect("parses");
        let flake = flake_nix("s.sh", &shebang, "x86_64-linux", None);
        assert!(
            flake.contains("pkgs.\"haskellPackages\".\"3d-graphics-examples\""),
            "{flake}"
        );
        assert!(!flake.contains("pkgs.haskellPackages.3d"), "{flake}");
    }
}

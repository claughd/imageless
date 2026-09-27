//! External flake references: what the plugin may add on top of the contract.
//!
//! Grammar is not decided here. [`imageless::validate_source`] is the node's
//! own validator and this module defers to it, so a reference the plugin
//! accepts is one the node accepts, with the node's own error text. What lives
//! here is the authoring-time rule the node deliberately does not enforce:
//! SPEC §3 says "rejecting mutable references is authoring- and
//! admission-tooling's job", and this is that tooling. Pin enforcement is
//! therefore absent from the `imageless` crate on purpose — that crate's
//! public surface is the node contract, and this rule is precisely what the
//! contract excludes.

use crate::podspec;

/// The pin forms SPEC §3 enumerates. There are exactly three; inventing a
/// fourth here would be the plugin claiming contract authority it does not
/// have, so a genuine-but-unlisted pin (a commit-named tarball URL, say) is
/// reported unpinned and needs `--unpinned`.
#[cfg_attr(test, derive(Debug, PartialEq))]
pub enum Pin {
    /// `?rev=<40 hex>`
    Revision,
    /// `?narHash=sha256-…`
    NarHash,
    /// `github:owner/repo/<40 hex>` and the other shorthand schemes.
    CommitPath,
}

/// Shorthand schemes whose third path segment may be a commit. Listing them
/// matters: `tarball+https://host/a/b/<40 hex>` is a path that merely looks
/// commit-shaped, and treating it as a pin would invent a fourth form.
const COMMIT_PATH_SCHEMES: [&str; 3] = ["github", "gitlab", "sourcehut"];

/// Validate a reference the user asked to deploy with `--external`.
///
/// Two checks precede the library, each because the library structurally
/// cannot make it, and everything else is the library's verbatim answer.
pub fn validate(reference: &str) -> Result<(), String> {
    // `validate_source` ACCEPTS a leading `/`: it is a valid in-image path,
    // which is what the packed mode produces. Only the CLI knows the user
    // typed `--external`, so only the CLI can call this a mistake.
    if reference.starts_with('/')
        || reference.starts_with("./")
        || reference.starts_with("../")
        || reference.starts_with('~')
        || reference == "."
        || reference == ".."
    {
        return Err(format!(
            "`{reference}` is a path, not an external flake reference; \
             drop --external to pack that directory into a seed image"
        ));
    }
    // The library rejects a fragment correctly but cannot name the CLI's
    // remedy, so name it before asking.
    if let Some((base, output)) = reference.split_once('#') {
        return Err(format!(
            "`{base}` carries a flake output fragment; pass the reference \
             without `#{output}` and select the output with `--output {output}`"
        ));
    }
    imageless::validate_source(reference).map_err(|error| error.to_string())
}

/// Which pin form, if any, this reference carries.
///
/// A malformed pin is its own error rather than "unpinned": a truncated
/// revision is a typo the node would only discover after a fetch.
pub fn pin(reference: &str) -> Result<Option<Pin>, String> {
    let (locator, query) = match reference.split_once('?') {
        Some((locator, query)) => (locator, Some(query)),
        None => (reference, None),
    };
    for parameter in query.unwrap_or_default().split('&') {
        let Some((key, value)) = parameter.split_once('=') else {
            continue;
        };
        // Nix compares these keys case-insensitively; `ref=` is a branch name,
        // which is mutable and so is not a pin at all.
        if key.eq_ignore_ascii_case("rev") {
            if !is_commit(value) {
                return Err(format!(
                    "`?rev={value}` is not a 40-character commit hash; nix resolves \
                     only full revisions, so this would fail on the node"
                ));
            }
            return Ok(Some(Pin::Revision));
        }
        if key.eq_ignore_ascii_case("narhash") {
            if !value.starts_with("sha256-") || value.len() <= "sha256-".len() {
                return Err(format!(
                    "`?narHash={value}` is not an SRI hash; a content pin looks \
                     like `?narHash=sha256-…`"
                ));
            }
            return Ok(Some(Pin::NarHash));
        }
    }
    // `github:owner/repo/<rev>`. Exactly three segments: `github:owner/repo`
    // is unpinned and `github:owner/repo/main` names a mutable branch.
    if let Some((scheme, path)) = locator.split_once(':') {
        let segments: Vec<&str> = path.split('/').collect();
        if COMMIT_PATH_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str())
            && segments.len() == 3
            && is_commit(segments[2])
        {
            return Ok(Some(Pin::CommitPath));
        }
    }
    Ok(None)
}

/// Why an unpinned reference is refused. The caller appends the opt-out to
/// make the error form, and prints this text as-is for the warning form.
pub fn unpinned_diagnostic(reference: &str) -> String {
    format!(
        "`{reference}` is not pinned: a mutable reference is not a deployment identity — \
         the pod records a name, and what it materializes can change between restarts, \
         nodes, and reschedules. Pin it with a commit (`github:owner/repo/<40-hex>`), \
         `?rev=<40-hex>`, or `?narHash=sha256-…`"
    )
}

/// The narrowest `eval_allowed_uri_prefixes` entry that authorizes this
/// reference, terminated at a `/` boundary.
///
/// SPEC §3 warns that an unterminated `github:myorg` also authorizes
/// `github:myorg-evil/anything`, so the suggestion always ends at a separator.
/// The node matches a literal byte prefix against the reference with its
/// output fragment removed, and `plan` builds that string as `{source}#{output}`
/// — so the bytes suggested here are the bytes the node compares.
pub fn policy_prefix(reference: &str) -> String {
    let locator = reference.split('?').next().unwrap_or(reference);
    // Only separators in the path count: the `//` of `https://host` belongs to
    // the authority, and cutting there would suggest `https://` — a prefix
    // authorizing every host on the internet.
    let path_starts = match locator.find("://") {
        Some(scheme) => scheme + "://".len(),
        None => locator.find(':').map_or(0, |colon| colon + 1),
    };
    match locator[path_starts..].rfind('/') {
        Some(slash) => locator[..=path_starts + slash].to_string(),
        // No path separator to cut at. A query is the next boundary: the node
        // compares against the reference with its query, so `host?` admits
        // `host?rev=…` and not `host.evil`. With no query either, the whole
        // reference is the narrowest prefix there is.
        None if locator.len() < reference.len() => format!("{locator}?"),
        None => locator.to_string(),
    }
}

/// Derive a pod name from a reference: its repository, never its revision.
pub fn derive_name(reference: &str) -> String {
    let locator = reference.split('?').next().unwrap_or(reference);
    let locator = locator.split_once(':').map_or(locator, |(_, rest)| rest);
    let mut segments: Vec<&str> = locator.split('/').filter(|s| !s.is_empty()).collect();
    // A trailing revision names the same repository as its unpinned form; two
    // pods of the same flake at different commits should not get names that
    // differ only in 40 hex digits.
    if segments.len() > 1 && segments.last().is_some_and(|last| is_commit(last)) {
        segments.pop();
    }
    let last = segments.last().copied().unwrap_or_default();
    podspec::sanitize_name(last.strip_suffix(".git").unwrap_or(last))
}

/// Whether a failed directory pack was probably a flake reference typed
/// without `--external`.
///
/// Defined as "the node would accept this as an external reference", so the
/// hint cannot drift away from the contract it is pointing at.
pub fn looks_like_reference(candidate: &str) -> bool {
    !candidate.starts_with('/') && imageless::validate_source(candidate).is_ok()
}

fn is_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REV: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn a_path_under_external_names_the_way_back_to_packing() {
        for path in ["/abs/dir", "./dir", "../dir", "~/dir", ".", ".."] {
            let error = validate(path).unwrap_err();
            assert!(error.contains("drop --external"), "{path}: {error}");
        }
    }

    #[test]
    fn an_absolute_path_is_refused_here_though_the_library_accepts_it() {
        // The asymmetry is the whole reason this check exists: to the node a
        // leading `/` is a valid in-image path.
        assert!(imageless::validate_source("/abs/dir").is_ok());
        assert!(validate("/abs/dir").is_err());
    }

    #[test]
    fn a_fragment_is_redirected_to_the_output_flag() {
        let error = validate("github:owner/repo#server").unwrap_err();
        assert!(error.contains("--output server"), "{error}");
    }

    #[test]
    fn grammar_failures_carry_the_node_s_own_text() {
        assert!(validate("nixpkgs")
            .unwrap_err()
            .contains("registry names are not resolved"));
        assert!(validate("path:/srv/flake")
            .unwrap_err()
            .contains("node-local schemes are not allowed"));
    }

    #[test]
    fn the_three_spec_pin_forms_are_recognized() {
        assert_eq!(
            pin(&format!("github:owner/repo?rev={REV}")).unwrap(),
            Some(Pin::Revision)
        );
        assert_eq!(
            pin("tarball+http://host/f.tar.gz?narHash=sha256-abc").unwrap(),
            Some(Pin::NarHash)
        );
        assert_eq!(
            pin(&format!("github:owner/repo/{REV}")).unwrap(),
            Some(Pin::CommitPath)
        );
    }

    #[test]
    fn a_branch_is_not_a_pin_in_either_spelling() {
        assert_eq!(pin("github:owner/repo?ref=main").unwrap(), None);
        assert_eq!(pin("github:owner/repo/main").unwrap(), None);
        assert_eq!(pin("github:owner/repo").unwrap(), None);
    }

    #[test]
    fn a_commit_shaped_segment_pins_only_the_shorthand_schemes() {
        // Any path can end in 40 hex digits; only the shorthand schemes give
        // that position the meaning "revision".
        assert_eq!(pin(&format!("tarball+https://host/a/{REV}")).unwrap(), None);
        assert_eq!(
            pin(&format!("gitlab:owner/repo/{REV}")).unwrap(),
            Some(Pin::CommitPath)
        );
    }

    #[test]
    fn a_malformed_pin_is_an_error_not_an_absent_pin() {
        assert!(pin("github:owner/repo?rev=0123456")
            .unwrap_err()
            .contains("40-character"));
        assert!(pin("github:owner/repo?narHash=abc")
            .unwrap_err()
            .contains("SRI hash"));
        assert!(pin("github:owner/repo?narHash=sha256-")
            .unwrap_err()
            .contains("SRI hash"));
    }

    #[test]
    fn pin_keys_are_matched_case_insensitively_like_nix() {
        assert_eq!(
            pin(&format!("github:o/r?REV={REV}")).unwrap(),
            Some(Pin::Revision)
        );
        assert_eq!(
            pin("tarball+http://h/f?NARHASH=sha256-abc").unwrap(),
            Some(Pin::NarHash)
        );
    }

    #[test]
    fn a_pin_is_found_after_other_query_parameters() {
        assert_eq!(
            pin(&format!("github:o/r?dir=sub&rev={REV}")).unwrap(),
            Some(Pin::Revision)
        );
    }

    #[test]
    fn suggested_prefixes_stop_at_a_boundary() {
        // SPEC §3: an unterminated `github:myorg` also authorizes
        // `github:myorg-evil/anything`.
        assert_eq!(policy_prefix(&format!("github:o/r?rev={REV}")), "github:o/");
        assert_eq!(policy_prefix(&format!("github:o/r/{REV}")), "github:o/r/");
        assert_eq!(
            policy_prefix("tarball+http://127.0.0.1:8081/flake.tar.gz?narHash=sha256-abc"),
            "tarball+http://127.0.0.1:8081/"
        );
        assert_eq!(policy_prefix("https://host"), "https://host");
        // Terminated at the query, so a longer host name is not admitted.
        let prefix = policy_prefix(&format!("git+https://example.com?rev={REV}"));
        assert_eq!(prefix, "git+https://example.com?");
        assert!(!"git+https://example.com.evil/x".starts_with(&prefix));
    }

    #[test]
    fn the_suggested_prefix_actually_matches_the_reference() {
        // The node compares literal bytes, so the suggestion is only useful if
        // it really is a prefix of what the node sees.
        for reference in [
            format!("github:owner/repo?rev={REV}"),
            format!("github:owner/repo/{REV}"),
            "tarball+http://127.0.0.1:8081/flake.tar.gz?narHash=sha256-abc".to_string(),
        ] {
            assert!(
                reference.starts_with(&policy_prefix(&reference)),
                "{reference}"
            );
        }
    }

    #[test]
    fn names_come_from_the_repository_not_the_revision() {
        assert_eq!(
            derive_name(&format!("github:owner/My_Repo/{REV}")),
            "my-repo"
        );
        assert_eq!(derive_name(&format!("github:owner/repo?rev={REV}")), "repo");
        assert_eq!(derive_name("git+https://host/team/app.git"), "app");
        assert_eq!(derive_name("tarball+http://host/"), "host");
    }

    #[test]
    fn the_hint_fires_only_for_things_the_node_would_accept() {
        assert!(looks_like_reference("github:owner/repo"));
        assert!(!looks_like_reference("./dir"));
        assert!(!looks_like_reference("/abs"));
        // Rejected by the node, so pointing the user at --external would lie.
        assert!(!looks_like_reference("nixpkgs"));
        assert!(!looks_like_reference("path:/srv"));
    }
}

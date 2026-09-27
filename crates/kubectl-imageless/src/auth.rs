//! Registry credentials: docker `config.json` lookup and the Bearer dance.
//!
//! Everything here is keyed by host and knows nothing about the push itself,
//! so WP6's pin tooling reuses it unchanged. Missing config, missing entry,
//! and a helper reporting "credentials not found" are all Anonymous — the
//! zero-setup localhost path must need no `docker login`. Identity-token
//! entries are a named hard error rather than a silent wrong-auth 401.

use base64::Engine;
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg_attr(test, derive(Debug))]
pub enum Credential {
    Anonymous,
    Basic { username: String, secret: String },
}

/// Where the credential came from, for the fail-closed 401 message: a user
/// whose helper returned a stale password needs to know which store to fix,
/// and one whose store was never consulted needs to know that instead.
#[cfg_attr(test, derive(Debug))]
pub enum CredentialSource {
    /// No config file at all.
    None,
    /// A config that was read and had no entry for the host; carries the path
    /// actually consulted, which `$DOCKER_CONFIG` can move.
    Missing(String),
    ConfigAuths,
    Helper(String),
    /// The helper ran and reported no entry for the host.
    HelperMiss(String),
}

impl CredentialSource {
    pub fn describe(&self) -> String {
        match self {
            CredentialSource::None => "no docker config.json to read".to_string(),
            CredentialSource::Missing(path) => format!("no credentials for this host in {path}"),
            CredentialSource::ConfigAuths => "using credentials from config.json auths".to_string(),
            CredentialSource::Helper(helper) => {
                format!("using credentials from docker-credential-{helper}")
            }
            CredentialSource::HelperMiss(helper) => {
                format!("docker-credential-{helper} has no entry for this host")
            }
        }
    }
}

pub fn lookup(host: &str) -> Result<(Credential, CredentialSource), String> {
    lookup_in(config_path().as_deref(), host)
}

fn config_path() -> Option<PathBuf> {
    // Docker treats an empty variable as unset. Taken literally, an empty
    // DOCKER_CONFIG names the relative `config.json` — whatever the working
    // directory holds, credential helper included.
    let set = |key| std::env::var_os(key).filter(|value| !value.is_empty());
    if let Some(directory) = set("DOCKER_CONFIG") {
        return Some(PathBuf::from(directory).join("config.json"));
    }
    set("HOME").map(|home| PathBuf::from(home).join(".docker/config.json"))
}

/// Docker stores Docker Hub credentials under the legacy index URL, never
/// under the host the client actually dials.
fn config_key(host: &str) -> &str {
    match host {
        "docker.io" | "index.docker.io" | "registry-1.docker.io" => "https://index.docker.io/v1/",
        other => other,
    }
}

/// Lookup order is docker's: a per-host helper beats the global store beats
/// inline auths. `Auths` carries the key it matched, which is not always the
/// key we asked for.
enum Source {
    Helper(String),
    Auths(String),
    None,
}

fn choose_source(config: &serde_json::Value, key: &str) -> Source {
    // An empty helper name is how a user disables a store without deleting
    // the key; docker falls through to the file, so this must too.
    if let Some(helper) = config["credHelpers"][key]
        .as_str()
        .filter(|h| !h.is_empty())
    {
        return Source::Helper(helper.to_string());
    }
    if let Some(helper) = config["credsStore"].as_str().filter(|h| !h.is_empty()) {
        return Source::Helper(helper.to_string());
    }
    if config["auths"][key].is_object() {
        return Source::Auths(key.to_string());
    }
    // docker matches a stored key by its hostname, so entries written by
    // older clients and CI templates (`https://registry.example/v2/`) resolve.
    if let Some(auths) = config["auths"].as_object() {
        for stored in auths.keys() {
            if key_hostname(stored) == key_hostname(key) {
                return Source::Auths(stored.clone());
            }
        }
    }
    Source::None
}

/// docker's ConvertToHostname: drop a scheme and anything from the first
/// slash, leaving `host[:port]`.
fn key_hostname(key: &str) -> &str {
    let bare = key.split_once("://").map_or(key, |(_, rest)| rest);
    bare.split('/').next().unwrap_or(bare)
}

fn lookup_in(config: Option<&Path>, host: &str) -> Result<(Credential, CredentialSource), String> {
    let Some(config_file) = config else {
        return Ok((Credential::Anonymous, CredentialSource::None));
    };
    let bytes = match std::fs::read(config_file) {
        Ok(bytes) => bytes,
        // A file that exists but cannot be read is a broken setup, not an
        // absent one; guessing anonymous would hide it behind a 401.
        Err(error) if config_file.exists() => {
            return Err(format!(
                "{} could not be read: {error}",
                config_file.display()
            ))
        }
        Err(_) => return Ok((Credential::Anonymous, CredentialSource::None)),
    };
    let config: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is not valid JSON: {error}", config_file.display()))?;
    let key = config_key(host);
    let missing = CredentialSource::Missing(config_file.display().to_string());
    match choose_source(&config, key) {
        Source::None => Ok((Credential::Anonymous, missing)),
        Source::Helper(helper) => {
            helper_credential(&format!("docker-credential-{helper}"), &helper, key, host)
        }
        Source::Auths(key) => {
            let entry = &config["auths"][&key];
            if entry.get("identitytoken").is_some() || entry.get("registrytoken").is_some() {
                return Err(identity_token_error(host));
            }
            // An empty `auth` means "no credentials" to docker, and a config
            // it accepts must not abort the push here.
            let auth = entry["auth"].as_str().unwrap_or_default();
            if auth.is_empty() {
                // docker never writes these, but it reads them, and config
                // generators emit them.
                let username = entry["username"].as_str().unwrap_or_default();
                let password = entry["password"].as_str().unwrap_or_default();
                if username.is_empty() && password.is_empty() {
                    return Ok((Credential::Anonymous, missing));
                }
                return Ok((
                    Credential::Basic {
                        username: username.to_string(),
                        secret: password.to_string(),
                    },
                    CredentialSource::ConfigAuths,
                ));
            }
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(auth)
                .map_err(|_| format!("auths entry for `{key}` is not valid base64"))?;
            let decoded = String::from_utf8(decoded)
                .map_err(|_| format!("auths entry for `{key}` is not UTF-8"))?;
            // Split on the FIRST colon: passwords may contain colons.
            let Some((username, secret)) = decoded.split_once(':') else {
                return Err(format!(
                    "auths entry for `{key}` does not decode to user:password"
                ));
            };
            Ok((
                Credential::Basic {
                    username: username.to_string(),
                    // Legacy wincred entries pad the password with NULs.
                    secret: secret.trim_end_matches('\0').to_string(),
                },
                CredentialSource::ConfigAuths,
            ))
        }
    }
}

fn identity_token_error(host: &str) -> String {
    format!(
        "credentials for `{host}` use an identity token, which kubectl-imageless does not \
         support; run `docker login {host}` to store a username and password"
    )
}

fn helper_credential(
    program: &str,
    helper: &str,
    key: &str,
    host: &str,
) -> Result<(Credential, CredentialSource), String> {
    // The helper is always a bare `docker-credential-*` name resolved on PATH:
    // a name carrying a path separator would execute an arbitrary file, and
    // config.json is not a place to name programs from.
    if helper.contains('/') || helper.contains(std::path::MAIN_SEPARATOR) {
        return Err(format!(
            "credential helper `{helper}` for `{host}` is not a bare name — imageless runs \
             only `docker-credential-<name>` from PATH"
        ));
    }
    match run_helper(program, key) {
        Ok(Some((username, secret))) => {
            // A helper answering `<token>` is handing back an identity token.
            if username == "<token>" {
                return Err(identity_token_error(host));
            }
            Ok((
                Credential::Basic { username, secret },
                CredentialSource::Helper(helper.to_string()),
            ))
        }
        Ok(None) => Ok((
            Credential::Anonymous,
            CredentialSource::HelperMiss(helper.to_string()),
        )),
        Err(error) => Err(format!("docker-credential-{helper} for `{host}` {error}")),
    }
}

/// The docker credential-helper exec protocol: `<program> get` with the server
/// key on stdin, `{"Username":…,"Secret":…}` on stdout.
fn run_helper(program: &str, server: &str) -> Result<Option<(String, String)>, String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run: {error}"))?;
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let written = stdin.write_all(server.as_bytes());
    // The helper reads the server key up to EOF; close stdin before waiting.
    drop(stdin);
    // A helper that fails before reading its input closes the pipe. Its exit
    // status and output are the answer; our write losing the race is not.
    if let Err(error) = written {
        if error.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(format!("did not read its stdin: {error}"));
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("did not exit: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        // The protocol reports a missing entry as a failure with this exact
        // phrase; that is an anonymous push, not an error.
        if stdout.contains("credentials not found") {
            return Ok(None);
        }
        return Err(format!(
            "failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|error| format!("returned invalid JSON: {error}"))?;
    let username = parsed["Username"].as_str().unwrap_or_default().to_string();
    let Some(secret) = parsed["Secret"]
        .as_str()
        .filter(|secret| !secret.is_empty())
    else {
        return Err("returned no Secret".to_string());
    };
    Ok(Some((username, secret.to_string())))
}

pub fn basic_header(username: &str, secret: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{secret}"))
    )
}

#[cfg_attr(test, derive(Debug))]
pub enum Challenge {
    Basic,
    Bearer {
        realm: String,
        service: Option<String>,
        scope: Option<String>,
    },
}

/// A registry may offer several schemes in one header (Artifactory sends
/// Basic and Bearer together); Bearer is preferred because it scopes the
/// credential to this repository instead of replaying the password.
pub fn parse_www_authenticate(header: &str) -> Result<Challenge, String> {
    let mut basic = None;
    let mut failure = None;
    for segment in split_challenges(header) {
        match parse_one_challenge(segment) {
            Ok(bearer @ Challenge::Bearer { .. }) => return Ok(bearer),
            Ok(Challenge::Basic) => basic = Some(Challenge::Basic),
            Err(error) => failure = Some(error),
        }
    }
    basic.ok_or_else(|| {
        failure
            .unwrap_or_else(|| format!("registry sent no auth scheme imageless supports: {header}"))
    })
}

/// A challenge begins where a scheme token sits at the start of the header or
/// just after a comma; anything else that looks like a scheme is a parameter
/// value.
fn split_challenges(header: &str) -> Vec<&str> {
    let lowered = header.to_ascii_lowercase();
    let mut starts = Vec::new();
    for scheme in ["basic", "bearer"] {
        let mut from = 0;
        while let Some(offset) = lowered[from..].find(scheme) {
            let at = from + offset;
            let prefix = lowered[..at].trim_end();
            let after = &lowered[at + scheme.len()..];
            if (prefix.is_empty() || prefix.ends_with(','))
                && (after.is_empty() || after.starts_with(char::is_whitespace))
            {
                starts.push(at);
            }
            from = at + scheme.len();
        }
    }
    starts.sort_unstable();
    starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let end = starts.get(index + 1).copied().unwrap_or(header.len());
            header[start..end].trim_end_matches([' ', ',']).trim()
        })
        .collect()
}

fn parse_one_challenge(header: &str) -> Result<Challenge, String> {
    let (scheme, parameters) = header
        .split_once(char::is_whitespace)
        .unwrap_or((header, ""));
    match scheme.to_ascii_lowercase().as_str() {
        "basic" => Ok(Challenge::Basic),
        "bearer" => {
            let mut realm = None;
            let mut service = None;
            let mut scope = None;
            for (key, value) in auth_parameters(parameters) {
                match key.to_ascii_lowercase().as_str() {
                    "realm" => realm = Some(value),
                    "service" => service = Some(value),
                    "scope" => scope = Some(value),
                    _ => {}
                }
            }
            Ok(Challenge::Bearer {
                realm: realm.ok_or_else(|| {
                    format!("registry sent a Bearer challenge without a realm: {header}")
                })?,
                service,
                scope,
            })
        }
        other => Err(format!(
            "registry sent an unsupported auth scheme `{other}`"
        )),
    }
}

/// `key="value"` pairs separated by commas; quoted values may contain commas
/// (a realm URL can), so this cannot be a plain split.
fn auth_parameters(parameters: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut rest = parameters.trim();
    while !rest.is_empty() {
        let Some((key, tail)) = rest.split_once('=') else {
            break;
        };
        let key = key
            .trim_matches(|c: char| c.is_whitespace() || c == ',')
            .to_string();
        let tail = tail.trim_start();
        let (value, next) = if let Some(quoted) = tail.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((value, next)) => (value.to_string(), next),
                None => (quoted.to_string(), ""),
            }
        } else {
            match tail.split_once(',') {
                Some((value, next)) => (value.trim().to_string(), next),
                None => (tail.trim().to_string(), ""),
            }
        };
        pairs.push((key, value));
        rest = next.trim_start_matches(|c: char| c.is_whitespace() || c == ',');
    }
    pairs
}

/// GET the token endpoint named by the challenge. The challenge's scope is
/// used verbatim when present — the registry knows what it wants; otherwise
/// ask for push on the one repository being pushed.
pub fn fetch_bearer_token(
    agent: &ureq::Agent,
    challenge: &Challenge,
    name: &str,
    credential: &Credential,
    source: &CredentialSource,
    host: &str,
) -> Result<String, String> {
    let Challenge::Bearer {
        realm,
        service,
        scope,
    } = challenge
    else {
        return Err("a Basic challenge has no token endpoint".to_string());
    };
    // The realm is registry-supplied: an http endpoint would carry the
    // password in cleartext and hand back a push-capable token, so only
    // https — or a loopback realm, which is the dev-registry case — is used.
    let realm_host = realm
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(rest))
        .unwrap_or_default();
    if !realm.starts_with("https://") && !crate::registry::is_loopback(realm_host) {
        return Err(format!(
            "registry `{host}` pointed authentication at `{realm}`, which is not https — \
             refusing to send credentials in cleartext"
        ));
    }
    let scope = scope
        .clone()
        .unwrap_or_else(|| format!("repository:{name}:pull,push"));
    let mut url = format!(
        "{realm}{}scope={}",
        if realm.contains('?') { '&' } else { '?' },
        percent_encode(&scope)
    );
    if let Some(service) = service {
        url.push_str("&service=");
        url.push_str(&percent_encode(service));
    }
    let mut request = agent.get(&url);
    if let Credential::Basic { username, secret } = credential {
        request = request.header("Authorization", basic_header(username, secret));
    }
    let mut response = request
        .call()
        .map_err(|error| format!("token endpoint `{realm}` is unreachable: {error}"))?;
    let status = response.status().as_u16();
    if status != 200 {
        // Wrong or stale credentials for a Bearer registry surface here, not
        // as a second registry 401, so the remedy belongs here too.
        return Err(format!(
            "token endpoint `{realm}` refused to issue a token for `{name}` (HTTP {status}, {}); \
             run `docker login {host}` and retry",
            source.describe()
        ));
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(256 * 1024)
        .read_to_vec()
        .map_err(|error| format!("token endpoint `{realm}` response unreadable: {error}"))?;
    let parsed: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|error| format!("token endpoint `{realm}` returned invalid JSON: {error}"))?;
    let token = parsed["token"]
        .as_str()
        .or_else(|| parsed["access_token"].as_str())
        .ok_or_else(|| format!("token endpoint `{realm}` returned no token"))?;
    Ok(token.to_string())
}

/// Query-component percent-encoding, unreserved characters only. A scope like
/// `repository:team/app:pull,push` round-trips through any spec-conforming
/// server whether or not it decodes eagerly.
pub fn percent_encode(raw: &str) -> String {
    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one test that touches DOCKER_CONFIG: cargo runs tests as threads of
    /// one process, so nothing else in this binary may set it.
    #[test]
    fn an_empty_docker_config_is_unset_not_the_working_directory() {
        let previous = std::env::var_os("DOCKER_CONFIG");
        std::env::set_var("DOCKER_CONFIG", "");
        let resolved = config_path();
        match previous {
            Some(previous) => std::env::set_var("DOCKER_CONFIG", previous),
            None => std::env::remove_var("DOCKER_CONFIG"),
        }
        let expected = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(|home| PathBuf::from(home).join(".docker/config.json"));
        assert_eq!(resolved, expected);
        assert_ne!(resolved, Some(PathBuf::from("config.json")));
    }

    fn fixture(label: &str, config: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "kubectl-imageless-auth-{label}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.json");
        std::fs::write(&path, config).unwrap();
        path
    }

    #[test]
    fn www_authenticate_bearer_with_quoted_params_parses() {
        let challenge = parse_www_authenticate(
            "Bearer realm=\"https://auth.example/token,v2\",service=\"registry.example\",\
             scope=\"repository:team/app:pull,push\"",
        )
        .unwrap();
        let Challenge::Bearer {
            realm,
            service,
            scope,
        } = challenge
        else {
            panic!("expected a bearer challenge");
        };
        assert_eq!(realm, "https://auth.example/token,v2");
        assert_eq!(service.as_deref(), Some("registry.example"));
        assert_eq!(scope.as_deref(), Some("repository:team/app:pull,push"));
    }

    #[test]
    fn basic_challenge_parses() {
        assert!(matches!(
            parse_www_authenticate("Basic realm=\"registry\"").unwrap(),
            Challenge::Basic
        ));
    }

    #[test]
    fn unknown_scheme_is_a_named_error() {
        let error = parse_www_authenticate("Negotiate").unwrap_err();
        assert!(error.contains("Negotiate"), "{error}");
    }

    #[test]
    fn bearer_wins_when_a_registry_offers_both_schemes() {
        // Artifactory answers with Basic first; taking it would replay the
        // password on every request instead of a repository-scoped token.
        let header = "Basic realm=\"Artifactory Realm\", \
                      Bearer realm=\"https://art.example/token\",service=\"art\"";
        let Challenge::Bearer { realm, service, .. } = parse_www_authenticate(header).unwrap()
        else {
            panic!("expected the bearer challenge");
        };
        assert_eq!(realm, "https://art.example/token");
        assert_eq!(service.as_deref(), Some("art"));
        // Order must not matter.
        assert!(matches!(
            parse_www_authenticate("Bearer realm=\"https://a/t\", Basic realm=\"r\"").unwrap(),
            Challenge::Bearer { .. }
        ));
        // A scheme name inside a quoted value is not a second challenge.
        assert!(matches!(
            parse_www_authenticate("Basic realm=\"use bearer instead\"").unwrap(),
            Challenge::Basic
        ));
    }

    #[test]
    fn a_cleartext_token_realm_is_refused() {
        let agent: ureq::Agent = ureq::Agent::config_builder().build().into();
        let challenge =
            parse_www_authenticate("Bearer realm=\"http://evil.example/token\"").unwrap();
        let error = fetch_bearer_token(
            &agent,
            &challenge,
            "team/app",
            &Credential::Basic {
                username: "user".to_string(),
                secret: "pw".to_string(),
            },
            &CredentialSource::ConfigAuths,
            "registry.example",
        )
        .unwrap_err();
        assert!(error.contains("not https"), "{error}");
    }

    #[test]
    fn a_bearer_challenge_needs_a_realm() {
        let error = parse_www_authenticate("Bearer service=\"x\"").unwrap_err();
        assert!(error.contains("realm"), "{error}");
    }

    #[test]
    fn auths_entry_base64_decodes_splitting_on_the_first_colon() {
        use base64::Engine;
        let auth = base64::engine::general_purpose::STANDARD.encode("user:pa:ss");
        let path = fixture(
            "auths",
            &format!("{{\"auths\":{{\"registry.example\":{{\"auth\":\"{auth}\"}}}}}}"),
        );
        let (credential, source) = lookup_in(Some(&path), "registry.example").unwrap();
        let Credential::Basic { username, secret } = credential else {
            panic!("expected basic credentials");
        };
        assert_eq!(username, "user");
        assert_eq!(secret, "pa:ss");
        assert!(matches!(source, CredentialSource::ConfigAuths));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn cred_helpers_win_over_creds_store_and_auths() {
        let config: serde_json::Value = serde_json::from_str(
            "{\"credHelpers\":{\"registry.example\":\"vault\"},\"credsStore\":\"desktop\",\
             \"auths\":{\"registry.example\":{\"auth\":\"eDp5\"}}}",
        )
        .unwrap();
        assert!(matches!(
            choose_source(&config, "registry.example"),
            Source::Helper(helper) if helper == "vault"
        ));
        assert!(matches!(
            choose_source(&config, "other.example"),
            Source::Helper(helper) if helper == "desktop"
        ));
        let auths_only: serde_json::Value =
            serde_json::from_str("{\"auths\":{\"registry.example\":{\"auth\":\"eDp5\"}}}").unwrap();
        assert!(matches!(
            choose_source(&auths_only, "registry.example"),
            Source::Auths(key) if key == "registry.example"
        ));
        assert!(matches!(
            choose_source(&auths_only, "other.example"),
            Source::None
        ));
    }

    #[test]
    fn an_emptied_store_falls_through_to_auths_like_docker() {
        let config: serde_json::Value = serde_json::from_str(
            "{\"credsStore\":\"\",\"credHelpers\":{\"other.example\":\"\"},\
             \"auths\":{\"registry.example\":{\"auth\":\"eDp5\"}}}",
        )
        .unwrap();
        assert!(matches!(
            choose_source(&config, "registry.example"),
            Source::Auths(_)
        ));
        assert!(matches!(
            choose_source(&config, "other.example"),
            Source::None
        ));
    }

    #[test]
    fn a_scheme_prefixed_auths_key_still_matches_the_host() {
        let config: serde_json::Value = serde_json::from_str(
            "{\"auths\":{\"https://registry.example/v2/\":{\"auth\":\"eDp5\"}}}",
        )
        .unwrap();
        assert!(matches!(
            choose_source(&config, "registry.example"),
            Source::Auths(key) if key == "https://registry.example/v2/"
        ));
        assert_eq!(
            key_hostname("https://registry.example/v2/"),
            "registry.example"
        );
        assert_eq!(key_hostname("localhost:5001"), "localhost:5001");
    }

    #[test]
    fn an_empty_auth_string_is_anonymous_not_an_error() {
        let path = fixture(
            "empty-auth",
            "{\"auths\":{\"registry.example\":{\"auth\":\"\"}}}",
        );
        let (credential, _) = lookup_in(Some(&path), "registry.example").unwrap();
        assert!(matches!(credential, Credential::Anonymous));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn explicit_username_and_password_fields_are_read() {
        let path = fixture(
            "explicit",
            "{\"auths\":{\"registry.example\":{\"username\":\"user\",\"password\":\"pw\"}}}",
        );
        let (credential, _) = lookup_in(Some(&path), "registry.example").unwrap();
        let Credential::Basic { username, secret } = credential else {
            panic!("expected the explicit fields");
        };
        assert_eq!((username.as_str(), secret.as_str()), ("user", "pw"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_helper_name_with_a_path_separator_is_refused() {
        let error = helper_credential(
            "docker-credential-../../evil",
            "../../evil",
            "registry.example",
            "registry.example",
        )
        .unwrap_err();
        assert!(error.contains("not a bare name"), "{error}");
    }

    #[test]
    fn docker_hub_host_maps_to_the_index_key() {
        for host in ["docker.io", "index.docker.io", "registry-1.docker.io"] {
            assert_eq!(config_key(host), "https://index.docker.io/v1/");
        }
        assert_eq!(config_key("ghcr.io"), "ghcr.io");
        let path = fixture(
            "hub",
            "{\"auths\":{\"https://index.docker.io/v1/\":{\"auth\":\"aHViOnNlY3JldA==\"}}}",
        );
        let (credential, _) = lookup_in(Some(&path), "docker.io").unwrap();
        let Credential::Basic { username, .. } = credential else {
            panic!("expected the hub entry");
        };
        assert_eq!(username, "hub");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn missing_config_and_missing_entry_are_anonymous() {
        let (credential, source) =
            lookup_in(Some(Path::new("/nonexistent/config.json")), "r.example").unwrap();
        assert!(matches!(credential, Credential::Anonymous));
        assert!(matches!(source, CredentialSource::None));
        let path = fixture("empty", "{}");
        let (credential, _) = lookup_in(Some(&path), "r.example").unwrap();
        assert!(matches!(credential, Credential::Anonymous));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn identity_token_entry_is_a_named_error() {
        let path = fixture(
            "identity",
            "{\"auths\":{\"registry.example\":{\"auth\":\"eDp5\",\"identitytoken\":\"t\"}}}",
        );
        let error = lookup_in(Some(&path), "registry.example").unwrap_err();
        assert!(error.contains("identity token"), "{error}");
        assert!(error.contains("docker login registry.example"), "{error}");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn helper_script(label: &str, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "kubectl-imageless-helper-{label}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("docker-credential-fake");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Exec of a freshly written script fails with ETXTBSY (os error 26) when
    /// a sibling test thread forks while this file's write descriptor is still
    /// open — the child inherits it, and execve refuses a file open for
    /// writing. Retrying the spawn is the only fix available to a test that
    /// writes its own executable.
    fn helper_get(program: &str, server: &str) -> Result<Option<(String, String)>, String> {
        for _ in 0..50 {
            match run_helper(program, server) {
                Err(error) if error.contains("Text file busy") => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                settled => return settled,
            }
        }
        run_helper(program, server)
    }

    /// The helper exits without reading stdin, so the write loses the race
    /// with the child: an EPIPE there must not mask the helper's own answer.
    #[test]
    fn helper_not_found_exit_means_anonymous() {
        let path = helper_script(
            "notfound",
            "#!/bin/sh\necho credentials not found in native keychain\nexit 1\n",
        );
        assert!(helper_get(path.to_str().unwrap(), "registry.example")
            .unwrap()
            .is_none());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn helper_output_becomes_basic_credentials() {
        let path = helper_script(
            "ok",
            "#!/bin/sh\nread -r server\n\
             printf '{\"ServerURL\":\"%s\",\"Username\":\"user\",\"Secret\":\"s3cret\"}' \
             \"$server\"\n",
        );
        let (username, secret) = helper_get(path.to_str().unwrap(), "registry.example")
            .unwrap()
            .unwrap();
        assert_eq!(username, "user");
        assert_eq!(secret, "s3cret");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_failing_helper_is_an_error_not_anonymous() {
        let path = helper_script("broken", "#!/bin/sh\necho keychain exploded >&2\nexit 3\n");
        let error = helper_get(path.to_str().unwrap(), "registry.example").unwrap_err();
        assert!(error.contains("keychain exploded"), "{error}");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn scopes_percent_encode_for_a_query_component() {
        assert_eq!(
            percent_encode("repository:team/app:pull,push"),
            "repository%3Ateam%2Fapp%3Apull%2Cpush"
        );
        assert_eq!(percent_encode("sha256:abc"), "sha256%3Aabc");
    }
}

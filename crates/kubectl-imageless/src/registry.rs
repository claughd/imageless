//! OCI distribution-spec push client: blob HEAD/POST/PUT, manifest PUT, and
//! the 401 → authenticate → retry-once dance.
//!
//! This module knows nothing about pods, packing, or Nix — `put_manifest`
//! taking a tag-or-digest reference is the seam WP6's pin tooling calls.
//! Blobs are uploaded monolithically (one POST, one PUT): seed images are
//! bounded far below any real chunking threshold, and the error for an
//! oversized upload says the fallback does not exist.
//!
//! Statuses are read the way registries actually answer, not the way the spec
//! mandates: any 2xx completes a write (the spec says 201, GCR answers 200),
//! and a 3xx on a blob HEAD means the blob exists behind a storage redirect
//! (distribution's S3 driver presigns HEAD for blobs it holds).

use crate::auth::{self, Challenge, Credential, CredentialSource};
use std::time::Duration;

pub struct Registry {
    /// `scheme://host[:port]` actually dialed.
    base_url: String,
    /// Host as the user wrote it, for messages and `docker login` advice.
    host: String,
    /// Repository path after the host, e.g. `team/app`.
    name: String,
    agent: ureq::Agent,
    credential: Credential,
    credential_source: CredentialSource,
    /// Full `Authorization` header value once a challenge has been answered.
    authorization: Option<String>,
    /// Plain HTTP to a host that is not loopback: the user's --plain-http is
    /// consent to send this host credentials in cleartext, and to nobody else.
    cleartext: bool,
    warned_cleartext: bool,
}

/// Generous because a blob PUT is one request: the whole seed uploads inside
/// a single timeout window, even through a slow tunnel.
const TIMEOUT: Duration = Duration::from_secs(120);

impl Registry {
    pub fn connect(repo: &str, plain_http: bool) -> Result<Registry, String> {
        let (host, name) = split_repo(repo);
        let scheme = if plain_http || is_loopback(host) {
            "http"
        } else {
            "https"
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            // Redirects are never followed: the spec's own redirect (the
            // upload Location) arrives as a header, and a storage redirect on
            // a blob HEAD is an existence answer, not a resource to fetch.
            .max_redirects(0)
            // Error statuses carry the challenge and the spec error envelope;
            // they must come back as responses, not opaque errors.
            .http_status_as_error(false)
            .build()
            .into();
        let (credential, credential_source) = auth::lookup(host)?;
        let mut registry = Registry {
            base_url: format!("{scheme}://{}", dial_host(host)),
            host: host.to_string(),
            name: name.to_string(),
            agent,
            credential,
            credential_source,
            authorization: None,
            cleartext: scheme == "http" && !is_loopback(host),
            warned_cleartext: false,
        };
        // The /v2/ ping proves the API exists and surfaces the auth challenge
        // before any upload starts.
        let url = format!("{}/v2/", registry.base_url);
        let response = registry.call(Method::Get, &url, None, None)?;
        match response.status().as_u16() {
            200 => Ok(registry),
            status => Err(format!(
                "`{host}` does not speak the OCI distribution API (GET {url} returned {status})"
            )),
        }
    }

    /// HEAD first, upload only on 404 — re-pushing an existing seed is a
    /// handful of cheap requests.
    pub fn ensure_blob(&mut self, digest: &str, bytes: &[u8]) -> Result<(), String> {
        let head_url = format!("{}/v2/{}/blobs/{digest}", self.base_url, self.name);
        let response = self.call(Method::Head, &head_url, None, None)?;
        let status = response.status().as_u16();
        // A redirect to storage is how a redirect-capable backend says "held".
        if status == 200 || (300..400).contains(&status) {
            eprintln!("blob     {digest} already present");
            return Ok(());
        }
        if status != 404 {
            return Err(self.failure("blob check", &head_url, response));
        }
        let start_url = format!("{}/v2/{}/blobs/uploads/", self.base_url, self.name);
        let response = self.call(Method::Post, &start_url, None, Some(&[]))?;
        match response.status().as_u16() {
            202 => {}
            404 => {
                return Err(format!(
                    "repository `{}` not found on `{}` — some registries (ECR) require \
                     creating the repository before the first push",
                    self.name, self.host
                ))
            }
            _ => return Err(self.failure("blob upload start", &start_url, response)),
        }
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                format!(
                    "registry `{}` began an upload without a Location header",
                    self.host
                )
            })?;
        let put_url = append_digest(&absolutize(&start_url, location), digest);
        let response = self.call(
            Method::Put,
            &put_url,
            Some("application/octet-stream"),
            Some(bytes),
        )?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        if status == 413 {
            return Err(format!(
                "blob upload of {digest} ({} bytes) to `{put_url}` was refused as too large \
                 (HTTP 413); there is no chunked fallback — a seed this size should never \
                 exist, check what was packed",
                bytes.len()
            ));
        }
        let (code, detail) = self.detail(response);
        // The digest is in the upload URL, so only the envelope's own code can
        // distinguish a content mismatch from an expired session or a bad name.
        if code.as_deref() == Some("DIGEST_INVALID") {
            return Err(format!(
                "registry `{}` rejected {digest} as a digest mismatch — the pushed bytes are \
                 deterministic, so this is an imageless bug",
                self.host
            ));
        }
        Err(format!(
            "blob upload of {digest} ({} bytes) to `{put_url}` failed (HTTP {status} {detail})",
            bytes.len()
        ))
    }

    /// `reference` is a tag or a digest — pushing by digest is the normal
    /// path; a tag is one more PUT of the identical bytes.
    pub fn put_manifest(
        &mut self,
        reference: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        let url = format!("{}/v2/{}/manifests/{reference}", self.base_url, self.name);
        let response = self.call(Method::Put, &url, Some(media_type), Some(bytes))?;
        if !(200..300).contains(&response.status().as_u16()) {
            return Err(self.failure("manifest PUT", &url, response));
        }
        // The registry echoes the digest it stored; a disagreement means the
        // pushed image is not the one the pod manifest pins.
        if let Some(returned) = response
            .headers()
            .get("docker-content-digest")
            .and_then(|value| value.to_str().ok())
        {
            let expected = crate::oci::digest(bytes);
            if returned != expected {
                return Err(format!(
                    "registry `{}` stored the manifest as {returned}, but its digest is \
                     {expected} — refusing the mismatched push",
                    self.host
                ));
            }
        }
        Ok(())
    }

    /// One request with the current Authorization header; on a 401, answer
    /// the challenge and retry exactly once.
    fn call(
        &mut self,
        method: Method,
        url: &str,
        content_type: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<ureq::http::Response<ureq::Body>, String> {
        let mut authenticated = false;
        loop {
            let response = self
                .dispatch(method, url, content_type, body)
                .map_err(|error| self.transport_error(url, error))?;
            if response.status().as_u16() == 401 {
                if !same_origin(&self.base_url, url) {
                    return Err(format!(
                        "`{url}` demanded authentication, but it is not `{}` — imageless does \
                         not send registry credentials to a redirect target",
                        self.host
                    ));
                }
                // Retrying without having gained a header would replay the
                // same unauthenticated request and burn the one retry.
                if !authenticated && self.authenticate(&response)? {
                    authenticated = true;
                    continue;
                }
                return Err(self.refused());
            }
            return Ok(response);
        }
    }

    fn dispatch(
        &self,
        method: Method,
        url: &str,
        content_type: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        // The upload Location may point at another host (a storage backend's
        // presigned URL); the registry's Authorization header stays home.
        let authorization = self
            .authorization
            .as_deref()
            .filter(|_| same_origin(&self.base_url, url));
        match method {
            Method::Get | Method::Head => {
                let mut request = match method {
                    Method::Get => self.agent.get(url),
                    _ => self.agent.head(url),
                };
                if let Some(authorization) = authorization {
                    request = request.header("Authorization", authorization);
                }
                request.call()
            }
            Method::Post | Method::Put => {
                let mut request = match method {
                    Method::Post => self.agent.post(url),
                    _ => self.agent.put(url),
                };
                if let Some(authorization) = authorization {
                    request = request.header("Authorization", authorization);
                }
                if let Some(content_type) = content_type {
                    request = request.header("Content-Type", content_type);
                }
                request.send(body.unwrap_or(&[]))
            }
        }
    }

    /// Said once per push: the credential is about to cross the wire in
    /// cleartext, with the user's --plain-http as the only consent.
    fn warn_cleartext(&mut self) {
        if !self.warned_cleartext {
            self.warned_cleartext = true;
            eprintln!(
                "warning: sending the credentials for `{}` over plain HTTP (--plain-http); \
                 anyone on the network path can read them",
                self.host
            );
        }
    }

    /// Answers the challenge; `false` means no header was gained, so the
    /// caller must not retry.
    fn authenticate(
        &mut self,
        response: &ureq::http::Response<ureq::Body>,
    ) -> Result<bool, String> {
        let Some(header) = response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok())
        else {
            return Ok(false);
        };
        match auth::parse_www_authenticate(header)? {
            Challenge::Basic => {
                let Credential::Basic { username, secret } = &self.credential else {
                    return Ok(false);
                };
                let header = auth::basic_header(username, secret);
                if self.cleartext {
                    self.warn_cleartext();
                }
                self.authorization = Some(header);
                Ok(true)
            }
            challenge @ Challenge::Bearer { .. } => {
                let token = auth::fetch_bearer_token(
                    &self.agent,
                    &challenge,
                    &self.name,
                    &self.credential,
                    &self.credential_source,
                    &self.host,
                    self.cleartext.then_some(self.host.as_str()),
                )?;
                if self.cleartext && matches!(self.credential, Credential::Basic { .. }) {
                    self.warn_cleartext();
                }
                self.authorization = Some(format!("Bearer {token}"));
                Ok(true)
            }
        }
    }

    fn refused(&self) -> String {
        format!(
            "registry `{}` refused authentication for `{}` ({}); run `docker login {}` and retry",
            self.host,
            self.name,
            self.credential_source.describe(),
            self.host
        )
    }

    fn transport_error(&self, url: &str, error: ureq::Error) -> String {
        // Only a real handshake failure may suggest plain HTTP: telling a user
        // to drop TLS because a certificate did not verify is bad advice.
        if let ureq::Error::Tls(reason) = &error {
            return format!(
                "TLS handshake with `{}` failed ({reason}) — if this is a plain-HTTP \
                 registry, pass --plain-http",
                self.host
            );
        }
        if let ureq::Error::Rustls(reason) = &error {
            return format!(
                "TLS with `{}` failed: {reason} — the certificate must be trusted by the \
                 system store; imageless does not take a custom CA",
                self.host
            );
        }
        format!("cannot reach `{url}`: {error}")
    }

    /// Map a non-success response to the most specific message its status and
    /// spec error envelope allow.
    fn failure(
        &self,
        action: &str,
        url: &str,
        response: ureq::http::Response<ureq::Body>,
    ) -> String {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("an unspecified delay")
            .to_string();
        let (_, detail) = self.detail(response);
        match status {
            403 if self.authorization.is_some() => format!(
                "authenticated to `{}` but not permitted to push `{}` (HTTP 403 {detail})",
                self.host, self.name
            ),
            403 => format!(
                "anonymous push to `{}` was denied (HTTP 403 {detail}); run `docker login {}` \
                 and retry",
                self.host, self.host
            ),
            429 => format!(
                "registry `{}` rate-limited the push (HTTP 429); retry after {retry_after}",
                self.host
            ),
            _ => format!("{action} to `{url}` failed (HTTP {status} {detail})"),
        }
    }

    /// The spec error envelope's code (when it has one) and a printable
    /// detail string.
    fn detail(&self, mut response: ureq::http::Response<ureq::Body>) -> (Option<String>, String) {
        let body = response
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_vec()
            .unwrap_or_default();
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&body) {
            let error = &parsed["errors"][0];
            let code = error["code"].as_str();
            let message = error["message"].as_str();
            if let (Some(code), Some(message)) = (code, message) {
                return (
                    Some(code.to_string()),
                    printable(&format!("{code}: {message}")),
                );
            }
        }
        let snippet = printable(&String::from_utf8_lossy(&body));
        if snippet.trim().is_empty() {
            (None, "with an empty body".to_string())
        } else {
            (None, snippet.chars().take(200).collect())
        }
    }
}

#[derive(Clone, Copy)]
enum Method {
    Get,
    Head,
    Post,
    Put,
}

/// Registry-supplied text reaches a terminal, so control characters — escape
/// sequences that could rewrite what the user sees — never survive.
pub(crate) fn printable(raw: &str) -> String {
    raw.chars()
        .map(|character| {
            if character == '\n' || character == '\t' {
                ' '
            } else if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

/// `HOST/REPO` splits at the first slash; parse_run has already required one.
fn split_repo(repo: &str) -> (&str, &str) {
    repo.split_once('/').unwrap_or((repo, ""))
}

/// Docker Hub's API lives on a different host than the name everyone writes;
/// every mainstream client rewrites it for the wire.
fn dial_host(host: &str) -> &str {
    match host {
        "docker.io" | "index.docker.io" => "registry-1.docker.io",
        other => other,
    }
}

/// Loopback gets plain http automatically — the kind quickstart's
/// localhost:5001 must work with zero flags — and nothing else does: no
/// https-then-fallback probing, no RFC1918 allowance. A `.localhost` name is
/// only trusted once it resolves to a loopback address; RFC 6761 makes that
/// the convention, not a guarantee, and /etc/hosts can say otherwise.
pub fn is_loopback(host: &str) -> bool {
    let (bare, port) = split_host_port(host);
    if bare == "127.0.0.1" || bare == "::1" {
        return true;
    }
    if bare != "localhost" && !bare.ends_with(".localhost") {
        return false;
    }
    use std::net::ToSocketAddrs;
    match (bare, port.unwrap_or(80)).to_socket_addrs() {
        Ok(mut addresses) => addresses.all(|address| address.ip().is_loopback()),
        // Unresolvable: https, and the failure names --plain-http.
        Err(_) => false,
    }
}

/// Splits a `host[:port]`, keeping a bracketed IPv6 literal intact.
fn split_host_port(host: &str) -> (&str, Option<u16>) {
    if let Some(rest) = host.strip_prefix('[') {
        let (inside, after) = rest.split_once(']').unwrap_or((rest, ""));
        return (inside, after.strip_prefix(':').and_then(|p| p.parse().ok()));
    }
    match host.rsplit_once(':') {
        Some((before, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            (before, port.parse().ok())
        }
        _ => (host, None),
    }
}

/// Whether a URL is the registry itself, so the Authorization header may ride
/// along. Compared as origins: a proxy-built Location naming the default port
/// explicitly is still the same registry, while a presigned storage URL on
/// another host must never see the token.
fn same_origin(base_url: &str, url: &str) -> bool {
    match (origin(base_url), origin(url)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// `(scheme, host, port)` with the default port filled in and the host
/// lowercased, since neither is case-sensitive.
fn origin(url: &str) -> Option<(String, String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // Credentials in an authority are not something this client emits.
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let (host, port) = split_host_port(authority);
    Some((
        scheme,
        host.to_ascii_lowercase(),
        port.unwrap_or(default_port),
    ))
}

/// The upload Location may be absolute (possibly on another host), rooted, or
/// relative to the request that produced it — RFC 3986 resolution, not a
/// guess against the registry root.
fn absolutize(request_url: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let Some(scheme_end) = request_url.find("://") else {
        return location.to_string();
    };
    let authority_start = scheme_end + 3;
    let authority_end = request_url[authority_start..]
        .find('/')
        .map(|index| authority_start + index)
        .unwrap_or(request_url.len());
    if location.starts_with('/') {
        return format!("{}{location}", &request_url[..authority_end]);
    }
    let path_end = request_url[authority_end..]
        .find(['?', '#'])
        .map(|index| authority_end + index)
        .unwrap_or(request_url.len());
    let directory_end = request_url[authority_end..path_end]
        .rfind('/')
        .map(|index| authority_end + index + 1)
        .unwrap_or(authority_end);
    format!("{}{location}", &request_url[..directory_end])
}

/// The Location is allowed to carry its own query parameters (upload session
/// state); the digest must append, not replace.
fn append_digest(url: &str, digest: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}digest={}", auth::percent_encode(digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_name_split_at_the_first_slash() {
        assert_eq!(
            split_repo("registry.example/team/app"),
            ("registry.example", "team/app")
        );
        assert_eq!(split_repo("localhost:5001/app"), ("localhost:5001", "app"));
    }

    #[test]
    fn loopback_hosts_get_plain_http_and_others_do_not() {
        for host in ["localhost", "localhost:5001", "127.0.0.1", "127.0.0.1:5001"] {
            assert!(is_loopback(host), "{host}");
        }
        assert!(is_loopback("[::1]"));
        assert!(is_loopback("[::1]:5001"));
        for host in [
            "registry.example",
            "registry.example:5001",
            "127.0.0.1.evil.example",
            "notlocalhost",
            "10.0.0.7:5000",
        ] {
            assert!(!is_loopback(host), "{host}");
        }
    }

    #[test]
    fn docker_hub_is_dialed_on_its_api_host() {
        assert_eq!(dial_host("docker.io"), "registry-1.docker.io");
        assert_eq!(dial_host("index.docker.io"), "registry-1.docker.io");
        assert_eq!(dial_host("ghcr.io"), "ghcr.io");
    }

    #[test]
    fn upload_location_preserves_existing_query_params() {
        assert_eq!(
            append_digest("http://r/v2/app/blobs/uploads/1?state=x", "sha256:ab"),
            "http://r/v2/app/blobs/uploads/1?state=x&digest=sha256%3Aab"
        );
        assert_eq!(
            append_digest("http://r/v2/app/blobs/uploads/1", "sha256:ab"),
            "http://r/v2/app/blobs/uploads/1?digest=sha256%3Aab"
        );
    }

    #[test]
    fn locations_resolve_against_the_request_that_produced_them() {
        let request = "http://r:5001/v2/team/app/blobs/uploads/";
        assert_eq!(
            absolutize(request, "https://bucket.example/presigned?sig=1"),
            "https://bucket.example/presigned?sig=1"
        );
        assert_eq!(
            absolutize(request, "/v2/team/app/blobs/uploads/1"),
            "http://r:5001/v2/team/app/blobs/uploads/1"
        );
        // Relative: against the request's directory, not the host root.
        assert_eq!(
            absolutize(request, "session-1?state=x"),
            "http://r:5001/v2/team/app/blobs/uploads/session-1?state=x"
        );
        assert_eq!(
            absolutize("http://r:5001/v2/app/blobs/uploads/?a=b", "s1"),
            "http://r:5001/v2/app/blobs/uploads/s1"
        );
    }

    #[test]
    fn same_origin_compares_origins_not_prefixes() {
        // A default port written out is still the same registry.
        assert!(same_origin(
            "https://registry.example",
            "https://registry.example:443/v2/app/blobs/uploads/1"
        ));
        assert!(same_origin(
            "http://registry.example:80",
            "http://REGISTRY.example/v2/"
        ));
        // A presigned storage URL never sees the registry's token.
        assert!(!same_origin(
            "https://registry.example",
            "https://bucket.example/presigned"
        ));
        assert!(!same_origin(
            "https://registry.example",
            "http://registry.example/v2/"
        ));
        assert!(!same_origin(
            "https://registry.example",
            "https://registry.example.evil.test/v2/"
        ));
    }

    #[test]
    fn registry_text_cannot_carry_escape_sequences_to_the_terminal() {
        let cleaned = printable("bad\u{1b}[2Jname\u{7}\ndone");
        assert!(!cleaned.contains('\u{1b}'), "{cleaned}");
        assert!(!cleaned.contains('\u{7}'), "{cleaned}");
        assert!(!cleaned.contains('\n'), "{cleaned}");
        assert!(cleaned.starts_with("bad"), "{cleaned}");
        assert!(cleaned.ends_with("done"), "{cleaned}");
    }
}

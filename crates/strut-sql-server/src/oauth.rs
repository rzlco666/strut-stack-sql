//! OAuth 2.1 resource-server support (PRD-F1-07).
//!
//! The HTTP transport accepts Bearer JWTs issued by an external
//! authorization server when `STRUT_STACK_SQL_OAUTH_ISSUER` /
//! `STRUT_STACK_SQL_OAUTH_AUDIENCE` are configured:
//!
//! * discovery: fetch `/.well-known/oauth-authorization-server` (falling
//!   back to `/.well-known/openid-configuration`), then the advertised
//!   `jwks_uri`;
//! * validation: RS256 signature, `exp`, `iss`, `aud`; the principal id
//!   comes from a configurable claim (default `sub`);
//! * failure mode: if discovery/JWKS fetching fails at startup, OAuth
//!   tokens are rejected (fail closed). API keys keep working. There is no
//!   token issuance and no built-in authorization server.
//!
//! Token material is never logged.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Serialize;
use serde_json::Value;

use strut_sql_core::auth::{AuthMethod, Principal};
use strut_sql_core::config::OAuthSettings;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Validates access tokens against an external AS's JWKS.
#[derive(Clone)]
pub struct OAuthValidator {
    issuer: String,
    audience: String,
    subject_claim: String,
    keys: Arc<JwkSet>,
}

impl std::fmt::Debug for OAuthValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthValidator")
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .field("subject_claim", &self.subject_claim)
            .field("keys", &self.keys.keys.len())
            .finish()
    }
}

impl OAuthValidator {
    /// Fetch AS metadata + JWKS from the configured issuer.
    pub async fn discover(settings: &OAuthSettings) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(DISCOVERY_TIMEOUT)
            .build()
            .context("building OAuth discovery HTTP client")?;
        let issuer = settings.issuer.trim_end_matches('/');

        let mut jwks_uri = None;
        let mut last_error = String::from("no metadata endpoint attempted");
        for candidate in [
            format!("{issuer}/.well-known/oauth-authorization-server"),
            format!("{issuer}/.well-known/openid-configuration"),
        ] {
            match fetch_metadata(&client, &candidate, issuer).await {
                Ok(uri) => {
                    jwks_uri = Some(uri);
                    break;
                }
                Err(err) => last_error = format!("{candidate}: {err:#}"),
            }
        }
        let jwks_uri = jwks_uri.context(format!(
            "no usable authorization-server metadata from issuer '{issuer}' ({last_error})"
        ))?;

        let response = client
            .get(&jwks_uri)
            .send()
            .await
            .context("fetching JWKS")?;
        if !response.status().is_success() {
            bail!("JWKS endpoint returned HTTP {}", response.status());
        }
        let keys: JwkSet = response.json().await.context("parsing JWKS")?;
        if keys.keys.is_empty() {
            bail!("JWKS at {jwks_uri} contains no keys");
        }

        Ok(Self {
            issuer: settings.issuer.clone(),
            audience: settings.audience.clone(),
            subject_claim: settings.subject_claim.clone(),
            keys: Arc::new(keys),
        })
    }

    /// Build a validator from an already-fetched JWK set (tests, embedders).
    pub fn from_jwks(settings: &OAuthSettings, keys: JwkSet) -> Self {
        Self {
            issuer: settings.issuer.clone(),
            audience: settings.audience.clone(),
            subject_claim: settings.subject_claim.clone(),
            keys: Arc::new(keys),
        }
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn audience(&self) -> &str {
        &self.audience
    }

    /// Discovery document for `/.well-known/oauth-protected-resource`.
    pub fn protected_resource_metadata(&self) -> ProtectedResourceMetadata {
        ProtectedResourceMetadata {
            resource: self.audience.clone(),
            authorization_servers: vec![self.issuer.clone()],
            bearer_methods_supported: vec!["header".to_string()],
            scopes_supported: None,
        }
    }

    /// Value for the `WWW-Authenticate` header on a 401.
    pub fn www_authenticate(&self, invalid_token: bool) -> String {
        let metadata = resource_metadata_url(&self.audience);
        if invalid_token {
            format!("Bearer error=\"invalid_token\", resource_metadata=\"{metadata}\"")
        } else {
            format!("Bearer resource_metadata=\"{metadata}\"")
        }
    }

    /// Validate an access token and map it to a principal.
    ///
    /// Errors are deliberately coarse on the outside; the reason is only
    /// logged server-side.
    pub fn validate(&self, token: &str) -> Result<Principal> {
        let header = decode_header(token).context("malformed JWT")?;
        if header.alg != Algorithm::RS256 {
            bail!("unsupported JWT algorithm (expected RS256)");
        }
        let key = match header.kid.as_deref() {
            Some(kid) => self
                .keys
                .find(kid)
                .with_context(|| format!("unknown JWT key id '{kid}'"))?,
            None if self.keys.keys.len() == 1 => &self.keys.keys[0],
            None => bail!("JWT has no kid but the JWKS holds multiple keys"),
        };
        let decoding_key = DecodingKey::from_jwk(key).context("unusable JWK")?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(&[self.audience.as_str()]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        let data =
            decode::<Value>(token, &decoding_key, &validation).context("JWT validation failed")?;

        let subject = data
            .claims
            .get(&self.subject_claim)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .with_context(|| format!("JWT has no '{}' claim", self.subject_claim))?;
        Ok(Principal {
            id: subject.to_string(),
            display_name: None,
            auth_method: AuthMethod::OAuth,
        })
    }
}

/// RFC 9728 protected-resource metadata served by the MCP server.
#[derive(Debug, Clone, Serialize)]
pub struct ProtectedResourceMetadata {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    pub bearer_methods_supported: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
}

/// RFC 9728 §3.1 metadata URL for a resource identifier: the well-known
/// segment is inserted after the authority, keeping the resource path.
///
/// `https://mcp.example.com/mcp` ->
/// `https://mcp.example.com/.well-known/oauth-protected-resource/mcp`.
pub fn resource_metadata_url(resource: &str) -> String {
    let Some(scheme_end) = resource.find("://") else {
        return "/.well-known/oauth-protected-resource".to_string();
    };
    let authority_start = scheme_end + 3;
    let rest = &resource[authority_start..];
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return "/.well-known/oauth-protected-resource".to_string();
    }
    let path = path.trim_end_matches('/');
    format!(
        "{}://{authority}/.well-known/oauth-protected-resource{path}",
        &resource[..scheme_end]
    )
}

async fn fetch_metadata(
    client: &reqwest::Client,
    url: &str,
    expected_issuer: &str,
) -> Result<String> {
    let response = client.get(url).send().await.context("metadata request")?;
    if !response.status().is_success() {
        bail!("metadata endpoint returned HTTP {}", response.status());
    }
    let metadata: Value = response.json().await.context("parsing metadata")?;
    if let Some(issuer) = metadata.get("issuer").and_then(Value::as_str) {
        if issuer.trim_end_matches('/') != expected_issuer.trim_end_matches('/') {
            bail!("metadata issuer '{issuer}' does not match configured issuer");
        }
    }
    metadata
        .get("jwks_uri")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("metadata has no jwks_uri")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::jwk::Jwk;

    fn settings() -> OAuthSettings {
        OAuthSettings {
            issuer: "https://as.example.com".into(),
            audience: "https://mcp.example.com/mcp".into(),
            subject_claim: "sub".into(),
        }
    }

    #[test]
    fn metadata_url_inserts_well_known_after_authority() {
        assert_eq!(
            resource_metadata_url("https://mcp.example.com/mcp"),
            "https://mcp.example.com/.well-known/oauth-protected-resource/mcp"
        );
        assert_eq!(
            resource_metadata_url("https://mcp.example.com"),
            "https://mcp.example.com/.well-known/oauth-protected-resource"
        );
        assert_eq!(
            resource_metadata_url("http://127.0.0.1:8080/mcp/"),
            "http://127.0.0.1:8080/.well-known/oauth-protected-resource/mcp"
        );
    }

    #[test]
    fn protected_resource_metadata_shape() {
        let validator = OAuthValidator {
            issuer: settings().issuer,
            audience: settings().audience,
            subject_claim: settings().subject_claim,
            keys: Arc::new(JwkSet::default()),
        };
        let metadata = validator.protected_resource_metadata();
        assert_eq!(metadata.resource, "https://mcp.example.com/mcp");
        assert_eq!(
            metadata.authorization_servers,
            vec!["https://as.example.com".to_string()]
        );
        assert_eq!(
            metadata.bearer_methods_supported,
            vec!["header".to_string()]
        );
    }

    #[test]
    fn www_authenticate_is_spec_shaped() {
        let validator = OAuthValidator {
            issuer: settings().issuer,
            audience: settings().audience,
            subject_claim: settings().subject_claim,
            keys: Arc::new(JwkSet::default()),
        };
        let missing = validator.www_authenticate(false);
        assert!(missing.starts_with("Bearer "), "{missing}");
        assert!(
            missing.contains(
                "resource_metadata=\"https://mcp.example.com/.well-known/oauth-protected-resource/mcp\""
            ),
            "{missing}"
        );
        let invalid = validator.www_authenticate(true);
        assert!(invalid.contains("error=\"invalid_token\""), "{invalid}");
    }

    #[test]
    fn rejects_tokens_that_are_not_jwts() {
        let validator = OAuthValidator {
            issuer: settings().issuer,
            audience: settings().audience,
            subject_claim: settings().subject_claim,
            keys: Arc::new(JwkSet {
                keys: vec![Jwk::from_encoding_key(
                    &jsonwebtoken::EncodingKey::from_secret(b"not-a-real-key"),
                    Algorithm::HS256,
                )
                .unwrap()],
            }),
        };
        assert!(validator.validate("not-a-jwt").is_err());
    }
}

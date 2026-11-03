//! OAuth 2.1 resource-server tests (PRD-F1-07). Fully local: the test spins
//! an authorization-server metadata + JWKS endpoint on 127.0.0.1 and signs
//! its own JWTs with an embedded test key. No external network.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::routing::get;
use axum::{Json, Router};
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use strut_sql_core::config::OAuthSettings;
use strut_sql_server::{oauth::resource_metadata_url, OAuthValidator};
use tokio::net::TcpListener;

/// Throwaway 2048-bit RSA key used only by these tests.
const TEST_PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQD6ctOBNhOywlVz
3Xu+rxrbwqszVsHzhmQUS4+ZceqHsgd0x3SnSMkVYNsCca1PXx0+IRiLXYz9XTou
fbF5avwYBOtdgTqGQbJHY+Sq1q4Td21teUuqI+ZOCBlpIleZWvLVP9mPBlaw6T11
xLm899XNhg09PTX+9vlIRlxPU+aavR5FPIvky8eyrDCFu1UssZQbqgsWaaBjxjcu
J4xV4YCBq+RLNBePFl02ESpWmf+Ax+xwEwAMyK1QR3CTubR1xXMrfNWdt0xHV1xx
q2vZAA1DiiWPp3W+/qZsRg6z+xJN777ihYK/BOuScL562MzqUocCh6MrlCTA3YIA
a66TuyZlAgMBAAECggEAKWTxMFoPx2v3U783xLyMY+Y5BbNBO07rG6+hfGsnqGX0
dYjB7yRImA/6It1HF+dQngQ6Wk6MDxRpmWWM/9R+9jlZ7Yp+cdJJ9SK9mL8C5pTG
sWFq8CQe3KbaDFZwxBgUKsry2oDhszvZvK1XRbRw0hdl+Mk1M37+lMOMXlAvvUNR
GNvNibQripm/FcgaUTJWyXbh+4xwBAAqLhy0/JXeUOk/WokDjxIEAkZxZDITI0n5
ucFVf06+SWWk9gr1gjUzojT+mf+2qZ1zrsVDthPivrTpMNfJcxrNMtyKLHvLtOGy
yihRxaXktDB1E7SajVARjIqzwZtngrQ9q5DO2VcRwQKBgQD/cXriMXQ7hTKPJ0Mu
hwkCR/5YPtVomfBF72Pf9HqhhanrMHDQGWt2L8lF/zHtiKNeBE6nYz7GHKhAYeXi
gMu9PPCdMH7SCd01ZlB6ea1IM/T+1YoqwNkMAdInuYjsken+ET5ClTfxfSiJZyLv
8Kgkb/pjjwW80xJinDqi+pVIQwKBgQD6/o84IPY97pQ/0gKMJYxmD9mPniOjK2Qq
YoZXdS/Ig5SvRy94zgU0cQwPMqUnxkv0DVlZyBFVbQGLDMWOVkg5uikhAX2pSXr5
AC2B7nVZm7DyaGnP1xkeEDm9K3Z90yZ4UBIHfZD8feeQHC3GPduR64DDSoUdevCM
rZYj7/rgNwKBgQCd9j/mRmy5LdohGkd6KzkP9xM64Mg5ohAqPypE4LtAzRZRgSgS
F9YTROOzbGckm31i80jsS/wSqMAG6EMveTQEdv3YRpf2LPFlRzr3Me1NHAwh1k1V
uOG/ghTCSgJ5QJYxbdpRdFMe+6WoqmW5eNmRhsmQ7W2/4cqZvkCobksIXwKBgQC3
S3dj0g6Bo913zfI2TqV9U03aZ+42ltWLbnAVJmtCz3ShnCa63hlG9teLsbySk2mc
TLNXsZeW/wgFd8o97BzKq3ylseUG37aCaPLS1EW2rtBF9qJvOfIJ1meUEk1yDeUm
L7M67SQYgAm/YBS9lSboJUffVmhn4wQQeCQREovveQKBgGV+8qoxylQbrAG13+qb
1u2ph7Xx8LSfcyZ4kUptv/XEKywbFp+8bXzXOanTAop6X4oamHfb1Rd0yStZiJwS
Y0jSKqbwOVVpwhE7S52sfO7pHBGA/TB5pzRLotMtEpAqpkIHuTIKDkBp/TRoSllM
+9Ft/Ou45UMt0PfK485+XrZ8
-----END PRIVATE KEY-----"#;

const AUDIENCE: &str = "https://mcp.test/mcp";

struct LocalIssuer {
    issuer: String,
    encoding: EncodingKey,
    jwks: JwkSet,
    handle: tokio::task::JoinHandle<()>,
}

impl LocalIssuer {
    async fn start() -> Self {
        let encoding = EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY.as_bytes()).expect("test key");
        let mut jwk = Jwk::from_encoding_key(&encoding, Algorithm::RS256).expect("jwk");
        jwk.common.key_id = Some("test-key".to_string());
        let jwks = JwkSet { keys: vec![jwk] };

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let issuer = format!("http://{addr}");

        let metadata_jwks = Arc::new(jwks.clone());
        let metadata_issuer = issuer.clone();
        let app = Router::new()
            .route(
                "/.well-known/oauth-authorization-server",
                get({
                    let issuer = metadata_issuer.clone();
                    move || {
                        let issuer = issuer.clone();
                        async move {
                            Json(json!({
                                "issuer": issuer,
                                "jwks_uri": format!("{issuer}/jwks"),
                                "token_endpoint": format!("{issuer}/token"),
                            }))
                        }
                    }
                }),
            )
            .route(
                "/jwks",
                get(move || {
                    let jwks = metadata_jwks.clone();
                    async move { Json((*jwks).clone()) }
                }),
            );

        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            issuer,
            encoding,
            jwks,
            handle,
        }
    }

    fn settings(&self) -> OAuthSettings {
        OAuthSettings {
            issuer: self.issuer.clone(),
            audience: AUDIENCE.to_string(),
            subject_claim: "sub".to_string(),
        }
    }

    fn claims(&self, sub: &str) -> Value {
        json!({
            "sub": sub,
            "iss": self.issuer,
            "aud": AUDIENCE,
            "exp": now() + 300,
        })
    }

    fn sign(&self, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key".to_string());
        jsonwebtoken::encode(&header, claims, &self.encoding).expect("sign")
    }
}

impl Drop for LocalIssuer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

#[tokio::test]
async fn discovery_fetches_metadata_and_jwks() {
    let local = LocalIssuer::start().await;
    let validator = OAuthValidator::discover(&local.settings())
        .await
        .expect("discovery");

    let principal = validator
        .validate(&local.sign(&local.claims("alice")))
        .expect("valid token");
    assert_eq!(principal.id, "alice");
    assert_eq!(
        principal.auth_method,
        strut_sql_core::auth::AuthMethod::OAuth
    );
}

#[tokio::test]
async fn rejects_wrong_issuer_audience_and_expiry() {
    let local = LocalIssuer::start().await;
    let validator = OAuthValidator::discover(&local.settings())
        .await
        .expect("discovery");

    let mut wrong_issuer = local.claims("alice");
    wrong_issuer["iss"] = json!("https://evil.example.com");
    assert!(validator.validate(&local.sign(&wrong_issuer)).is_err());

    let mut wrong_audience = local.claims("alice");
    wrong_audience["aud"] = json!("https://other.test/mcp");
    assert!(validator.validate(&local.sign(&wrong_audience)).is_err());

    let mut expired = local.claims("alice");
    expired["exp"] = json!(now() - 3600);
    assert!(validator.validate(&local.sign(&expired)).is_err());

    let mut missing_exp = local.claims("alice");
    missing_exp.as_object_mut().unwrap().remove("exp");
    assert!(validator.validate(&local.sign(&missing_exp)).is_err());
}

#[tokio::test]
async fn rejects_tampering_unknown_kid_and_wrong_algorithm() {
    let local = LocalIssuer::start().await;
    let validator = OAuthValidator::discover(&local.settings())
        .await
        .expect("discovery");

    // Signature tampering.
    let token = local.sign(&local.claims("alice"));
    let mut tampered = token.clone();
    let last = tampered.pop().expect("token");
    tampered.push(if last == 'A' { 'B' } else { 'A' });
    assert!(validator.validate(&tampered).is_err());

    // Unknown key id.
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("other-key".to_string());
    let token = jsonwebtoken::encode(&header, &local.claims("alice"), &local.encoding).unwrap();
    assert!(validator.validate(&token).is_err());

    // HS256 is never accepted (alg confusion defence).
    let hs = EncodingKey::from_secret(b"shared-secret");
    let header = Header::new(Algorithm::HS256);
    let token = jsonwebtoken::encode(&header, &local.claims("alice"), &hs).unwrap();
    assert!(validator.validate(&token).is_err());

    // Non-JWT garbage.
    assert!(validator.validate("not-a-jwt").is_err());
}

#[tokio::test]
async fn subject_claim_is_configurable() {
    let local = LocalIssuer::start().await;
    let mut settings = local.settings();
    settings.subject_claim = "preferred_username".to_string();
    let validator = OAuthValidator::from_jwks(&settings, local.jwks.clone());

    let mut claims = local.claims("ignored");
    claims["preferred_username"] = json!("bob");
    let principal = validator.validate(&local.sign(&claims)).expect("valid");
    assert_eq!(principal.id, "bob");

    // Missing configured claim is rejected even though `sub` exists.
    assert!(validator
        .validate(&local.sign(&local.claims("alice")))
        .is_err());
}

#[test]
fn discovery_failure_is_fail_closed() {
    // Unreachable issuer: discover() must error, never return a validator
    // that accepts unverified tokens.
    let settings = OAuthSettings {
        issuer: "http://127.0.0.1:1".to_string(),
        audience: AUDIENCE.to_string(),
        subject_claim: "sub".to_string(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(runtime
        .block_on(OAuthValidator::discover(&settings))
        .is_err());
}

#[test]
fn resource_metadata_url_is_rfc9728_shaped() {
    assert_eq!(
        resource_metadata_url(AUDIENCE),
        "https://mcp.test/.well-known/oauth-protected-resource/mcp"
    );
}

// ---------------------------------------------------------------------------
// HTTP resource-server behaviour end to end (discovery, 401 challenge,
// principal mapping, unknown-principal denial).
// ---------------------------------------------------------------------------

mod http_oauth {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use jsonwebtoken::EncodingKey;
    use serde_json::{json, Value};
    use strut_sql_core::auth::{AuthMethod, Grant, GrantSource, Policy, Tier};
    use strut_sql_core::config::{AppConfig, HttpToken, OAuthSettings, ResolvedSource, WriteMode};
    use strut_sql_driver_api::{ConnectOptions, EngineDriver, EngineKind};
    use strut_sql_driver_sqlite::SqliteDriver;
    use strut_sql_server::{build_http_router_with_auth, OAuthValidator};
    use tower::ServiceExt;

    use super::TEST_PRIVATE_KEY;
    use super::{LocalIssuer, AUDIENCE};

    async fn sqlite_config(policy: Policy, oauth: Option<OAuthSettings>) -> AppConfig {
        std::env::set_var("MCP_SQL_FORMAT", "columnar");
        let driver = SqliteDriver::from_connection_string_with_options(
            "sqlite::memory:",
            &ConnectOptions {
                pool_size: 1,
                read_only: false,
                ..ConnectOptions::default()
            },
        )
        .await
        .expect("sqlite driver");
        let mut sources = HashMap::new();
        sources.insert(
            "default".into(),
            ResolvedSource::with_driver(
                "default",
                "sqlite::memory:".into(),
                EngineKind::Sqlite,
                Arc::new(*driver),
                false,
            ),
        );
        AppConfig {
            write_mode: WriteMode::AllowDdl,
            full_tools: true,
            max_rows: 100,
            max_bytes: 64 * 1024,
            query_timeout: Duration::from_secs(10),
            batch_concurrency: 8,
            batch_max_statements: 50,
            batch_timeout: Duration::from_secs(60),
            batch_max_bytes: 1024 * 1024,
            fail_fast: false,
            deny_columns: vec![],
            allow_explain_analyze: false,
            principal_id: "local".into(),
            query_history: false,
            default_source: "default".into(),
            sources,
            searched_paths: vec![],
            workspace: None,
            http_tokens: Vec::<HttpToken>::new(),
            http_rate_limit_per_minute: 600,
            allow_unauthenticated_http: false,
            allow_grants: false,
            oauth,
            policy,
            tables: strut_sql_core::auth::TablePolicy::default(),
            ephemeral: false,
            hide_endpoints: None,
        }
    }

    fn policy_for_alice() -> Policy {
        let mut policy = Policy::new(Tier::Ddl, true, false);
        policy.add_grant(Grant::new("alice", GrantSource::Any, Tier::Read));
        policy.with_local_default()
    }

    async fn mcp_call(
        app: axum::Router,
        token: Option<&str>,
        payload: Value,
    ) -> (StatusCode, String, axum::http::HeaderMap) {
        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "oauth-test", "version": "0" }
            }
        })
        .to_string();
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("host", "localhost");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(init)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "initialize");
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .expect("session id")
            .to_string();

        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("host", "localhost")
            .header("mcp-session-id", session_id.clone());
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let _ = app
            .clone()
            .oneshot(
                builder
                    .body(Body::from(
                        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("host", "localhost")
            .header("mcp-session-id", session_id);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let response = app
            .oneshot(builder.body(Body::from(payload.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string(), headers)
    }

    fn execute_sql_body() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {
                "name": "execute_sql",
                "arguments": { "sql": "SELECT 1" }
            }
        })
    }

    #[tokio::test]
    async fn discovery_metadata_and_bearer_flow() {
        let local = LocalIssuer::start().await;
        let validator = OAuthValidator::discover(&local.settings())
            .await
            .expect("discover");
        let config = sqlite_config(policy_for_alice(), Some(local.settings())).await;
        let (app, _ct) = build_http_router_with_auth(config, Some(Arc::new(validator)));

        // RFC 9728 discovery document.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let metadata: Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(metadata["resource"], AUDIENCE);
        assert_eq!(metadata["authorization_servers"][0], local.issuer);
        assert_eq!(metadata["bearer_methods_supported"][0], "header");

        // Valid JWT for a principal with a grant.
        let token = local.sign(&local.claims("alice"));
        let (status, body, _) = mcp_call(app.clone(), Some(&token), execute_sql_body()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("\"result\""), "{body}");

        // Valid JWT, no grant for this subject: deny all tools.
        let token = local.sign(&local.claims("mallory"));
        let (status, body, _) = mcp_call(app.clone(), Some(&token), execute_sql_body()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

        // Missing credential: 401 with a spec-shaped challenge.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .body(Body::from(execute_sql_body().to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let challenge = response
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .expect("WWW-Authenticate");
        assert!(challenge.starts_with("Bearer "), "{challenge}");
        assert!(challenge.contains("resource_metadata="), "{challenge}");
    }

    #[tokio::test]
    async fn invalid_jwt_is_401_with_invalid_token_error() {
        let local = LocalIssuer::start().await;
        let validator = OAuthValidator::discover(&local.settings())
            .await
            .expect("discover");
        let config = sqlite_config(policy_for_alice(), Some(local.settings())).await;
        let (app, _ct) = build_http_router_with_auth(config, Some(Arc::new(validator)));

        let mut claims = local.claims("alice");
        claims["aud"] = json!("https://wrong.test/mcp");
        let token = local.sign(&claims);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from(execute_sql_body().to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let challenge = response
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .expect("WWW-Authenticate");
        assert!(challenge.contains("error=\"invalid_token\""), "{challenge}");
    }

    #[tokio::test]
    async fn auth_method_is_oauth_for_jwt_principals() {
        // Guards the audit field: JWT callers are OAuth, not API key.
        let local = LocalIssuer::start().await;
        let validator = OAuthValidator::discover(&local.settings())
            .await
            .expect("discover");
        let principal = validator
            .validate(&local.sign(&local.claims("alice")))
            .expect("valid");
        assert_eq!(principal.auth_method, AuthMethod::OAuth);
    }

    #[tokio::test]
    async fn oauth_configured_without_validator_fails_closed() {
        // Mirrors a startup JWKS discovery failure: OAuth is configured but
        // no validator is available. Requests must be 401, never anonymous.
        let settings = OAuthSettings {
            issuer: "https://as.unreachable.test".into(),
            audience: AUDIENCE.into(),
            subject_claim: "sub".into(),
        };
        let config = sqlite_config(policy_for_alice(), Some(settings)).await;
        let (app, _ct) = build_http_router_with_auth(config, None);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .body(Body::from(execute_sql_body().to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn unused_encoding_key_helper_stays_in_sync() {
        // The fixture key must remain loadable (guards accidental corruption).
        EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY.as_bytes()).expect("test key");
    }
}

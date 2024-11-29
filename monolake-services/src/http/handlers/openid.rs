use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use bytes::{Bytes, BytesMut};
use cookie::Cookie;
use http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use lazy_static::lazy_static;
use monoio::net::TcpStream;
use monoio_http::common::body::{Body, FixedBody, HttpBody, StreamHint};
use monoio_transports::{
    connectors::{Connector, TcpConnector, TcpTlsAddr, TlsConnector, TlsStream},
    http::H1Connector,
};
use monolake_core::http::HttpHandler;
#[allow(unused)]
use openidconnect::core::{
    CoreAuthDisplay, CoreClaimName, CoreClaimType, CoreClient, CoreClientAuthMethod,
    CoreGenderClaim, CoreGrantType, CoreIdTokenClaims, CoreIdTokenVerifier, CoreJsonWebKey,
    CoreJsonWebKeyType, CoreJsonWebKeyUse, CoreJweContentEncryptionAlgorithm,
    CoreJweKeyManagementAlgorithm, CoreJwsSigningAlgorithm, CoreProviderMetadata, CoreResponseMode,
    CoreResponseType, CoreRevocableToken, CoreSubjectIdentifierType,
};
#[allow(unused)]
use openidconnect::{
    AccessToken, AdditionalClaims, AdditionalProviderMetadata, AuthenticationFlow,
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, OAuth2TokenResponse,
    ProviderMetadata, RedirectUrl, RevocationUrl, Scope, UserInfoClaims,
};
use openidconnect::{HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use service_async::{
    layer::{layer_fn, FactoryLayer},
    AsyncMakeService, MakeService, Param, Service,
};
use thiserror::Error;
use tracing::debug;
use url::Url;

use crate::http::util::generate_empty_response;

type HttpsConnector = H1Connector<TlsConnector<TcpConnector>, TcpTlsAddr, TlsStream<TcpStream>>;

#[derive(Debug, Error)]
pub enum Error {}

#[derive(Debug, Deserialize, Serialize)]
struct ExtraClaims {
    // Deprecated and thus optional as it might be removed in the futre
    // sub_legacy: Option<String>,
    // groups: Vec<String>,
}
impl AdditionalClaims for ExtraClaims {}

fn handle_error<T: std::error::Error>(fail: &T, msg: &'static str) {
    let mut err_msg = format!("ERROR: {}", msg);
    let mut cur_fail: Option<&dyn std::error::Error> = Some(fail);
    while let Some(cause) = cur_fail {
        err_msg += &format!("\n    caused by: {}", cause);
        cur_fail = cause.source();
    }
    debug!("{}", err_msg);
    // exit(1);
}

pub async fn async_http_client(request: HttpRequest) -> Result<HttpResponse, Error> {
    let uri = request.url.as_str().parse::<http::uri::Uri>().unwrap();
    let method = request.method.as_str().parse::<http::Method>().unwrap();
    let mut req = Request::builder()
        .version(http::Version::HTTP_11)
        .method(method)
        .uri(&uri);

    let headers = req.headers_mut().unwrap();
    for (key, value) in request.headers.iter() {
        headers.insert(
            HeaderName::from_bytes(key.as_str().as_bytes()).unwrap(),
            HeaderValue::from_str(value.to_str().unwrap()).unwrap(),
        );
    }
    let host = uri.host().unwrap();
    headers.insert("Host", HeaderValue::from_bytes(host.as_bytes()).unwrap());

    let request_payload: Bytes = request.body.into();
    let req: http::Request<HttpBody> = req
        .body(HttpBody::fixed_body(Some(request_payload)))
        .unwrap();

    let client = HttpsConnector::default().with_default_pool();
    let key = req.uri().try_into().unwrap();
    let mut client = client.connect(key).await.unwrap();
    let (response, _) = client.send_request(req).await;
    let response = response.unwrap();

    let status = response.status().as_u16();
    // let headers: HeaderMap = response.headers().clone();
    let mut headers = openidconnect::http::HeaderMap::new();
    for (key, value) in response.headers().iter() {
        headers.insert(
            openidconnect::http::HeaderName::from_bytes(key.as_str().as_bytes()).unwrap(),
            openidconnect::http::HeaderValue::from_str(value.to_str().unwrap()).unwrap(),
        );
    }
    let mut body = response.into_body();

    let payload = match body.stream_hint() {
        StreamHint::None => Bytes::new(),
        StreamHint::Fixed => body.next_data().await.unwrap().unwrap(),
        StreamHint::Stream => {
            let mut bytes = BytesMut::new();
            while let Some(Ok(data)) = body.next_data().await {
                bytes.extend_from_slice(&data[..]);
            }
            bytes.freeze()
        }
    };

    let body: Vec<u8> = payload.to_vec();
    Ok(HttpResponse {
        status_code: status.try_into().unwrap(),
        headers,
        body,
    })
}

#[derive(Clone)]
pub struct OpenIdHandler<H> {
    inner: H,
    openid_config: Option<OpenIdConfig>,
}

impl<F: MakeService> MakeService for OpenIdHandler<F> {
    type Service = OpenIdHandler<F::Service>;
    type Error = F::Error;

    fn make_via_ref(&self, old: Option<&Self::Service>) -> Result<Self::Service, Self::Error> {
        Ok(OpenIdHandler {
            inner: self.inner.make_via_ref(old.map(|o| &o.inner))?,
            openid_config: self.openid_config.clone(),
        })
    }
}

impl<F: AsyncMakeService> AsyncMakeService for OpenIdHandler<F> {
    type Service = OpenIdHandler<F::Service>;
    type Error = F::Error;

    async fn make_via_ref(
        &self,
        old: Option<&Self::Service>,
    ) -> Result<Self::Service, Self::Error> {
        Ok(OpenIdHandler {
            inner: self.inner.make_via_ref(old.map(|o| &o.inner)).await?,
            openid_config: self.openid_config.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenIdConfig {
    // TODO: Need to add openid scopes etc.
    pub client_id: String,
    pub client_secret: String,
    pub issuer_url: String,
    pub redirect_url: String,
}

impl<F> OpenIdHandler<F> {
    pub fn layer<C>() -> impl FactoryLayer<C, F, Factory = Self>
    where
        C: Param<Option<OpenIdConfig>>,
    {
        layer_fn(move |c: &C, inner| Self {
            inner,
            openid_config: c.param(),
        })
    }
}

#[derive(Clone)]
struct SessionState {
    // Plenty more to add, eg. expiration time
    pub nonce: Nonce,
    pub access_token: Option<AccessToken>,
}

// TODO: This is only a PoC, eventually need to replace this with a backend store like Redis for
// example.
lazy_static! {
    static ref SESSION_STORE: Arc<RwLock<HashMap<String, SessionState>>> =
        Arc::new(RwLock::new(HashMap::new()));
}

// impl<H> HttpHandler for OpenIdHandler<H>
impl<H, CX, B> Service<(Request<B>, CX)> for OpenIdHandler<H>
where
    H: HttpHandler<CX, B>,
    H::Body: FixedBody,
{
    type Response = Response<H::Body>;
    type Error = H::Error;

    async fn call(&self, (request, ctx): (Request<B>, CX)) -> Result<Self::Response, Self::Error> {
        if self.openid_config.is_none() {
            return self.inner.handle(request, ctx).await;
        }

        let headers = request.headers();
        let mut auth_cookie: Option<String> = None;
        if headers.contains_key(http::header::COOKIE) {
            let cookies = Cookie::split_parse(
                (headers.get(http::header::COOKIE).unwrap())
                    .to_str()
                    .unwrap(),
            );
            for cookie in cookies {
                let cookie = cookie.unwrap();
                if cookie.name() == "session-id" {
                    let session_store = SESSION_STORE.read().unwrap();
                    if let Some(state) = session_store.get(cookie.value())
                        && state.access_token.is_some()
                    {
                        auth_cookie = Some(cookie.value().to_string());
                    }
                    break;
                }
            }
        }

        let mut authed = false;
        if let Some(auth) = auth_cookie {
            // authorized
            let session_store = SESSION_STORE.read().unwrap();
            if let Some(access) = session_store.get(&auth) {
                authed = access.access_token.is_some()
            }
        }
        if authed {
            return self.inner.handle(request, ctx).await;
        }

        let openid_config = self.openid_config.clone().unwrap();
        let client_id = ClientId::new(openid_config.client_id);
        let client_secret = ClientSecret::new(openid_config.client_secret);
        let issuer_url = IssuerUrl::new(openid_config.issuer_url).expect("Invalid issuer URL");

        let provider_metadata = CoreProviderMetadata::discover_async(issuer_url, async_http_client)
            .await
            .unwrap_or_else(|err| {
                handle_error(&err, "Failed to discover OpenID Provider");
                unreachable!();
            });

        // Set up the config for the OAuth2 process.
        let client =
            CoreClient::from_provider_metadata(provider_metadata, client_id, Some(client_secret))
                .set_redirect_uri(
                    RedirectUrl::new(openid_config.redirect_url).expect("Invalid redirect URL"),
                );

        // Generate the authorization URL to which we'll redirect the user.
        let (authorize_url, csrf_state, mut nonce) = client
            .authorize_url(
                AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            // Should add scopes to the server config as well in order to set them up here.
            //.add_scope(Scope::new("email".to_string()))
            //.add_scope(Scope::new("profile".to_string()))
            .url();

        debug!("CSRF: {}", csrf_state.secret());

        let uri = request.uri().to_string();
        let url = Url::parse(&("http://localhost".to_string() + &uri)).unwrap();

        let code_pair = url.query_pairs().find(|pair| pair.0 == "code");
        let state_pair = url.query_pairs().find(|pair| pair.0 == "state");

        let code;
        let state;
        {
            if code_pair.is_none() || state_pair.is_none() {
                let mut redirect_response = Response::builder()
                    .status(StatusCode::from_u16(301).unwrap())
                    .body(H::Body::fixed_body(None))
                    .unwrap();
                redirect_response
                    .headers_mut()
                    .insert(http::header::LOCATION, unsafe {
                        HeaderValue::from_maybe_shared_unchecked(format!("{}", authorize_url))
                    });
                SESSION_STORE.write().unwrap().insert(
                    csrf_state.secret().clone(),
                    SessionState {
                        nonce,
                        access_token: None,
                    },
                );
                return Ok(redirect_response);
            }
            let session_store = SESSION_STORE.read().unwrap();
            let (_, code_val) = code_pair.clone().unwrap();
            code = AuthorizationCode::new(code_val.into_owned());
            let (_, state_val) = state_pair.clone().unwrap();
            state = CsrfToken::new(state_val.clone().into_owned());
            if !session_store.contains_key(&state_val.to_string()) {
                let mut redirect_response: Response<H::Body> = Response::builder()
                    .status(StatusCode::from_u16(301).unwrap())
                    .body(H::Body::fixed_body(None))
                    .unwrap();
                redirect_response
                    .headers_mut()
                    .insert(http::header::LOCATION, unsafe {
                        HeaderValue::from_maybe_shared_unchecked(format!("{}", authorize_url))
                    });
                let mut session_store = SESSION_STORE.write().unwrap();
                session_store.insert(
                    state_val.to_string(),
                    SessionState {
                        nonce,
                        access_token: None,
                    },
                );
                return Ok(redirect_response);
            }
            nonce = session_store
                .get(&state_val.to_string())
                .unwrap()
                .nonce
                .clone();
        }

        debug!(
            "OpenID provider returned the following code:\n{}\n",
            code.secret()
        );
        debug!(
            "OpenID provider returned the following state: {}",
            state.secret()
        );

        // Exchange the code with a token.
        let token_response = client
            .exchange_code(code)
            .request_async(async_http_client)
            .await
            .unwrap_or_else(|err| {
                handle_error(&err, "Failed to contact token endpoint");
                unreachable!();
            });
        debug!(
            "OpenID provider returned access token:\n{}\n",
            token_response.access_token().secret()
        );
        debug!(
            "OpenID provider returned scopes: {:?}",
            token_response.scopes()
        );

        // Need to save this as well
        let id_token_verifier: CoreIdTokenVerifier = client.id_token_verifier();
        let id_token_claims: &CoreIdTokenClaims = token_response
            .extra_fields()
            .id_token()
            .expect("Server did not return an ID token")
            .claims(&id_token_verifier, &nonce)
            .unwrap_or_else(|err| {
                handle_error(&err, "Failed to verify ID token");
                unreachable!();
            });
        debug!("OpenID provider returned ID token: {:?}\n", id_token_claims);

        {
            let mut session_store = SESSION_STORE.write().unwrap();
            session_store.get_mut(state.secret()).unwrap().access_token =
                Some(token_response.access_token().clone());
        }

        match self.inner.handle(request, ctx).await {
            Ok(mut response) => {
                let headers = response.headers_mut();
                // Use the state number (csrf) as the session-id for future auth. Need to add
                // more cookies like expiration time.
                headers.insert(
                    http::header::SET_COOKIE,
                    HeaderValue::from_str(&format!("session-id={}", state.secret())).unwrap(),
                );
                Ok(response)
            }
            Err(_e) => Ok(generate_empty_response(StatusCode::INTERNAL_SERVER_ERROR)),
        }
    }
}

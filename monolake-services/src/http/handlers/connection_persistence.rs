//! HTTP connection persistence and keep-alive management module.
//!
//! This module provides functionality to manage HTTP connection persistence (keep-alive)
//! across different HTTP versions. It handles the intricacies of connection reuse for
//! HTTP/1.0, HTTP/1.1, and HTTP/2, ensuring proper header management and version compatibility.
//! # Key Components
//!
//! - [`ConnectionReuseHandler`]: The main service component responsible for managing connection
//!   persistence and keep-alive behavior.
//!
//! # Features
//!
//! - Automatic detection and handling of keep-alive support for incoming requests
//! - Version-specific handling for HTTP/1.0, HTTP/1.1, and HTTP/2
//! - Modification of request and response headers to ensure proper keep-alive behavior
//! - Seamless integration with `service_async` for easy composition in service stacks
//! - Support for upgrading HTTP/1.0 connections to HTTP/1.1-like behavior
//!
//! # Usage
//!
//! This handler is typically used as part of a larger HTTP service stack. Here's a basic example:
//!
//! ```rust
//! use monolake_services::{
//!     common::ContextService,
//!     http::{
//!         core::HttpCoreService,
//!         detect::H2Detect,
//!         handlers::{
//!             route::RouteConfig, ConnectionReuseHandler, ContentHandler, RewriteAndRouteHandler,
//!             UpstreamHandler,
//!         },
//!         HttpServerTimeout,
//!     },
//! };
//! use service_async::{layer::FactoryLayer, stack::FactoryStack, Param};
//!
//! // Dummy struct to satisfy Param trait requirements
//! struct DummyConfig;
//!
//! // Implement Param for DummyConfig to return Vec<RouteConfig>
//! impl Param<Vec<RouteConfig>> for DummyConfig {
//!     fn param(&self) -> Vec<RouteConfig> {
//!         vec![]
//!     }
//! }
//! impl Param<HttpServerTimeout> for DummyConfig {
//!     fn param(&self) -> HttpServerTimeout {
//!         HttpServerTimeout::default()
//!     }
//! }
//!
//! let config = DummyConfig;
//! let stacks = FactoryStack::new(config)
//!     .replace(UpstreamHandler::factory(
//!         Default::default(),
//!         Default::default(),
//!     ))
//!     .push(ContentHandler::layer())
//!     .push(RewriteAndRouteHandler::layer())
//!     .push(ConnectionReuseHandler::layer())
//!     .push(HttpCoreService::layer())
//!     .push(H2Detect::layer());
//!
//! // Use the service to handle HTTP requests
//! ```
//!
//! # Performance Considerations
//!
//! - Efficient header manipulation to minimize overhead
//! - Optimized handling for HTTP/2, which has built-in connection persistence
use http::{Request, Version};
use monolake_core::http::{HttpError, HttpHandler, ResponseWithContinue};
use service_async::{
    layer::{layer_fn, FactoryLayer},
    AsyncMakeService, MakeService, Service,
};

use crate::http::{CLOSE, CLOSE_VALUE, KEEPALIVE, KEEPALIVE_VALUE};

/// Handler for managing HTTP connection persistence and keep-alive behavior.
///
/// `ConnectionReuseHandler` is responsible for:
/// 1. Detecting whether an incoming request supports keep-alive.
/// 2. Modifying request and response headers to ensure proper keep-alive behavior.
/// 3. Handling version-specific connection persistence logic for HTTP/1.0, HTTP/1.1, and HTTP/2.
///
/// For implementation details and example usage, see the
/// [module level documentation](crate::http::handlers::connection_persistence).
#[derive(Clone)]
pub struct ConnectionReuseHandler<H> {
    inner: H,
}

impl<H, CX, B> Service<(Request<B>, CX)> for ConnectionReuseHandler<H>
where
    H: HttpHandler<CX, B>,
    H::Error: HttpError<H::Body>,
{
    type Response = ResponseWithContinue<H::Body>;
    type Error = H::Error;

    async fn call(
        &self,
        (mut request, ctx): (Request<B>, CX),
    ) -> Result<Self::Response, Self::Error> {
        match request.version() {
            // for http 1.0, hack it to 1.1 like setting nginx `proxy_http_version` to 1.1
            Version::HTTP_10 => {
                // get and remove connection header
                let mut keepalive = false;
                let conn_header_val = request.headers_mut().remove(http::header::CONNECTION);
                if matches!(conn_header_val, Some(v) if v.as_bytes().eq_ignore_ascii_case(KEEPALIVE.as_bytes()))
                {
                    keepalive = true;
                }

                // modify to 1.1
                *request.version_mut() = Version::HTTP_11;

                // send http 1.1 request
                let maybe_response = self.inner.handle(request, ctx).await;
                let mut response = match maybe_response {
                    Ok(r) => r,
                    Err(e) => {
                        return match e.to_response() {
                            // if recoverable, return generated http 1.0 response with keepalive
                            // TODO(ihciah): Note: the caller should insert keepalive header if
                            // needed
                            Some(mut resp) => {
                                if keepalive {
                                    resp.headers_mut()
                                        .insert(http::header::CONNECTION, KEEPALIVE_VALUE);
                                }
                                *resp.version_mut() = Version::HTTP_10;
                                Ok((resp, keepalive))
                            }
                            None => Err(e),
                        };
                    }
                };

                // modify to 1.0
                *response.version_mut() = Version::HTTP_10;

                // insert keepalive header if needed
                if keepalive {
                    response
                        .headers_mut()
                        .insert(http::header::CONNECTION, KEEPALIVE_VALUE);
                } else {
                    response
                        .headers_mut()
                        .insert(http::header::CONNECTION, CLOSE_VALUE);
                }

                Ok((response, keepalive))
            }
            Version::HTTP_11 => {
                // get and remove connection header
                let mut keepalive = true;
                let conn_header_val = request.headers_mut().remove(http::header::CONNECTION);
                if matches!(conn_header_val, Some(v) if v.as_bytes().eq_ignore_ascii_case(CLOSE.as_bytes()))
                {
                    keepalive = false;
                }

                // send http 1.1 request
                let mut response = match self.inner.handle(request, ctx).await {
                    Ok(r) => r,
                    Err(e) => {
                        return match e.to_response() {
                            // if recoverable, return generated http 1.1 response with keepalive
                            Some(mut resp) => {
                                if !keepalive {
                                    resp.headers_mut()
                                        .insert(http::header::CONNECTION, CLOSE_VALUE);
                                }
                                Ok((resp, keepalive))
                            }
                            None => Err(e),
                        };
                    }
                };

                // insert keepalive header if needed
                if keepalive {
                    response
                        .headers_mut()
                        .insert(http::header::CONNECTION, KEEPALIVE_VALUE);
                } else {
                    response
                        .headers_mut()
                        .insert(http::header::CONNECTION, CLOSE_VALUE);
                }

                Ok((response, keepalive))
            }
            Version::HTTP_2 => {
                let response = match self.inner.handle(request, ctx).await {
                    Ok(r) => r,
                    Err(e) => {
                        return match e.to_response() {
                            // if recoverable, return generated http 1.1 response with keepalive
                            Some(mut resp) => {
                                *resp.version_mut() = Version::HTTP_2;
                                // keepalive is always true for http2
                                Ok((resp, true))
                            }
                            None => Err(e),
                        };
                    }
                };
                Ok((response, true))
            }
            // for http 0.9, just relay it
            Version::HTTP_09 => {
                let response = match self.inner.handle(request, ctx).await {
                    Ok(r) => r,
                    Err(e) => {
                        return match e.to_response() {
                            // if recoverable, return generated http 1.1 response with keepalive
                            Some(mut resp) => {
                                *resp.version_mut() = Version::HTTP_09;
                                Ok((resp, false))
                            }
                            None => Err(e),
                        };
                    }
                };
                Ok((response, false))
            }
            _ => {
                // we haven't support h3 yet.
                let response = self.inner.handle(request, ctx).await?;
                Ok((response, true))
            }
        }
    }
}

// ConnReuseHandler is a Service and a MakeService.
impl<F: MakeService> MakeService for ConnectionReuseHandler<F> {
    type Service = ConnectionReuseHandler<F::Service>;
    type Error = F::Error;

    fn make_via_ref(&self, old: Option<&Self::Service>) -> Result<Self::Service, Self::Error> {
        Ok(ConnectionReuseHandler {
            inner: self.inner.make_via_ref(old.map(|o| &o.inner))?,
        })
    }
}

impl<F: AsyncMakeService> AsyncMakeService for ConnectionReuseHandler<F> {
    type Service = ConnectionReuseHandler<F::Service>;
    type Error = F::Error;

    async fn make_via_ref(
        &self,
        old: Option<&Self::Service>,
    ) -> Result<Self::Service, Self::Error> {
        Ok(ConnectionReuseHandler {
            inner: self.inner.make_via_ref(old.map(|o| &o.inner)).await?,
        })
    }
}

impl<F> ConnectionReuseHandler<F> {
    pub fn layer<C>() -> impl FactoryLayer<C, F, Factory = Self> {
        layer_fn(|_: &C, inner| Self { inner })
    }
}

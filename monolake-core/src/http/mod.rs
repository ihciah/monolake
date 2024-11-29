//! HTTP handling traits and types for asynchronous services.
//!
//! This module provides traits and types for implementing HTTP handlers
//! that can be used with asynchronous services. It defines a common interface
//! for processing HTTP requests and generating responses, with support for
//! connection management and context-aware handling.
//!
//! # Key Components
//!
//! - [`HttpHandler`]: A trait for implementing HTTP request handlers.
//! - [`ResponseWithContinue`]: A type alias for responses that indicate whether to continue
//!   processing the connection.
//! - [`HttpAccept`]: A type alias for connection acceptance information.
//!
//! # Usage
//!
//! The `HttpHandler` trait is automatically implemented for types
//! that implement the [`Service`] trait with
//! request type `(Request<B>, CX)` and return type
//! [`ResponseWithContinue`].
use std::{convert::Infallible, future::Future};

use http::{Request, Response};
use service_async::Service;

use crate::sealed::SealedT;

/// A tuple representing an HTTP response along with a connection continuation flag.
///
/// # Type Parameters
///
/// - `B`: The body type of the response.
///
/// # Fields
///
/// - `Response<B>`: The HTTP response.
/// - `bool`: A flag indicating whether to continue processing the connection.
///   - `true`: Continue processing the connection.
///   - `false`: Close the connection after sending the response.
///
/// Note: The service does not need to add the `Connection: close` header itself;
/// this is handled by the HTTP core service based on this flag.
// TODO: replace it with HttpError<B>.
pub type ResponseWithContinue<B> = (Response<B>, bool);

pub trait HttpError<B> {
    /// If an error can be turned to an HTTP response, it means the error is
    /// a recoverable error and the connection can be kept alive.
    fn to_response(&self) -> Option<Response<B>>;

    /// A helper function to check if an error is recoverable.
    /// Manually implement this function has better performance.
    /// Makes sure if returns true, `to_response` must return Some.
    /// And if returns false, `to_response` must return None.
    fn is_recoverable(&self) -> bool {
        self.to_response().is_some()
    }
}

impl<B> HttpError<B> for Infallible {
    fn to_response(&self) -> Option<Response<B>> {
        match *self {}
    }
    fn is_recoverable(&self) -> bool {
        match *self {}
    }
}

#[derive(Debug, Clone, Default, Copy, PartialEq, Eq)]
pub struct HttpFatalError<E>(pub E);
impl<B, E> HttpError<B> for HttpFatalError<E> {
    #[inline]
    fn to_response(&self) -> Option<Response<B>> {
        None
    }
    fn is_recoverable(&self) -> bool {
        false
    }
}

/// A tuple representing an accepted HTTP connection with its context.
///
/// # Type Parameters
///
/// - `Stream`: The type of the I/O stream for the connection.
/// - `CX`: The type of the connection context, typically a `certain_map`.
///
/// # Fields
///
/// - `bool`: Indicates whether the connection is using HTTP/2.
///   - `true`: The connection is using HTTP/2.
///   - `false`: The connection is using HTTP/1.x.
/// - `Stream`: The I/O stream for the connection.
/// - `CX`: The context of the connection, providing additional information or state.
pub type HttpAccept<Stream, CX> = (bool, Stream, CX);

struct HttpSeal;

/// A trait for HTTP request handlers.
///
/// This trait defines the interface for processing HTTP requests and generating responses.
/// It is designed to work with asynchronous services and supports context-aware handling.
///
/// Implementors of this trait can process HTTP requests and return responses along with
/// a boolean flag indicating whether to continue processing the connection.
///
/// # Type Parameters
///
/// - `CX`: The context type for additional request processing information.
/// - `B`: The body type of the incoming request.
///
/// # Associated Types
///
/// - `Body`: The body type of the outgoing response.
/// - `Error`: The error type that may occur during request handling.
///
/// # Examples
///
/// ```ignore
/// use your_crate::{HttpHandler, ResponseWithContinue};
/// use http::{Request, Response};
///
/// struct MyHandler;
///
/// impl HttpHandler<(), Vec<u8>> for MyHandler {
///     type Body = Vec<u8>;
///     type Error = std::io::Error;
///
///     async fn handle(&self, request: Request<Vec<u8>>, ctx: ())
///         -> Result<ResponseWithContinue<Self::Body>, Self::Error> {
///         // Process the request and generate a response
///         let response = Response::new(Vec::new());
///         Ok((response, true))
///     }
/// }
/// ```
///
/// The [`HttpHandler`] trait is automatically implemented for types
/// that implement the [`Service`] trait with
/// request type `(Request<B>, CX)` and return type
/// [`ResponseWithContinue`].
#[allow(private_bounds)]
pub trait HttpHandler<CX, B>: SealedT<HttpSeal, (CX, B)> {
    type Body;
    type Error;

    fn handle(
        &self,
        request: Request<B>,
        ctx: CX,
    ) -> impl Future<Output = Result<Response<Self::Body>, Self::Error>>;
}

impl<T, CX, IB, OB> SealedT<HttpSeal, (CX, IB)> for T where
    T: Service<(Request<IB>, CX), Response = OB>
{
}

impl<T, CX, IB, OB> HttpHandler<CX, IB> for T
where
    T: Service<(Request<IB>, CX), Response = Response<OB>>,
{
    type Body = OB;
    type Error = T::Error;

    async fn handle(&self, req: Request<IB>, ctx: CX) -> Result<Response<OB>, Self::Error> {
        self.call((req, ctx)).await
    }
}

use crate::retryable::Retryable;
use http::StatusCode;
use reqwest_middleware::Error;

/// A strategy to create a [`Retryable`] from a [`Result<reqwest::Response, reqwest_middleware::Error>`]
///
/// A [`RetryableStrategy`] has two functions, a `success_handler` and a `error_handler`
/// The correct handler will be called based on what the result of calling the request was.
/// The result of calling the request could be:\
/// - [`reqwest::Response`] In case the request has been sent and received correctly
///     This could however still mean that the server responded with a erroneous response.
///     For example a HTTP statuscode of 500
/// - [`reqwest_middleware::Error`] In this case the request actually failed.
///     This could, for example, be caused by a timeout on the connection.
/// 
/// Example:
/// 
/// ```
/// use reqwest_retry::{RetryableStrategy, policies::ExponentialBackoff, RetryTransientMiddleware, Retryable};
/// use reqwest::{Request, Response};
/// use reqwest_middleware::{ClientBuilder, Middleware, Next, Result};
/// use task_local_extensions::Extensions;
/// 
/// // Log each request to show that the requests will be retried
/// struct LoggingMiddleware;
/// 
/// #[async_trait::async_trait]
/// impl Middleware for LoggingMiddleware {
///     async fn handle(
///         &self,
///         req: Request,
///         extensions: &mut Extensions,
///         next: Next<'_>,
///     ) -> Result<Response> {
///         println!("Request started {}", req.url());
///         let res = next.run(req, extensions).await;
///         println!("Request finished");
///         res
///     }
/// }
/// 
/// // Just a toy example, retry when the response code is 201, else do nothing.
/// fn retry_201(res: &reqwest::Response) -> Option<Retryable> {
///     if res.status() == 201 {
///         Some(Retryable::Transient)
///     } else {
///         None
///     }
/// }
/// 
/// 
/// #[tokio::main]
/// async fn main() {
///     // Create the retry stategy. Success responses will be handled by `retry_201`.
///     // Error handling is default.
///     let ret_strat = RetryableStrategy::new(
///         Some(retry_201),
///         None
///     );
/// 
///     // Exponential backoff with max 2 retries
///     let retry_policy = ExponentialBackoff::builder()
///         .build_with_max_retries(2);
///     
///     // Create the actual middleware, with the exponential backoff and custom retry stategy.
///     let ret_s = RetryTransientMiddleware::new_with_retryable_strat(
///         retry_policy,
///         ret_strat
///     );
/// 
///     let client = ClientBuilder::new(reqwest::Client::new())
///         // Retry failed requests.
///         .with(ret_s)
///         // Log the requests
///         .with(LoggingMiddleware)
///         .build();
/// 
///     // Send request which should get a 201 response. So it will be retried
///     let r = client   
///         .get("https://httpbin.org/status/201")
///         .send()
///         .await;
///     println!("{:?}", r);
/// 
///     // Send request which should get a 200 response. So it will not be retried
///     let r = client   
///         .get("https://httpbin.org/status/200")
///         .send()
///         .await;
///     println!("{:?}", r);
/// }
/// ```
pub struct RetryableStrategy {
    pub success_handler: fn(&reqwest::Response) -> Option<Retryable>,
    pub error_handler: fn(&Error) -> Option<Retryable>,
}

impl Default for RetryableStrategy {
    /// The default strategy to use to convert a [`reqwest::Response`] to a retry decision
    fn default() -> Self {
        Self {
            success_handler: Self::default_success_handler,
            error_handler: Self::default_error_handler,
        }
    }
}

impl RetryableStrategy {
    /// Actually classifies the request as a [`Retryable`],
    /// based on the success- and error-handlers for this [`RetryableStrategy`]
    pub fn handle(&self, res: &Result<reqwest::Response, Error>) -> Option<Retryable> {
        match res {
            Ok(success) => (self.success_handler)(success),
            Err(error) => (self.error_handler)(error),
        }
    }

    pub fn new(
        success_handler: Option<fn(&reqwest::Response) -> Option<Retryable>>,
        error_handler: Option<fn(&Error) -> Option<Retryable>>,
    ) -> Self {
        // Choose default success_handler if the passed argument is `None`
        let success_handler = if let Some(x) = success_handler {
            x
        } else {
            Self::default_success_handler
        };

        // Choose default error_handler if the passed argument is `None`
        let error_handler = if let Some(x) = error_handler {
            x
        } else {
            Self::default_error_handler
        };

        Self {
            success_handler,
            error_handler,
        }
    }

    /// A good default handler for a success response
    pub fn default_success_handler(success: &reqwest::Response) -> Option<Retryable> {
        let status = success.status();
        if status.is_server_error() {
            Some(Retryable::Transient)
        } else if status.is_client_error()
            && status != StatusCode::REQUEST_TIMEOUT
            && status != StatusCode::TOO_MANY_REQUESTS
        {
            Some(Retryable::Fatal)
        } else if status.is_success() {
            None
        } else if status == StatusCode::REQUEST_TIMEOUT || status == StatusCode::TOO_MANY_REQUESTS {
            Some(Retryable::Transient)
        } else {
            Some(Retryable::Fatal)
        }
    }

    /// A good default handler for a failed response
    pub fn default_error_handler(error: &Error) -> Option<Retryable> {
        match error {
            // If something fails in the middleware we're screwed.
            Error::Middleware(_) => Some(Retryable::Fatal),
            Error::Reqwest(error) => {
                #[cfg(not(target_arch = "wasm32"))]
                let is_connect = error.is_connect();
                #[cfg(target_arch = "wasm32")]
                let is_connect = false;
                if error.is_timeout() || is_connect {
                    Some(Retryable::Transient)
                } else if error.is_body()
                    || error.is_decode()
                    || error.is_builder()
                    || error.is_redirect()
                {
                    Some(Retryable::Fatal)
                } else if error.is_request() {
                    // It seems that hyper::Error(IncompleteMessage) is not correctly handled by reqwest.
                    // Here we check if the Reqwest error was originated by hyper and map it consistently.
                    #[cfg(not(target_arch = "wasm32"))]
                    if let Some(hyper_error) = get_source_error_type::<hyper::Error>(&error) {
                        // The hyper::Error(IncompleteMessage) is raised if the HTTP response is well formatted but does not contain all the bytes.
                        // This can happen when the server has started sending back the response but the connection is cut halfway thorugh.
                        // We can safely retry the call, hence marking this error as [`Retryable::Transient`].
                        // Instead hyper::Error(Canceled) is raised when the connection is
                        // gracefully closed on the server side.
                        if hyper_error.is_incomplete_message() || hyper_error.is_canceled() {
                            Some(Retryable::Transient)

                        // Try and downcast the hyper error to io::Error if that is the
                        // underlying error, and try and classify it.
                        } else if let Some(io_error) =
                            get_source_error_type::<std::io::Error>(hyper_error)
                        {
                            Some(classify_io_error(io_error))
                        } else {
                            Some(Retryable::Fatal)
                        }
                    } else {
                        Some(Retryable::Fatal)
                    }
                    #[cfg(target_arch = "wasm32")]
                    Some(Retryable::Fatal)
                } else {
                    // We omit checking if error.is_status() since we check that already.
                    // However, if Response::error_for_status is used the status will still
                    // remain in the response object.
                    None
                }
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn classify_io_error(error: &std::io::Error) -> Retryable {
    match error.kind() {
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => {
            Retryable::Transient
        }
        _ => Retryable::Fatal,
    }
}

/// Downcasts the given err source into T.
#[cfg(not(target_arch = "wasm32"))]
fn get_source_error_type<T: std::error::Error + 'static>(
    err: &dyn std::error::Error,
) -> Option<&T> {
    let mut source = err.source();

    while let Some(err) = source {
        if let Some(hyper_err) = err.downcast_ref::<T>() {
            return Some(hyper_err);
        }

        source = err.source();
    }
    None
}

use super::{BoxFuture, Client, Error, ErrorKind, Request, Response, TransportClient};

/// Ordered JSON-RPC endpoint failover.
///
/// Each member owns its endpoint and authentication policy. A request advances
/// only after a retryable transport/status failure; protocol errors and remote
/// JSON-RPC failures are returned by the endpoint that produced them.
#[derive(Clone, Debug)]
pub struct Failover<C> {
    clients: Vec<C>,
}

impl<C> Failover<C> {
    pub fn new(clients: Vec<C>) -> Result<Self, Error> {
        if clients.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "JSON-RPC failover requires at least one endpoint",
            ));
        }
        Ok(Self { clients })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.clients.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    fn finish<T>(
        result: Result<T, Error>,
        last_error: &mut Option<Error>,
    ) -> Option<Result<T, Error>> {
        match result {
            Ok(value) => Some(Ok(value)),
            Err(error) if error.is_retryable() => {
                *last_error = Some(error);
                None
            }
            Err(error) => Some(Err(error)),
        }
    }

    fn exhausted(last_error: Option<Error>) -> Error {
        match last_error {
            Some(error) => error,
            None => Error::new(
                ErrorKind::InvalidRequest,
                "JSON-RPC failover has no configured endpoint",
            ),
        }
    }
}

impl<T> Failover<TransportClient<T>> {
    /// Adds the same header to every endpoint without exposing its value in debug output.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        let value = value.into();
        for client in &mut self.clients {
            client.headers.push((name.clone(), value.clone()));
        }
        self
    }
}

impl<T> TransportClient<T>
where
    T: Clone,
{
    /// Builds ordered endpoint failover over one reusable HTTP transport.
    pub fn from_endpoints<I, E>(transport: T, endpoints: I) -> Result<Failover<Self>, Error>
    where
        I: IntoIterator<Item = E>,
        E: Into<String>,
    {
        Failover::new(
            endpoints
                .into_iter()
                .map(|endpoint| Self::new(transport.clone(), endpoint))
                .collect(),
        )
    }
}

impl<C> Client for Failover<C>
where
    C: Client,
{
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            let mut last_error = None;
            for client in &self.clients {
                let result = client.request(request.clone()).await;
                let Some(done) = Self::finish(result, &mut last_error) else {
                    continue;
                };
                return done;
            }
            Err(Self::exhausted(last_error))
        })
    }

    fn batch<'a>(&'a self, requests: Vec<Request>) -> BoxFuture<'a, Result<Vec<Response>, Error>> {
        Box::pin(async move {
            let mut last_error = None;
            for client in &self.clients {
                let result = client.batch(requests.clone()).await;
                let Some(done) = Self::finish(result, &mut last_error) else {
                    continue;
                };
                return done;
            }
            Err(Self::exhausted(last_error))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::{RawJson, RequestId};

    #[derive(Clone)]
    struct Stub {
        result: Result<Response, Error>,
        calls: Arc<AtomicUsize>,
    }

    impl Client for Stub {
        fn request<'a>(&'a self, _request: Request) -> BoxFuture<'a, Result<Response, Error>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let result = self.result.clone();
            Box::pin(async move { result })
        }

        fn batch<'a>(
            &'a self,
            _requests: Vec<Request>,
        ) -> BoxFuture<'a, Result<Vec<Response>, Error>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let result = self.result.clone().map(|response| vec![response]);
            Box::pin(async move { result })
        }
    }

    #[test]
    fn advances_after_retryable_failure() {
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let expected = Response {
            id: RequestId::Number(1),
            result: Ok(RawJson::from_serializable(&true).expect("boolean JSON must encode")),
        };
        let client = Failover::new(vec![
            Stub {
                result: Err(Error::new(ErrorKind::HttpStatus(503), "unavailable")),
                calls: Arc::clone(&first_calls),
            },
            Stub {
                result: Ok(expected.clone()),
                calls: Arc::clone(&second_calls),
            },
        ])
        .expect("two endpoints must be valid");
        let request = Request::new(RequestId::Number(1), "method", &[] as &[u8])
            .expect("request must encode");

        let actual = futures_executor::block_on(client.request(request))
            .expect("second endpoint must succeed");

        assert_eq!(actual, expected);
        assert_eq!(first_calls.load(Ordering::Relaxed), 1);
        assert_eq!(second_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stops_after_non_retryable_failure() {
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let client = Failover::new(vec![
            Stub {
                result: Err(Error::new(ErrorKind::HttpStatus(400), "invalid")),
                calls: Arc::clone(&first_calls),
            },
            Stub {
                result: Err(Error::new(ErrorKind::HttpStatus(503), "unavailable")),
                calls: Arc::clone(&second_calls),
            },
        ])
        .expect("two endpoints must be valid");
        let request = Request::new(RequestId::Number(1), "method", &[] as &[u8])
            .expect("request must encode");

        let error = futures_executor::block_on(client.request(request))
            .expect_err("client error must be returned");

        assert_eq!(error.kind, ErrorKind::HttpStatus(400));
        assert_eq!(first_calls.load(Ordering::Relaxed), 1);
        assert_eq!(second_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn endpoint_constructor_preserves_order_and_redacts_headers() {
        #[derive(Clone)]
        enum NeverTransport {
            Value,
        }
        let failover = TransportClient::from_endpoints(
            NeverTransport::Value,
            ["http://first.invalid", "http://second.invalid"],
        )
        .expect("two endpoints must be valid")
        .with_header("authorization", "Bearer hidden");

        assert_eq!(failover.len(), 2);
        let debug = format!("{failover:?}");
        assert!(debug.contains("authorization"));
        assert!(!debug.contains("Bearer hidden"));
    }
}

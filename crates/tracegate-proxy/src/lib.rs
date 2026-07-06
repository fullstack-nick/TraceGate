use std::{convert::Infallible, future::Future, net::SocketAddr, sync::Arc, time::Instant};

use bytes::Bytes;
use http::{
    HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
    header::{
        CONNECTION, CONTENT_LENGTH, HOST, HeaderName, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE,
        TRAILER, TRANSFER_ENCODING, UPGRADE,
    },
};
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder as ServerBuilder,
};
use serde::Serialize;
use thiserror::Error;
use tokio::{net::TcpListener, time::timeout};
use tracegate_core::{
    AppConfig, FORWARDED_FOR_HEADER, FORWARDED_HOST_HEADER, FORWARDED_PROTO_HEADER, Route, Router,
    Upstream, request_id_from_headers, request_id_header_value,
};

type ProxyBody = BoxBody<Bytes, hyper::Error>;
type ProxyClient = Client<HttpConnector, ProxyBody>;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("failed to bind listener: {0}")]
    Bind(#[from] std::io::Error),
}

#[derive(Clone)]
struct Proxy {
    router: Arc<Router>,
    client: ProxyClient,
}

#[derive(Clone)]
struct RequestTemplate {
    method: Method,
    uri: Uri,
    version: Version,
    headers: HeaderMap,
}

#[derive(Debug)]
enum AttemptError {
    Timeout,
    Transport(String),
    BuildRequest(String),
}

#[derive(Debug, Serialize)]
pub struct RequestLogRecord {
    pub request_id: String,
    pub method: String,
    pub path: String,
    pub route_id: Option<String>,
    pub upstream: Option<String>,
    pub status: u16,
    pub latency_ms: u128,
    pub error: Option<String>,
}

impl RequestLogRecord {
    fn emit(&self) {
        tracing::info!(
            request_id = %self.request_id,
            method = %self.method,
            path = %self.path,
            route_id = self.route_id.as_deref().unwrap_or(""),
            upstream = self.upstream.as_deref().unwrap_or(""),
            status = self.status,
            latency_ms = self.latency_ms,
            error = self.error.as_deref().unwrap_or(""),
            "request complete"
        );
    }
}

pub async fn serve(config: AppConfig) -> Result<(), ProxyError> {
    let listener = TcpListener::bind(config.listen).await?;
    serve_listener(listener, config, std::future::pending::<()>()).await
}

pub async fn serve_listener<S>(
    listener: TcpListener,
    config: AppConfig,
    shutdown: S,
) -> Result<(), ProxyError>
where
    S: Future<Output = ()> + Send,
{
    let proxy = Proxy::new(config);
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                break;
            }
            accepted = listener.accept() => {
                let (stream, remote_addr) = accepted?;
                let proxy = proxy.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let proxy = proxy.clone();
                        async move { proxy.handle(request, remote_addr).await }
                    });

                    let io = TokioIo::new(stream);
                    if let Err(err) = ServerBuilder::new(TokioExecutor::new())
                        .serve_connection_with_upgrades(io, service)
                        .await
                    {
                        tracing::warn!(error = %err, "connection failed");
                    }
                });
            }
        }
    }

    Ok(())
}

impl Proxy {
    fn new(config: AppConfig) -> Self {
        let mut connector = HttpConnector::new();
        connector.enforce_http(true);
        let client = Client::builder(TokioExecutor::new()).build(connector);

        Self {
            router: Arc::new(Router::new(config.routes)),
            client,
        }
    }

    async fn handle(
        &self,
        request: Request<Incoming>,
        remote_addr: SocketAddr,
    ) -> Result<Response<ProxyBody>, Infallible> {
        let started = Instant::now();
        let request_id = request_id_from_headers(request.headers());
        let method = request.method().clone();
        let path = request
            .uri()
            .path_and_query()
            .map(|path| path.as_str().to_owned())
            .unwrap_or_else(|| "/".to_owned());
        let host = request
            .headers()
            .get(HOST)
            .and_then(|value| value.to_str().ok());

        let Some(matched) = self.router.match_route(host, request.uri().path()) else {
            let response = response_with_request_id(
                StatusCode::NOT_FOUND,
                "no route matched request",
                &request_id,
            );
            self.log_request(RequestLogRecord {
                request_id,
                method: method.to_string(),
                path,
                route_id: None,
                upstream: None,
                status: response.status().as_u16(),
                latency_ms: started.elapsed().as_millis(),
                error: Some("no_route".to_owned()),
            });
            return Ok(response);
        };

        let upstream = matched.route.select_upstream();
        let upstream_origin = upstream.origin();
        let retry_eligible = retry_eligible(&method, request.headers());
        let template = RequestTemplate {
            method,
            uri: request.uri().clone(),
            version: request.version(),
            headers: request.headers().clone(),
        };

        let result = if retry_eligible {
            drop(request.into_body());
            self.request_with_retries(
                &matched.route,
                &upstream,
                &template,
                &request_id,
                remote_addr,
            )
            .await
        } else {
            self.request_once(
                &matched.route,
                &upstream,
                &template,
                request.into_body().boxed(),
                &request_id,
                remote_addr,
            )
            .await
        };

        let (response, error) = match result {
            Ok(response) => (response, None),
            Err(AttemptError::Timeout) => (
                response_with_request_id(
                    StatusCode::GATEWAY_TIMEOUT,
                    "upstream request timed out",
                    &request_id,
                ),
                Some("timeout".to_owned()),
            ),
            Err(AttemptError::Transport(err)) => (
                response_with_request_id(
                    StatusCode::BAD_GATEWAY,
                    "upstream request failed",
                    &request_id,
                ),
                Some(err),
            ),
            Err(AttemptError::BuildRequest(err)) => (
                response_with_request_id(
                    StatusCode::BAD_GATEWAY,
                    "failed to build upstream request",
                    &request_id,
                ),
                Some(err),
            ),
        };

        self.log_request(RequestLogRecord {
            request_id,
            method: template.method.to_string(),
            path,
            route_id: Some(matched.route.id),
            upstream: Some(upstream_origin),
            status: response.status().as_u16(),
            latency_ms: started.elapsed().as_millis(),
            error,
        });

        Ok(response)
    }

    async fn request_with_retries(
        &self,
        route: &Route,
        upstream: &Upstream,
        template: &RequestTemplate,
        request_id: &str,
        remote_addr: SocketAddr,
    ) -> Result<Response<ProxyBody>, AttemptError> {
        let attempts = route.retries.saturating_add(1);
        let mut last_error = None;

        for _ in 0..attempts {
            let body = Empty::<Bytes>::new()
                .map_err(|never| match never {})
                .boxed();
            match self
                .request_once(route, upstream, template, body, request_id, remote_addr)
                .await
            {
                Ok(response) => return Ok(response),
                Err(err) => last_error = Some(err),
            }
        }

        Err(last_error.unwrap_or_else(|| AttemptError::Transport("no attempts made".to_owned())))
    }

    async fn request_once(
        &self,
        route: &Route,
        upstream: &Upstream,
        template: &RequestTemplate,
        body: ProxyBody,
        request_id: &str,
        remote_addr: SocketAddr,
    ) -> Result<Response<ProxyBody>, AttemptError> {
        let request = build_upstream_request(template, body, upstream, request_id, remote_addr)?;
        let response = timeout(route.timeout, self.client.request(request))
            .await
            .map_err(|_| AttemptError::Timeout)?
            .map_err(|err| AttemptError::Transport(err.to_string()))?;

        let mut response = response.map(|body| body.boxed());
        response.headers_mut().insert(
            HeaderName::from_static(tracegate_core::REQUEST_ID_HEADER),
            request_id_header_value(request_id),
        );

        Ok(response)
    }

    fn log_request(&self, record: RequestLogRecord) {
        record.emit();
    }
}

fn build_upstream_request(
    template: &RequestTemplate,
    body: ProxyBody,
    upstream: &Upstream,
    request_id: &str,
    remote_addr: SocketAddr,
) -> Result<Request<ProxyBody>, AttemptError> {
    let path_and_query = template
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let uri: Uri = format!(
        "{}://{}{}",
        upstream.scheme, upstream.authority, path_and_query
    )
    .parse()
    .map_err(|err| AttemptError::BuildRequest(format!("{err}")))?;

    let mut request = Request::new(body);
    *request.method_mut() = template.method.clone();
    *request.uri_mut() = uri;
    *request.version_mut() = template.version;
    *request.headers_mut() = template.headers.clone();

    remove_hop_by_hop_headers(request.headers_mut());
    request
        .headers_mut()
        .insert(HOST, header_value(&upstream.authority)?);
    request.headers_mut().insert(
        HeaderName::from_static(tracegate_core::REQUEST_ID_HEADER),
        request_id_header_value(request_id),
    );

    if let Some(host) = template.headers.get(HOST).cloned() {
        request
            .headers_mut()
            .insert(HeaderName::from_static(FORWARDED_HOST_HEADER), host);
    }
    request.headers_mut().insert(
        HeaderName::from_static(FORWARDED_FOR_HEADER),
        header_value(&remote_addr.ip().to_string())?,
    );
    request.headers_mut().insert(
        HeaderName::from_static(FORWARDED_PROTO_HEADER),
        HeaderValue::from_static("http"),
    );

    Ok(request)
}

fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
    for header in [
        CONNECTION,
        HeaderName::from_static("keep-alive"),
        PROXY_AUTHENTICATE,
        PROXY_AUTHORIZATION,
        TE,
        TRAILER,
        TRANSFER_ENCODING,
        UPGRADE,
    ] {
        headers.remove(header);
    }
}

fn response_with_request_id(
    status: StatusCode,
    body: &'static str,
    request_id: &str,
) -> Response<ProxyBody> {
    let mut response = Response::new(full_body(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        HeaderName::from_static(tracegate_core::REQUEST_ID_HEADER),
        request_id_header_value(request_id),
    );
    response
}

fn full_body(body: &'static str) -> ProxyBody {
    Full::new(Bytes::from_static(body.as_bytes()))
        .map_err(|never| match never {})
        .boxed()
}

fn header_value(value: &str) -> Result<HeaderValue, AttemptError> {
    HeaderValue::from_str(value).map_err(|err| AttemptError::BuildRequest(err.to_string()))
}

pub fn retry_eligible(method: &Method, headers: &HeaderMap) -> bool {
    matches!(method, &Method::GET | &Method::HEAD | &Method::OPTIONS)
        && headers
            .get(CONTENT_LENGTH)
            .map(|value| value == "0")
            .unwrap_or(true)
        && !headers.contains_key(TRANSFER_ENCODING)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;

    #[test]
    fn retries_only_empty_idempotent_methods() {
        assert!(retry_eligible(&Method::GET, &HeaderMap::new()));
        assert!(retry_eligible(&Method::HEAD, &HeaderMap::new()));
        assert!(!retry_eligible(&Method::POST, &HeaderMap::new()));

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("12"));
        assert!(!retry_eligible(&Method::GET, &headers));
    }

    #[test]
    fn request_log_record_serializes_required_fields() {
        let value = serde_json::to_value(RequestLogRecord {
            request_id: "req".to_owned(),
            method: "GET".to_owned(),
            path: "/api/users".to_owned(),
            route_id: Some("users".to_owned()),
            upstream: Some("http://users-service:3000".to_owned()),
            status: 200,
            latency_ms: 3,
            error: None,
        })
        .unwrap();

        for field in [
            "request_id",
            "method",
            "path",
            "route_id",
            "upstream",
            "status",
            "latency_ms",
            "error",
        ] {
            assert!(value.get(field).is_some(), "missing field {field}");
        }
    }
}

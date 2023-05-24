//! [axum](https://github.com/tokio-rs/axum) OpenTelemetry Metrics middleware with prometheus exporter
//!
//! ## Simple Usage
//! ```
//! use axum_otel_metrics::HttpMetricsLayerBuilder;
//! use axum::{response::Html, routing::get, Router};
//!
//! let metrics = HttpMetricsLayerBuilder::new()
//!     .build();
//!
//! let app = Router::<()>::new()
//!     // export metrics at `/metrics` endpoint
//!     .merge(metrics.routes())
//!     .route("/", get(handler))
//!     .route("/hello", get(handler))
//!     .route("/world", get(handler))
//!     // add the metrics middleware
//!     .layer(metrics);
//!
//! async fn handler() -> Html<&'static str> {
//!     Html("<h1>Hello, World!</h1>")
//! }
//! ```
//!
//! ## Advanced Usage
//! ```
//! use axum_otel_metrics::HttpMetricsLayerBuilder;
//! use axum::{response::Html, routing::get, Router};
//!
//! let metrics = HttpMetricsLayerBuilder::new()
//! .with_service_name(env!("CARGO_PKG_NAME").to_string())
//! .with_service_version(env!("CARGO_PKG_VERSION").to_string())
//! .with_prefix("axum_metrics_demo".to_string())
//! .with_labels(vec![("env".to_string(), "testing".to_string())].into_iter().collect())
//! .build();
//!
//! let app = Router::<()>::new()
//!     // export metrics at `/metrics` endpoint
//!     .merge(metrics.routes())
//!     .route("/", get(handler))
//!     .route("/hello", get(handler))
//!     .route("/world", get(handler))
//!     // add the metrics middleware
//!     .layer(metrics);
//!
//! async fn handler() -> Html<&'static str> {
//!     Html("<h1>Hello, World!</h1>")
//! }
//! ```

use std::time::Duration;
use axum::http::Response;
use axum::{extract::MatchedPath, extract::State, http::Request, response::IntoResponse, routing::get, Router};
use std::collections::HashMap;

use std::future::Future;
use std::pin::Pin;
use std::task::Poll::Ready;
use std::task::{Context, Poll};
use std::time::Instant;

use opentelemetry_prometheus::PrometheusExporter;

use prometheus::{Encoder, TextEncoder, Registry};

use opentelemetry::{Key, KeyValue, Value};

use opentelemetry::metrics::{Counter, Histogram};

use opentelemetry::metrics::{Meter, MeterProvider as _, Unit};

use opentelemetry_prometheus::ExporterBuilder;
use opentelemetry::sdk::metrics::{new_view, Aggregation, Instrument, MeterProvider, Stream};
use opentelemetry::sdk::resource::{
    EnvResourceDetector, SdkProvidedResourceDetector, TelemetryResourceDetector,
};
use opentelemetry_semantic_conventions::resource::{SERVICE_NAME, TELEMETRY_SDK_VERSION};


use opentelemetry::{global, Context as OtelContext};

use tower::{Layer, Service};

use futures_util::ready;
use opentelemetry::sdk::Resource;
use pin_project_lite::pin_project;

#[derive(Clone)]
pub struct Metric {
    pub requests_total: Counter<u64>,

    // before opentelemetry 0.18.0, Histogram called ValueRecorder
    pub req_duration: Histogram<f64>,

    pub req_size: Histogram<u64>,
}

#[derive(Clone)]
pub struct MetricState {
    registry: prometheus::Registry,
    pub metric: Metric,
    skipper: PathSkipper,
}

#[derive(Clone)]
pub struct HttpMetrics<S> {
    pub(crate) state: MetricState,
    service: S,
}

#[derive(Clone)]
pub struct HttpMetricsLayer {
    pub(crate) state: MetricState,
}

// TODO support custom buckets
// allocation not allowed in statics: static HTTP_REQ_DURATION_HISTOGRAM_BUCKETS: Vec<f64> = vec![0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];
const HTTP_REQ_DURATION_HISTOGRAM_BUCKETS: &[f64] = &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

// write .005 * 1000, .01 * 1000, .025 * 1000, .05 * 1000, .1 * 1000, .25 * 1000, .5 * 1000, 1 * 1000, 2.5 * 1000, 5 * 1000, 10 * 1000
const HTTP_REQ_DURATION_MS_HISTOGRAM_BUCKETS:  &[f64] = &[5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0];

const KB: f64 = 1024.0;
const MB: f64 = 1024.0 * KB;

const HTTP_REQ_SIZE_HISTOGRAM_BUCKETS:  &[f64] = &[
    1.0 * KB,  // 1 KB
    2.0 * KB,  // 2 KB
    5.0 * KB,  // 5 KB
    10.0 * KB, // 10 KB
    100.0 * KB, // 100 KB
    500.0 * KB, // 500 KB
    1.0 * MB, // 1 MB
    2.5 * MB, // 2 MB
    5.0 * MB, // 5 MB
    10.0 * MB, // 10 MB
];

impl HttpMetricsLayer {
    pub fn routes<S>(&self) -> Router<S> {
        Router::new()
            .route("/metrics", get(Self::exporter_handler))
            .with_state(self.state.clone())
    }

    // TODO use a static global exporter like autometrics-rs?
    // https://github.com/autometrics-dev/autometrics-rs/blob/d3e7bffeede43f6c77b6a992b0443c0fca34003f/autometrics/src/prometheus_exporter.rs#L10
    pub async fn exporter_handler(state: State<MetricState>) -> impl IntoResponse {
        // tracing::trace!("exporter_handler called");
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        encoder.encode(&state.registry.gather(), &mut buffer).unwrap();
        // return metrics
        String::from_utf8(buffer).unwrap()
    }
}

#[derive(Clone)]
pub struct PathSkipper {
    skip: fn(&str) -> bool,
}

impl PathSkipper {
    pub fn new(skip: fn(&str) -> bool) -> Self {
        Self { skip }
    }
}

impl Default for PathSkipper {
    fn default() -> Self {
        Self {
            skip: |s| {
                s.starts_with("/metrics")
                || s.starts_with("/favicon.ico")
            },
        }
    }
}

#[derive(Clone, Default)]
pub struct HttpMetricsLayerBuilder {
    service_name: Option<String>,
    service_version: Option<String>,
    prefix: Option<String>,
    labels: Option<HashMap<String, String>>,
    skipper: PathSkipper,
}

impl HttpMetricsLayerBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_service_name(mut self, service_name: String) -> Self {
        self.service_name = Some(service_name);
        self
    }

    pub fn with_service_version(mut self, service_version: String) -> Self {
        self.service_version = Some(service_version);
        self
    }

    pub fn with_prefix(mut self, prefix: String) -> Self {
        self.prefix = Some(prefix);
        self
    }

    pub fn with_labels(mut self, labels: HashMap<String, String>) -> Self {
        self.labels = Some(labels);
        self
    }

    pub fn with_skipper(mut self, skipper: PathSkipper) -> Self {
        self.skipper = skipper;
        self
    }

    pub fn build(self) -> HttpMetricsLayer {
        let mut resource = vec![];
        if let Some(service_name) = self.service_name {
            resource.push(KeyValue::new("service.name", service_name));
        }
        if let Some(service_version) = self.service_version {
            resource.push(KeyValue::new("service.version", service_version));
        }

        let res = if resource.is_empty() {
            Resource::from_detectors(
                Duration::from_secs(6),
                vec![
                    Box::new(SdkProvidedResourceDetector),
                    Box::new(EnvResourceDetector::new()),
                    Box::new(TelemetryResourceDetector),
                ],
            )
        } else {
            Resource::from_detectors(
                Duration::from_secs(6),
                vec![
                    Box::new(SdkProvidedResourceDetector),
                    Box::new(EnvResourceDetector::new()),
                    Box::new(TelemetryResourceDetector),
                ],
            ).merge(&mut Resource::new(resource))
        };

        let registry = if let Some(prefix) = self.prefix {
            prometheus::Registry::new_custom(Some(prefix), self.labels).expect("create prometheus registry")
        } else {
            prometheus::Registry::new()
        };
        // init global meter provider and prometheus exporter
        let exporter = opentelemetry_prometheus::exporter().with_registry(registry.clone()).build().unwrap();

        let provider = MeterProvider::builder()
            .with_resource(res)
            .with_reader(exporter)
            .with_view(
                new_view(
                    Instrument::new().name("*_duration_milliseconds"),
                    Stream::new().aggregation(Aggregation::ExplicitBucketHistogram {
                        boundaries: HTTP_REQ_DURATION_MS_HISTOGRAM_BUCKETS.to_vec(),
                        record_min_max: true,
                    }),
                )
                    .unwrap(),
            )
            .with_view(
                new_view(
                    Instrument::new().name("*_duration_seconds"),
                    Stream::new().aggregation(Aggregation::ExplicitBucketHistogram {
                        boundaries: HTTP_REQ_DURATION_HISTOGRAM_BUCKETS.to_vec(),
                        record_min_max: true,
                    }),
                )
                    .unwrap(),
            )
            .with_view(
                new_view(
                    Instrument::new().name("*request_size_bytes"),
                    Stream::new().aggregation(Aggregation::ExplicitBucketHistogram {
                        boundaries: HTTP_REQ_SIZE_HISTOGRAM_BUCKETS.to_vec(),
                        record_min_max: true,
                    }),
                )
                    .unwrap(),
            )
        .build();

        // init the global meter provider
        global::set_meter_provider(provider.clone());
        // this must called after the global meter provider has ben initialized
        // let meter = global::meter("axum-app");
        let meter = provider.meter("axum-app");

        let requests_total = meter
            .u64_counter("requests_total")
            .with_description("How many HTTP requests processed, partitioned by status code and HTTP method.")
            .init();

        let req_duration = meter
            .f64_histogram("request_duration_seconds")
            .with_description("The HTTP request latencies in seconds.")
            .init();

        let req_size = meter
            .u64_histogram("request_size_bytes")
            .with_description("The HTTP request sizes in bytes.")
            .init();

        let meter_state = MetricState {
            registry,
            metric: Metric {
                requests_total: requests_total,
                req_duration: req_duration,
                req_size: req_size,
            },
            skipper: self.skipper,
        };

        HttpMetricsLayer { state: meter_state }
    }
}

impl<S> Layer<S> for HttpMetricsLayer {
    type Service = HttpMetrics<S>;

    fn layer(&self, service: S) -> Self::Service {
        HttpMetrics {
            state: self.state.clone(),
            service,
        }
    }
}

pin_project! {
    /// Response future for [`HttpMetrics`].
    pub struct ResponseFuture<F> {
        #[pin]
        inner: F,
        start: Instant,
        state: MetricState,
        path: String,
        method: String,
        req_size: u64,
    }
}

impl<S, R, ResBody> Service<Request<R>> for HttpMetrics<S>
where
    S: Service<Request<R>, Response = Response<ResBody>>,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: Request<R>) -> Self::Future {
        // axum::middleware::from_fn_with_state(self.state.clone(), track_metrics)

        let start = Instant::now();
        let method = req.method().clone().to_string();
        let path = if let Some(matched_path) = req.extensions().get::<MatchedPath>() {
            matched_path.as_str().to_owned()
        } else {
            req.uri().path().to_owned()
        };

        let req_size = compute_approximate_request_size(&req);

        ResponseFuture {
            inner: self.service.call(req),
            start,
            method,
            path,
            req_size: req_size as u64,
            state: self.state.clone(),
        }
    }
}

fn compute_approximate_request_size<T>(req: &Request<T>) -> usize {
    let mut s = 0;
    s += req.uri().path().len();
    s += req.method().as_str().len();

    req.headers().iter().for_each(|(k, v)| {
        s += k.as_str().len();
        s += v.as_bytes().len();
    });

    s += req.uri().host().map(|h| h.len()).unwrap_or(0);

    s += req.headers().get(http::header::CONTENT_LENGTH).map(|v| v.to_str().unwrap().parse::<usize>().unwrap_or(0)).unwrap_or(0);
    s
}


impl<F, B, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response<B>, E>>,
{
    type Output = Result<Response<B>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let response = ready!(this.inner.poll(cx))?;

        if (this.state.skipper.skip)(this.path.as_str()) {
            return Poll::Ready(Ok(response));
        }

        let latency = this.start.elapsed().as_secs_f64();
        let status = response.status().as_u16().to_string();

        let labels = [
            KeyValue {
                key: Key::from("method"),
                value: Value::from(this.method.clone()),
            },
            KeyValue::new("path", this.path.clone()),
            KeyValue::new("status", status),
        ];

        let cx = OtelContext::current();

        this.state.metric.requests_total.add(&cx, 1, &labels);

        this.state.metric.req_size.record(&cx, *this.req_size, &labels);

        this.state.metric.req_duration.record(&cx, latency, &labels);

        // tracing::trace!(
        //     "record metrics, method={} latency={} status={} labels={:?}",
        //     &this.method,
        //     &latency,
        //     &status,
        //     &labels
        // );

        Ready(Ok(response))
    }
}

#[cfg(test)]
mod tests {
    use crate::{HttpMetricsLayerBuilder, HTTP_REQ_DURATION_HISTOGRAM_BUCKETS};
    use axum::extract::State;
    use axum::routing::get;
    use axum::Router;
    use opentelemetry::{global, Context, KeyValue};
    use opentelemetry_prometheus::PrometheusExporter;
    use prometheus::{Encoder, TextEncoder};

    // init global meter provider and prometheus exporter
    fn init_meter() -> PrometheusExporter {
        let controller = controllers::basic(
            processors::factory(
                selectors::simple::histogram(HTTP_REQ_DURATION_HISTOGRAM_BUCKETS),
                aggregation::cumulative_temporality_selector(),
            ),
        )
        .build();

        // this will setup the global meter provider
        opentelemetry_prometheus::exporter(controller)
            .with_registry(prometheus::Registry::new_custom(Some("axum_app".into()), None).expect("create prometheus registry"))
            .init()
    }

    #[test]
    fn test_prometheus_exporter() {
        let cx = Context::current();
        let exporter = init_meter();
        let meter = global::meter("my-app");

        // Use two instruments
        let counter = meter.u64_counter("a.counter").with_description("Counts things").init();
        let recorder = meter.i64_histogram("a.histogram").with_description("Records values").init();

        counter.add(&cx, 100, &[KeyValue::new("key", "value")]);
        recorder.record(&cx, 100, &[KeyValue::new("key", "value")]);

        // Encode data as text or protobuf
        let encoder = TextEncoder::new();
        let metric_families = exporter.registry().gather();
        let mut result = Vec::new();
        encoder.encode(&metric_families, &mut result).expect("encode failed");
        println!("{}", String::from_utf8(result).unwrap());
    }

    #[test]
    fn test_builder() {
        let metrics = HttpMetricsLayerBuilder::new().build();
        let _app = Router::new()
            // export metrics at `/metrics` endpoint
            .merge(metrics.routes::<()>())
            .route("/", get(handler))
            .route("/hello", get(handler))
            .route("/world", get(handler))
            // add the metrics middleware
            .layer(metrics);

        async fn handler() -> &'static str {
            "<h1>Hello, World!</h1>"
        }
    }

    #[test]
    fn test_builder_with_state_router() {
        #[derive(Clone)]
        struct AppState {}

        let metrics = HttpMetricsLayerBuilder::new().build();
        let _app: Router<AppState> = Router::new()
            // export metrics at `/metrics` endpoint
            .merge(metrics.routes::<AppState>())
            .route("/", get(handler))
            .route("/hello", get(handler))
            .route("/world", get(handler))
            // add the metrics middleware
            .layer(metrics)
            .with_state(AppState {});

        async fn handler(_state: State<AppState>) -> &'static str {
            "<h1>Hello, World!</h1>"
        }
    }
}

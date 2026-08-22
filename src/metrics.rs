//! Prometheus metrics registry for upstream request instrumentation.
//!
//! Tracks request counts by error class, token usage, latency histograms, and
//! in-flight gauge per provider/route/model. The registry is exposed on a
//! `/metrics` endpoint (spec §6) and every request path records into it.

use prometheus_client::encoding::text::encode;
use prometheus_client::encoding::{EncodeLabelSet, EncodeLabelValue};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;
use std::fmt::Write;
use std::sync::Arc;

/// Time-to-first-token histogram bucket boundaries (seconds).
const TTFT_BUCKETS: [f64; 10] = [0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0];

/// Full request duration histogram bucket boundaries (seconds).
const DURATION_BUCKETS: [f64; 10] = [0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0];

/// Error classification for upstream request outcomes.
///
/// Used as the `error_class` label value on the request counter. The
/// [`EncodeLabelValue`] impl is hand-written because the derived encoding
/// (which uses `stringify!`) cannot express the empty-string value needed
/// for the successful (2xx) case.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum ErrorClass {
    /// 2xx success; encodes as the empty string label value.
    Empty,
    /// HTTP 429 Too Many Requests.
    Http429,
    /// HTTP 500 Internal Server Error.
    Http500,
    /// HTTP 503 Service Unavailable.
    Http503,
    /// Other 4xx client errors (400, 401, 404, …).
    Http4xx,
    /// Other 5xx server errors (502, 504, …).
    Http5xx,
    /// The upstream request timed out.
    Timeout,
    /// Network-layer failure (connection refused, DNS, TLS, …).
    Network,
    /// The upstream stream was truncated before completion.
    StreamTruncated,
}

impl ErrorClass {
    /// Classify an upstream HTTP status code into an [`ErrorClass`].
    ///
    /// 2xx maps to [`ErrorClass::Empty`]; 429, 500, and 503 get dedicated
    /// variants; all other 4xx and 5xx map to the coarse bucket.
    pub fn from_status(status: u16) -> Self {
        match status {
            200..=299 => Self::Empty,
            429 => Self::Http429,
            500 => Self::Http500,
            503 => Self::Http503,
            400..=499 => Self::Http4xx,
            501..=599 => Self::Http5xx,
            _ => Self::Network,
        }
    }
}

impl EncodeLabelValue for ErrorClass {
    fn encode(
        &self,
        encoder: &mut prometheus_client::encoding::LabelValueEncoder,
    ) -> Result<(), std::fmt::Error> {
        match self {
            Self::Empty => encoder.write_str("")?,
            Self::Http429 => encoder.write_str("http_429")?,
            Self::Http500 => encoder.write_str("http_500")?,
            Self::Http503 => encoder.write_str("http_503")?,
            Self::Http4xx => encoder.write_str("http_4xx")?,
            Self::Http5xx => encoder.write_str("http_5xx")?,
            Self::Timeout => encoder.write_str("timeout")?,
            Self::Network => encoder.write_str("network")?,
            Self::StreamTruncated => encoder.write_str("stream_truncated")?,
        }
        Ok(())
    }
}

/// Labels for the request counter: provider, route, model, error class.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ErrorLabels {
    provider: String,
    route: String,
    model: String,
    error_class: ErrorClass,
}

/// Labels for token counters: provider, route, model.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct TokenLabels {
    provider: String,
    route: String,
    model: String,
}

/// Labels for latency histograms: provider, route, model.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct LatencyLabels {
    provider: String,
    route: String,
    model: String,
}

/// Labels for the in-flight gauge: provider, route.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct GaugeLabels {
    provider: String,
    route: String,
}

/// Prometheus metrics registry for the proxy.
///
/// Holds all metric families that later request-handler tasks will record
/// into. Clones are cheap because each family wraps shared state.
#[derive(Clone)]
pub struct Metrics {
    registry: Arc<Registry>,
    requests: Family<ErrorLabels, Counter>,
    input_tokens: Family<TokenLabels, Counter>,
    output_tokens: Family<TokenLabels, Counter>,
    in_flight: Family<GaugeLabels, Gauge>,
    ttft: Family<LatencyLabels, Histogram>,
    duration: Family<LatencyLabels, Histogram>,
}

impl Metrics {
    /// Create a new [`Metrics`] instance with all families registered.
    pub fn new() -> Self {
        let requests = Family::<ErrorLabels, Counter>::default();
        let input_tokens = Family::<TokenLabels, Counter>::default();
        let output_tokens = Family::<TokenLabels, Counter>::default();
        let in_flight = Family::<GaugeLabels, Gauge>::default();
        let ttft = Family::<LatencyLabels, Histogram>::new_with_constructor(|| {
            Histogram::new(TTFT_BUCKETS)
        });
        let duration = Family::<LatencyLabels, Histogram>::new_with_constructor(|| {
            Histogram::new(DURATION_BUCKETS)
        });

        let mut registry = Registry::default();
        registry.register(
            "upstream_requests",
            "Number of upstream requests by error class",
            requests.clone(),
        );
        registry.register(
            "input_tokens",
            "Total input tokens sent to upstream providers",
            input_tokens.clone(),
        );
        registry.register(
            "output_tokens",
            "Total output tokens received from upstream providers",
            output_tokens.clone(),
        );
        registry.register(
            "upstream_requests_in_flight",
            "Number of upstream requests currently in flight",
            in_flight.clone(),
        );
        registry.register(
            "upstream_ttft_seconds",
            "Time to first token from upstream providers, in seconds",
            ttft.clone(),
        );
        registry.register(
            "upstream_duration_seconds",
            "Total upstream request duration, in seconds",
            duration.clone(),
        );

        Self {
            registry: Arc::new(registry),
            requests,
            input_tokens,
            output_tokens,
            in_flight,
            ttft,
            duration,
        }
    }

    /// Encode the registry into the given buffer using the OpenMetrics text format.
    pub fn encode(&self, buf: &mut String) -> Result<(), std::fmt::Error> {
        encode(buf, &self.registry)
    }

    /// Record one upstream request outcome.
    pub fn record_request(
        &self,
        provider: &str,
        route: &str,
        model: &str,
        error_class: ErrorClass,
    ) {
        self.requests
            .get_or_create(&ErrorLabels {
                provider: provider.to_string(),
                route: route.to_string(),
                model: model.to_string(),
                error_class,
            })
            .inc();
    }

    /// Record input and output token counts for one request.
    pub fn record_tokens(
        &self,
        provider: &str,
        route: &str,
        model: &str,
        input_tokens: u32,
        output_tokens: u32,
    ) {
        self.input_tokens
            .get_or_create(&TokenLabels {
                provider: provider.to_string(),
                route: route.to_string(),
                model: model.to_string(),
            })
            .inc_by(u64::from(input_tokens));
        self.output_tokens
            .get_or_create(&TokenLabels {
                provider: provider.to_string(),
                route: route.to_string(),
                model: model.to_string(),
            })
            .inc_by(u64::from(output_tokens));
    }

    /// Observe time-to-first-token for one request.
    pub fn observe_ttft(&self, provider: &str, route: &str, model: &str, seconds: f64) {
        self.ttft
            .get_or_create(&LatencyLabels {
                provider: provider.to_string(),
                route: route.to_string(),
                model: model.to_string(),
            })
            .observe(seconds);
    }

    /// Observe total request duration for one request.
    pub fn observe_duration(&self, provider: &str, route: &str, model: &str, seconds: f64) {
        self.duration
            .get_or_create(&LatencyLabels {
                provider: provider.to_string(),
                route: route.to_string(),
                model: model.to_string(),
            })
            .observe(seconds);
    }

    /// Increment the in-flight gauge for a provider/route pair.
    pub fn inc_in_flight(&self, provider: &str, route: &str) {
        self.in_flight
            .get_or_create(&GaugeLabels {
                provider: provider.to_string(),
                route: route.to_string(),
            })
            .inc();
    }

    /// Decrement the in-flight gauge for a provider/route pair.
    pub fn dec_in_flight(&self, provider: &str, route: &str) {
        self.in_flight
            .get_or_create(&GaugeLabels {
                provider: provider.to_string(),
                route: route.to_string(),
            })
            .dec();
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_classification() {
        assert_eq!(ErrorClass::from_status(200), ErrorClass::Empty);
        assert_eq!(ErrorClass::from_status(204), ErrorClass::Empty);
        assert_eq!(ErrorClass::from_status(429), ErrorClass::Http429);
        assert_eq!(ErrorClass::from_status(500), ErrorClass::Http500);
        assert_eq!(ErrorClass::from_status(503), ErrorClass::Http503);
        assert_eq!(ErrorClass::from_status(400), ErrorClass::Http4xx);
        assert_eq!(ErrorClass::from_status(404), ErrorClass::Http4xx);
        assert_eq!(ErrorClass::from_status(502), ErrorClass::Http5xx);
        assert_eq!(ErrorClass::from_status(504), ErrorClass::Http5xx);
    }

    #[test]
    fn empty_registry_encodes_without_error() {
        let metrics = Metrics::new();
        let mut buf = String::new();
        metrics.encode(&mut buf).expect("encode should succeed");
        // Empty families are not emitted by prometheus-client; the OpenMetrics
        // text format still requires the EOF marker, so this proves encode()
        // succeeded on a fresh Metrics instance.
        assert!(buf.contains("# EOF"));
    }

    #[test]
    fn recording_increments_counters() {
        let metrics = Metrics::new();
        metrics.record_request("deepseek", "deepseek/v4", "v4", ErrorClass::Empty);
        metrics.record_request("deepseek", "deepseek/v4", "v4", ErrorClass::Http429);
        metrics.record_tokens("deepseek", "deepseek/v4", "v4", 100, 50);
        metrics.observe_ttft("deepseek", "deepseek/v4", "v4", 0.25);
        metrics.observe_duration("deepseek", "deepseek/v4", "v4", 1.5);

        let mut buf = String::new();
        metrics.encode(&mut buf).expect("encode should succeed");

        // Request counter labels (spec: lowercase/underscored error classes).
        assert!(buf.contains("upstream_requests_total{provider=\"deepseek\",route=\"deepseek/v4\",model=\"v4\",error_class=\"\"} 1"));
        assert!(buf.contains("upstream_requests_total{provider=\"deepseek\",route=\"deepseek/v4\",model=\"v4\",error_class=\"http_429\"} 1"));

        // Token counters.
        assert!(buf.contains(
            "input_tokens_total{provider=\"deepseek\",route=\"deepseek/v4\",model=\"v4\"} 100"
        ));
        assert!(buf.contains(
            "output_tokens_total{provider=\"deepseek\",route=\"deepseek/v4\",model=\"v4\"} 50"
        ));

        // Histograms - labels are interleaved between the name and value.
        assert!(buf.contains("upstream_ttft_seconds_sum{provider=\"deepseek\",route=\"deepseek/v4\",model=\"v4\"} 0.25"));
        assert!(buf.contains("upstream_ttft_seconds_count{provider=\"deepseek\",route=\"deepseek/v4\",model=\"v4\"} 1"));
        assert!(buf.contains("upstream_duration_seconds_sum{provider=\"deepseek\",route=\"deepseek/v4\",model=\"v4\"} 1.5"));
        assert!(buf.contains("upstream_duration_seconds_count{provider=\"deepseek\",route=\"deepseek/v4\",model=\"v4\"} 1"));
    }

    #[test]
    fn in_flight_gauge_increments_and_decrements() {
        let metrics = Metrics::new();
        metrics.inc_in_flight("provider", "route");
        metrics.inc_in_flight("provider", "route");

        let mut buf = String::new();
        metrics.encode(&mut buf).expect("encode should succeed");
        assert!(
            buf.contains("upstream_requests_in_flight{provider=\"provider\",route=\"route\"} 2")
        );

        metrics.dec_in_flight("provider", "route");

        let mut buf = String::new();
        metrics.encode(&mut buf).expect("encode should succeed");
        assert!(
            buf.contains("upstream_requests_in_flight{provider=\"provider\",route=\"route\"} 1")
        );
    }
}

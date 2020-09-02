use crate::{
    config::{self, GlobalOptions},
    internal_events::{
        ApacheMetricsErrorResponse, ApacheMetricsEventReceived, ApacheMetricsHttpError,
        ApacheMetricsParseError, ApacheMetricsRequestCompleted,
    },
    shutdown::ShutdownSignal,
    Event, Pipeline,
};
use chrono::Utc;
use futures::{
    compat::{Future01CompatExt, Sink01CompatExt},
    future, stream, FutureExt, StreamExt, TryFutureExt,
};
use futures01::Sink;
use hyper::{Body, Client, Request};
use hyper_openssl::HttpsConnector;
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

mod parser;

#[derive(Deserialize, Serialize, Clone, Debug)]
struct ApacheMetricsConfig {
    endpoints: Vec<String>,
    #[serde(default = "default_scrape_interval_secs")]
    scrape_interval_secs: u64,
    #[serde(default = "default_namespace")]
    namespace: String,
}

pub fn default_scrape_interval_secs() -> u64 {
    15
}

pub fn default_namespace() -> String {
    "apache".to_string()
}

#[typetag::serde(name = "apache_metrics")]
impl crate::config::SourceConfig for ApacheMetricsConfig {
    fn build(
        &self,
        _name: &str,
        _globals: &GlobalOptions,
        shutdown: ShutdownSignal,
        out: Pipeline,
    ) -> crate::Result<super::Source> {
        let urls = self
            .endpoints
            .iter()
            .map(|endpoint| endpoint.parse::<http::Uri>())
            .collect::<Result<Vec<_>, _>>()
            .context(super::UriParseError)?;

        Ok(apache_metrics(
            urls,
            self.scrape_interval_secs,
            self.namespace.clone(),
            shutdown,
            out,
        ))
    }

    fn output_type(&self) -> crate::config::DataType {
        config::DataType::Metric
    }

    fn source_type(&self) -> &'static str {
        "apache_metrics"
    }
}

fn apache_metrics(
    urls: Vec<http::Uri>,
    interval: u64,
    namespace: String,
    shutdown: ShutdownSignal,
    out: Pipeline,
) -> super::Source {
    let out = out
        .sink_map_err(|e| error!("error sending metric: {:?}", e))
        .sink_compat();
    let task = tokio::time::interval(Duration::from_secs(interval))
        .take_until(shutdown.compat())
        .map(move |_| stream::iter(urls.clone())) // TODO remove clone?
        .flatten()
        .map(move |url| {
            let https = HttpsConnector::new().expect("TLS initialization failed");
            let client = Client::builder().build(https);

            let request = Request::get(&url)
                .body(Body::empty())
                .expect("error creating request");

            let start = Instant::now();
            let namespace = namespace.clone(); // TODO remove?
            client
                .request(request)
                .and_then(|response| async {
                    let (header, body) = response.into_parts();
                    let body = hyper::body::to_bytes(body).await?;
                    Ok((header, body))
                })
                .into_stream()
                .filter_map(move |response| {
                    future::ready(match response {
                        Ok((header, body)) if header.status == hyper::StatusCode::OK => {
                            emit!(ApacheMetricsRequestCompleted {
                                start,
                                end: Instant::now()
                            });

                            let byte_size = body.len();
                            let body = String::from_utf8_lossy(&body);

                            let (metrics, errors) = parser::parse(
                                &body,
                                &namespace,
                                Utc::now(),
                                Some(&BTreeMap::new()), // TODO
                            );

                            for err in errors {
                                emit!(ApacheMetricsParseError {
                                    error: err.into(),
                                    url: url.clone(),
                                });
                            }

                            if metrics.len() > 0 {
                                emit!(ApacheMetricsEventReceived {
                                    byte_size,
                                    count: metrics.len(),
                                });
                                Some(stream::iter(metrics).map(Event::Metric).map(Ok))
                            } else {
                                None
                            }
                        }
                        Ok((header, _)) => {
                            emit!(ApacheMetricsErrorResponse {
                                code: header.status,
                                url: &url,
                            });
                            None
                        }
                        Err(error) => {
                            emit!(ApacheMetricsHttpError { error, url: &url });
                            None
                        }
                    })
                })
                .flatten()
        })
        .flatten()
        .forward(out)
        .inspect(|_| info!("finished sending"));

    Box::new(task.boxed().compat())
}

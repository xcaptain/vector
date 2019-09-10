use crate::{
    buffers::Acker,
    event::Event,
    region::RegionOrEndpoint,
    sinks::util::{
        http::{HttpRetryLogic, HttpService},
        retries::FixedRetryPolicy,
        BatchServiceSink, Buffer, Compression, SinkExt,
    },
    template::Template,
    topology::config::{DataType, SinkConfig},
};
use futures::{stream::iter_ok, Future, Sink};
use http::{Method, Uri};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Body, Client, Request};
use hyper_tls::HttpsConnector;
use rusoto_core::signature::{SignedRequest, SignedRequestPayload};
use rusoto_core::{DefaultCredentialsProvider, ProvideAwsCredentials, Region};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::convert::TryInto;
use std::time::Duration;
use tower::ServiceBuilder;

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ElasticSearchConfig {
    pub host: String,
    pub index: Option<String>,
    pub doc_type: Option<String>,
    pub id_key: Option<String>,
    pub batch_size: Option<usize>,
    pub batch_timeout: Option<u64>,
    pub compression: Option<Compression>,
    pub provider: Option<Provider>,
    pub region: Option<RegionOrEndpoint>,

    // Tower Request based configuration
    pub request_in_flight_limit: Option<usize>,
    pub request_timeout_secs: Option<u64>,
    pub request_rate_limit_duration_secs: Option<u64>,
    pub request_rate_limit_num: Option<u64>,
    pub request_retry_attempts: Option<usize>,
    pub request_retry_backoff_secs: Option<u64>,

    pub basic_auth: Option<ElasticSearchBasicAuthConfig>,

    pub headers: Option<HashMap<String, String>>,
    pub query: Option<HashMap<String, String>>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ElasticSearchBasicAuthConfig {
    pub password: String,
    pub user: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Default,
    Aws,
}

#[typetag::serde(name = "elasticsearch")]
impl SinkConfig for ElasticSearchConfig {
    fn build(&self, acker: Acker) -> Result<(super::RouterSink, super::Healthcheck), String> {
        let sink = es(self, acker)?;
        let healthcheck = healthcheck(&self.host);

        Ok((sink, healthcheck))
    }

    fn input_type(&self) -> DataType {
        DataType::Log
    }
}

fn es(config: &ElasticSearchConfig, acker: Acker) -> Result<super::RouterSink, String> {
    let id_key = config.id_key.clone();
    let mut gzip = match config.compression.unwrap_or(Compression::Gzip) {
        Compression::None => false,
        Compression::Gzip => true,
    };

    let batch_size = config.batch_size.unwrap_or(bytesize::mib(10u64) as usize);
    let batch_timeout = config.batch_timeout.unwrap_or(1);

    let timeout = config.request_timeout_secs.unwrap_or(60);
    let in_flight_limit = config.request_in_flight_limit.unwrap_or(5);
    let rate_limit_duration = config.request_rate_limit_duration_secs.unwrap_or(1);
    let rate_limit_num = config.request_rate_limit_num.unwrap_or(5);
    let retry_attempts = config.request_retry_attempts.unwrap_or(usize::max_value());
    let retry_backoff_secs = config.request_retry_backoff_secs.unwrap_or(1);

    let index = if let Some(idx) = &config.index {
        Template::from(idx.as_str())
    } else {
        Template::from("vector-%Y.%m.%d")
    };
    let doc_type = config.doc_type.clone().unwrap_or("_doc".into());

    let policy = FixedRetryPolicy::new(
        retry_attempts,
        Duration::from_secs(retry_backoff_secs),
        HttpRetryLogic,
    );

    let authorization = config.basic_auth.clone().map(|auth| {
        let token = format!("{}:{}", auth.user, auth.password);
        format!("Basic {}", base64::encode(token.as_bytes()))
    });
    let headers = config
        .headers
        .as_ref()
        .unwrap_or(&HashMap::default())
        .clone();

    let mut path_query = url::form_urlencoded::Serializer::new(String::from("/_bulk"));
    if let Some(ref query) = config.query {
        for (p, v) in query {
            path_query.append_pair(&p[..], &v[..]);
        }
    }
    let uri = format!("{}{}", config.host, path_query.finish());
    let uri = uri.parse::<Uri>().expect("Invalid elasticsearch host");

    let region: Option<Region> = match config.region {
        Some(ref region) => Some(region.try_into().map_err(|err| format!("{}", err))?),
        None => None,
    };

    let credentials = match config.provider.as_ref().unwrap_or(&Provider::Default) {
        Provider::Default => None,
        Provider::Aws => {
            gzip = false;
            if region.is_none() {
                return Err("AWS provider requires a configured region".into());
            }
            Some(
                DefaultCredentialsProvider::new()
                    .map_err(|err| format!("Could not create AWS credentials provider: {}", err))?
                    .credentials()
                    .wait()
                    .map_err(|err| format!("Could not generate AWS credentials: {}", err))?,
            )
        }
    };

    let http_service = HttpService::new(move |body: Vec<u8>| {
        let mut builder = hyper::Request::builder();
        builder.method(Method::POST);
        builder.uri(&uri);

        match credentials {
            None => {
                builder.header("Content-Type", "application/x-ndjson");
                if gzip {
                    builder.header("Content-Encoding", "gzip");
                }

                for (header, value) in &headers {
                    builder.header(&header[..], &value[..]);
                }

                if let Some(ref auth) = authorization {
                    builder.header("Authorization", &auth[..]);
                }

                builder.body(body).unwrap()
            }
            Some(ref credentials) => {
                let mut request =
                    SignedRequest::new("POST", "es", region.as_ref().unwrap(), uri.path());
                request.set_hostname(uri.host().map(|s| s.into()));

                request.add_header("Content-Type", "application/x-ndjson");

                for (header, value) in &headers {
                    request.add_header(header, value);
                }

                request.set_payload(Some(body));

                request.sign_with_plus(&credentials, true);

                for (name, values) in request.headers() {
                    let header_name = name
                        .parse::<HeaderName>()
                        .expect("Could not parse header name.");
                    for value in values {
                        let header_value =
                            HeaderValue::from_bytes(value).expect("Could not parse header value.");
                        builder.header(&header_name, header_value);
                    }
                }

                // The SignedRequest ends up owning the body, so we have
                // to play games here
                let body = request.payload.take().unwrap();
                match body {
                    SignedRequestPayload::Buffer(body) => builder.body(body).unwrap(),
                    _ => unreachable!(),
                }
            }
        }
    });

    let service = ServiceBuilder::new()
        .concurrency_limit(in_flight_limit)
        .rate_limit(rate_limit_num, Duration::from_secs(rate_limit_duration))
        .retry(policy)
        .timeout(Duration::from_secs(timeout))
        .service(http_service);

    let sink = BatchServiceSink::new(service, acker)
        .batched_with_min(
            Buffer::new(gzip),
            batch_size,
            Duration::from_secs(batch_timeout),
        )
        .with_flat_map(move |e| iter_ok(encode_event(e, &index, &doc_type, &id_key)));

    Ok(Box::new(sink))
}

fn encode_event(
    event: Event,
    index: &Template,
    doc_type: &String,
    id_key: &Option<String>,
) -> Option<Vec<u8>> {
    let index = index
        .render_string(&event)
        .map_err(|keys| {
            warn!(
                message = "Keys do not exist on the event. Dropping event.",
                ?keys
            );
        })
        .ok()?;

    let mut action = json!({
        "index": {
            "_index": index,
            "_type": doc_type,
        }
    });
    maybe_set_id(
        id_key.as_ref(),
        action.pointer_mut("/index").unwrap(),
        &event,
    );

    let mut body = serde_json::to_vec(&action).unwrap();
    body.push(b'\n');

    serde_json::to_writer(&mut body, &event.into_log().unflatten()).unwrap();
    body.push(b'\n');
    Some(body)
}

fn healthcheck(host: &str) -> super::Healthcheck {
    let uri = format!("{}/_cluster/health", host);
    let request = Request::get(uri).body(Body::empty()).unwrap();

    let https = HttpsConnector::new(4).expect("TLS initialization failed");
    let client = Client::builder().build(https);
    let healthcheck = client
        .request(request)
        .map_err(|err| err.to_string())
        .and_then(|response| {
            if response.status() == hyper::StatusCode::OK {
                Ok(())
            } else {
                Err(format!("Unexpected status: {}", response.status()))
            }
        });

    Box::new(healthcheck)
}

fn maybe_set_id(key: Option<impl AsRef<str>>, doc: &mut serde_json::Value, event: &Event) {
    if let Some(val) = key.and_then(|k| event.as_log().get(&k.as_ref().into())) {
        let val = val.to_string_lossy();

        doc.as_object_mut()
            .unwrap()
            .insert("_id".into(), json!(val));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;
    use serde_json::json;

    #[test]
    fn sets_id_from_custom_field() {
        let id_key = Some("foo");
        let mut event = Event::from("butts");
        event
            .as_mut_log()
            .insert_explicit("foo".into(), "bar".into());
        let mut action = json!({});

        maybe_set_id(id_key, &mut action, &event);

        assert_eq!(json!({"_id": "bar"}), action);
    }

    #[test]
    fn doesnt_set_id_when_field_missing() {
        let id_key = Some("foo");
        let mut event = Event::from("butts");
        event
            .as_mut_log()
            .insert_explicit("not_foo".into(), "bar".into());
        let mut action = json!({});

        maybe_set_id(id_key, &mut action, &event);

        assert_eq!(json!({}), action);
    }

    #[test]
    fn doesnt_set_id_when_not_configured() {
        let id_key: Option<&str> = None;
        let mut event = Event::from("butts");
        event
            .as_mut_log()
            .insert_explicit("foo".into(), "bar".into());
        let mut action = json!({});

        maybe_set_id(id_key, &mut action, &event);

        assert_eq!(json!({}), action);
    }
}

#[cfg(test)]
#[cfg(feature = "es-integration-tests")]
mod integration_tests {
    use super::*;
    use crate::buffers::Acker;
    use crate::{
        event,
        test_util::{block_on, random_events_with_stream, random_string},
        topology::config::SinkConfig,
        Event,
    };
    use elastic::client::SyncClientBuilder;
    use futures::{Future, Sink};
    use hyper::{Body, Client, Request};
    use hyper_tls::HttpsConnector;
    use serde_json::{json, Value};

    #[test]
    fn structures_events_correctly() {
        let index = gen_index();
        let config = ElasticSearchConfig {
            host: "http://localhost:9200/".into(),
            index: Some(index.clone()),
            doc_type: Some("log_lines".into()),
            id_key: Some("my_id".into()),
            compression: Some(Compression::None),
            batch_size: Some(1),
            ..Default::default()
        };

        let (sink, _hc) = config.build(Acker::Null).unwrap();

        let mut input_event = Event::from("raw log line");
        input_event
            .as_mut_log()
            .insert_explicit("my_id".into(), "42".into());
        input_event
            .as_mut_log()
            .insert_explicit("foo".into(), "bar".into());

        let pump = sink.send(input_event.clone());
        block_on(pump).unwrap();

        // make sure writes all all visible
        block_on(flush(config.host)).unwrap();

        let client = SyncClientBuilder::new().build().unwrap();

        let response = client
            .search::<Value>()
            .index(index)
            .body(json!({
                "query": { "query_string": { "query": "*" } }
            }))
            .send()
            .unwrap();
        assert_eq!(1, response.total());

        let hit = response.into_hits().next().unwrap();
        assert_eq!("42", hit.id());

        let value = hit.into_document().unwrap();
        let expected = json!({
            "message": "raw log line",
            "my_id": "42",
            "foo": "bar",
            "timestamp": input_event.as_log()[&event::TIMESTAMP],
        });
        assert_eq!(expected, value);
    }

    #[test]
    fn insert_events() {
        let index = gen_index();
        let config = ElasticSearchConfig {
            host: "http://localhost:9200/".into(),
            index: Some(index.clone()),
            doc_type: Some("log_lines".into()),
            compression: Some(Compression::None),
            batch_size: Some(1),
            ..Default::default()
        };

        let (sink, _hc) = config.build(Acker::Null).unwrap();

        let (input, events) = random_events_with_stream(100, 100);

        let pump = sink.send_all(events);
        block_on(pump).unwrap();

        // make sure writes all all visible
        block_on(flush(config.host)).unwrap();

        let client = SyncClientBuilder::new().build().unwrap();

        let response = client
            .search::<Value>()
            .index(index)
            .body(json!({
                "query": { "query_string": { "query": "*" } }
            }))
            .send()
            .unwrap();

        assert_eq!(input.len() as u64, response.total());
        let input = input
            .into_iter()
            .map(|rec| serde_json::to_value(rec.into_log().unflatten()).unwrap())
            .collect::<Vec<_>>();
        for hit in response.into_hits() {
            let event = hit.into_document().unwrap();
            assert!(input.contains(&event));
        }
    }

    fn gen_index() -> String {
        format!("test-{}", random_string(10).to_lowercase())
    }

    fn flush(host: String) -> impl Future<Item = (), Error = String> {
        let uri = format!("{}/_flush", host);
        let request = Request::post(uri).body(Body::empty()).unwrap();

        let https = HttpsConnector::new(4).expect("TLS initialization failed");
        let client = Client::builder().build(https);
        client
            .request(request)
            .map_err(|err| err.to_string())
            .and_then(|response| {
                if response.status() == hyper::StatusCode::OK {
                    Ok(())
                } else {
                    Err(format!("Unexpected status: {}", response.status()))
                }
            })
    }

}

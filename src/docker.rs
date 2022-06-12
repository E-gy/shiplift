//! Main entrypoint for interacting with the Docker API.
//!
//! API Reference: <https://docs.docker.com/engine/api/v1.41/>

use std::{collections::HashMap, env, io, path::Path};

use futures_util::{stream::Stream, TryStreamExt};
use hyper::{client::HttpConnector, Body, Client, Method};
use mime::Mime;
use serde::{de, Deserialize, Serialize};
use url::form_urlencoded;

use std::io::Read;

use crate::{
    container::Containers,
    errors::{Error, Result},
    image::Images,
    network::Networks,
    service::Services,
    transport::{Headers, Payload, Transport},
    volume::Volumes,
    Uri,
};

#[cfg(feature = "chrono")]
use crate::datetime::{datetime_from_nano_timestamp, datetime_from_unix_timestamp};
#[cfg(feature = "chrono")]
use chrono::{DateTime, Utc};

#[cfg(feature = "rust-tls")]
use hyper_rustls::HttpsConnector;
#[cfg(feature = "native-tls")]
use hyper_tls::HttpsConnector;

#[cfg(feature = "tls")]
use hyper_openssl::HttpsConnector;
#[cfg(feature = "tls")]
use openssl::ssl::{SslConnector, SslFiletype, SslMethod};

#[cfg(feature = "unix-socket")]
use hyperlocal::UnixConnector;

/// Entrypoint interface for communicating with docker daemon
#[derive(Clone)]
pub struct Docker {
    transport: Transport,
}

fn get_http_connector() -> HttpConnector {
    let mut http = HttpConnector::new();
    http.enforce_http(false);

    http
}

/*
async fn read_to_bytes(f: &str) -> tokio::io::Result<Vec<u8>> {
    let mut f = tokio::fs::File::open(f).await?;
    let mut buffer = Vec::new();
    f.read_to_end(&mut buffer).await?;
    Ok(buffer)
}
*/
fn read_to_bytes(f: &str) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(f)?;
    let mut buffer = Vec::new();
    f.read_to_end(&mut buffer)?;
    Ok(buffer)
}

#[cfg(feature = "rust-tls")]
fn get_https_connector(docker_cert_path: &str) -> HttpsConnector<HttpConnector> {
    use hyper_rustls::HttpsConnectorBuilder;
    use rustls::{ClientConfig, RootCertStore, Certificate, PrivateKey};
    use rustls_pemfile::{Item, read_one, read_all};

    fn read_certs(f: &str) -> std::io::Result<Vec<Certificate>> {
        Ok(read_all(&mut std::io::BufReader::new(std::fs::File::open(f)?))?.into_iter().filter_map(|item| match item {
            Item::X509Certificate(x509) => Some(Certificate(x509)),
            _ => None,
        }).collect())
    }
    fn read_key(f: &str) -> std::io::Result<PrivateKey> {
        Ok(match read_one(&mut std::io::BufReader::new(std::fs::File::open(f)?))? {
            Some(Item::RSAKey(bytes)) | Some(Item::PKCS8Key(bytes)) => PrivateKey(bytes),
            // Some(Item::ECKey(_)) => Err(io::Error::other("EC keys not supported, i think, :("))?,
            // _ => Err(io::Error::other("Not a private key"))?,
            _ => panic!("Not a private key"), //FIXME Bad panic bad!
        })
    }

    HttpsConnectorBuilder::new()
        .with_tls_config(ClientConfig::builder()
            .with_safe_default_cipher_suites()
            .with_safe_default_kx_groups()
            .with_safe_default_protocol_versions()
            .unwrap() //FIXME handle errors do not panik
            .with_root_certificates({
                let mut store = RootCertStore::empty();
                if env::var("DOCKER_TLS_VERIFY").is_ok() {
                    store.add_parsable_certificates(&[read_to_bytes(&format!("{}/ca.pem", docker_cert_path)).unwrap()]); //FIXME handle errors do not panik
                }
                store
            })
            .with_single_cert(read_certs(&format!("{}/cert.pem", docker_cert_path)).unwrap(), read_key(&format!("{}/key.pem", docker_cert_path)).unwrap()) //FIXME handle errors do not panik
            .unwrap() //FIXME handle errors do not panik
        )
        .https_only()
        .enable_http1()
        .wrap_connector(get_http_connector())
}

#[cfg(feature = "native-tls")]
fn get_https_connector(docker_cert_path: &str) -> HttpsConnector<HttpConnector> {
    use hyper_tls::native_tls::{ TlsConnector, Certificate };
    use native_tls::Identity;

    let mut builder = TlsConnector::builder();
    if env::var("DOCKER_TLS_VERIFY").is_ok() {
        let bytes = read_to_bytes(&format!("{}/ca.pem", docker_cert_path)).unwrap();
        builder.add_root_certificate(Certificate::from_der(&bytes).or_else(|_| Certificate::from_pem(&bytes)).unwrap()); //FIXME handle errors do not panik
    }
    builder.identity(Identity::from_pkcs8(&read_to_bytes(&format!("{}/cert.pem", docker_cert_path)).unwrap(),&read_to_bytes(&format!("{}/key.pem", docker_cert_path)).unwrap()).unwrap()); //FIXME handle errors do not panik
    (
        get_http_connector(),
        builder.build().unwrap().into(), //FIXME handle errors do not panik
    ).into()
}

/// Constructs Docker for HTTP-only TCP connection
fn get_docker_for_tcp_http(tcp_host_str: String) -> Docker {
    Docker {
        transport: Transport::Tcp {
            client: Client::builder().build(get_http_connector()),
            host: tcp_host_str,
        },
    }
}

/// Constructs Docker for HTTPS TCP connection
#[cfg(any(feature = "rust-tls", feature = "native-tls"))]
fn get_docker_for_tcp_https(tcp_host_str: String, docker_cert_path: &str) -> Docker {
    Docker {
        transport: Transport::EncryptedTcp {
            client: Client::builder().build(get_https_connector(docker_cert_path)),
            host: tcp_host_str,
        },
    }
}

#[cfg(not(any(feature = "rust-tls", feature = "native-tls")))]
fn get_docker_for_tcp(tcp_host_str: String) -> Docker {
    get_docker_for_tcp_http(tcp_host_str)
}

#[cfg(any(feature = "rust-tls", feature = "native-tls"))]
fn get_docker_for_tcp(tcp_host_str: String) -> Docker {
    match &env::var("DOCKER_CERT_PATH") {
        Ok(certs) => get_docker_for_tcp_https(tcp_host_str, &certs),
        _ => get_docker_for_tcp_http(tcp_host_str),
    }
}

// https://docs.docker.com/reference/api/docker_remote_api_v1.17/
impl Docker {
    /// constructs a new Docker instance for a docker host listening at a url specified by an env var `DOCKER_HOST`,
    /// falling back on unix:///var/run/docker.sock
    pub fn new() -> Docker {
        Self::host(env::var("DOCKER_HOST").ok().as_ref().map(String::as_str).unwrap_or("unix:///var/run/docker.sock").parse().expect("invalid url"))
    }

    /// Creates a new docker instance for a docker host
    /// listening on a given Unix socket.
    #[cfg(feature = "unix-socket")]
    pub fn unix<S>(socket_path: S) -> Docker
    where
        S: Into<String>,
    {
        Docker {
            transport: Transport::Unix {
                client: Client::builder()
                    .pool_max_idle_per_host(0)
                    .build(UnixConnector),
                path: socket_path.into(),
            },
        }
    }

    /// constructs a new Docker instance for docker host listening at the given host url
    pub fn host(host: Uri) -> Docker {
        let tcp_host_str = format!(
            "{}://{}:{}",
            host.scheme_str().unwrap(),
            host.host().unwrap(),
            host.port_u16().unwrap_or(80)
        );

        match host.scheme_str() {
            #[cfg(feature = "unix-socket")]
            Some("unix") => Docker {
                transport: Transport::Unix {
                    client: Client::builder().build(UnixConnector),
                    path: host.path().to_owned(),
                },
            },

            #[cfg(not(feature = "unix-socket"))]
            Some("unix") => panic!("Unix socket support is disabled"),

            _ => get_docker_for_tcp(tcp_host_str),
        }
    }

    /// Exports an interface for interacting with docker images
    pub fn images(&'_ self) -> Images<'_> {
        Images::new(self)
    }

    /// Exports an interface for interacting with docker containers
    pub fn containers(&'_ self) -> Containers<'_> {
        Containers::new(self)
    }

    /// Exports an interface for interacting with docker services
    pub fn services(&'_ self) -> Services<'_> {
        Services::new(self)
    }

    pub fn networks(&'_ self) -> Networks<'_> {
        Networks::new(self)
    }

    pub fn volumes(&'_ self) -> Volumes<'_> {
        Volumes::new(self)
    }

    /// Returns version information associated with the docker daemon
    pub async fn version(&self) -> Result<Version> {
        self.get_json("/version").await
    }

    /// Returns information associated with the docker daemon
    pub async fn info(&self) -> Result<Info> {
        self.get_json("/info").await
    }

    /// Returns a simple ping response indicating the docker daemon is accessible
    pub async fn ping(&self) -> Result<String> {
        self.get("/_ping").await
    }

    /// Returns a stream of docker events
    pub fn events<'docker>(
        &'docker self,
        opts: &EventsOptions,
    ) -> impl Stream<Item = Result<Event>> + Unpin + 'docker {
        let mut path = vec!["/events".to_owned()];
        if let Some(query) = opts.serialize() {
            path.push(query);
        }
        let reader = Box::pin(
            self.stream_get(path.join("?"))
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e)),
        )
        .into_async_read();

        let codec = futures_codec::LinesCodec {};

        Box::pin(
            futures_codec::FramedRead::new(reader, codec)
                .map_err(Error::IO)
                .and_then(|s: String| async move {
                    serde_json::from_str(&s).map_err(Error::SerdeJsonError)
                }),
        )
    }

    //
    // Utility functions to make requests
    //

    pub(crate) async fn get(
        &self,
        endpoint: &str,
    ) -> Result<String> {
        self.transport
            .request(Method::GET, endpoint, Payload::None, Headers::None)
            .await
    }

    pub(crate) async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
    ) -> Result<T> {
        let raw_string = self
            .transport
            .request(Method::GET, endpoint, Payload::None, Headers::None)
            .await?;

        Ok(serde_json::from_str::<T>(&raw_string)?)
    }

    pub(crate) async fn post(
        &self,
        endpoint: &str,
        body: Option<(Body, Mime)>,
    ) -> Result<String> {
        self.transport
            .request(Method::POST, endpoint, body, Headers::None)
            .await
    }

    pub(crate) async fn put(
        &self,
        endpoint: &str,
        body: Option<(Body, Mime)>,
    ) -> Result<String> {
        self.transport
            .request(Method::PUT, endpoint, body, Headers::None)
            .await
    }

    pub(crate) async fn post_json<T, B>(
        &self,
        endpoint: impl AsRef<str>,
        body: Option<(B, Mime)>,
    ) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
        B: Into<Body>,
    {
        let string = self
            .transport
            .request(Method::POST, endpoint, body, Headers::None)
            .await?;

        Ok(serde_json::from_str::<T>(&string)?)
    }

    pub(crate) async fn post_json_headers<'a, T, B, H>(
        &self,
        endpoint: impl AsRef<str>,
        body: Option<(B, Mime)>,
        headers: Option<H>,
    ) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
        B: Into<Body>,
        H: IntoIterator<Item = (&'static str, String)> + 'a,
    {
        let string = self
            .transport
            .request(Method::POST, endpoint, body, headers)
            .await?;

        Ok(serde_json::from_str::<T>(&string)?)
    }

    pub(crate) async fn delete(
        &self,
        endpoint: &str,
    ) -> Result<String> {
        self.transport
            .request(Method::DELETE, endpoint, Payload::None, Headers::None)
            .await
    }

    pub(crate) async fn delete_json<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
    ) -> Result<T> {
        let string = self
            .transport
            .request(Method::DELETE, endpoint, Payload::None, Headers::None)
            .await?;

        Ok(serde_json::from_str::<T>(&string)?)
    }

    /// Send a streaming post request.
    ///
    /// Use stream_post_into_values if the endpoint returns JSON values
    pub(crate) fn stream_post<'a, H>(
        &'a self,
        endpoint: impl AsRef<str> + 'a,
        body: Option<(Body, Mime)>,
        headers: Option<H>,
    ) -> impl Stream<Item = Result<hyper::body::Bytes>> + 'a
    where
        H: IntoIterator<Item = (&'static str, String)> + 'a,
    {
        self.transport
            .stream_chunks(Method::POST, endpoint, body, headers)
    }

    /// Send a streaming post request that returns a stream of JSON values
    ///
    /// Assumes that each received chunk contains one or more JSON values
    pub(crate) fn stream_post_into<'a, H, T>(
        &'a self,
        endpoint: impl AsRef<str> + 'a,
        body: Option<(Body, Mime)>,
        headers: Option<H>,
    ) -> impl Stream<Item = Result<T>> + 'a
    where
        H: IntoIterator<Item = (&'static str, String)> + 'a,
        T: de::DeserializeOwned,
    {
        self.stream_post(endpoint, body, headers)
            .and_then(|chunk| async move {
                let stream = futures_util::stream::iter(
                    serde_json::Deserializer::from_slice(&chunk)
                        .into_iter()
                        .collect::<Vec<_>>(),
                )
                .map_err(Error::from);

                Ok(stream)
            })
            .try_flatten()
    }

    pub(crate) fn stream_get<'a>(
        &'a self,
        endpoint: impl AsRef<str> + Unpin + 'a,
    ) -> impl Stream<Item = Result<hyper::body::Bytes>> + 'a {
        let headers = Some(Vec::default());
        self.transport
            .stream_chunks(Method::GET, endpoint, Option::<(Body, Mime)>::None, headers)
    }

    pub(crate) async fn stream_post_upgrade<'a>(
        &'a self,
        endpoint: impl AsRef<str> + 'a,
        body: Option<(Body, Mime)>,
    ) -> Result<impl futures_util::io::AsyncRead + futures_util::io::AsyncWrite + 'a> {
        self.transport
            .stream_upgrade(Method::POST, endpoint, body)
            .await
    }
}

impl Default for Docker {
    fn default() -> Self {
        Self::new()
    }
}

/// Options for filtering streams of Docker events
#[derive(Default, Debug)]
pub struct EventsOptions {
    params: HashMap<&'static str, String>,
}

impl EventsOptions {
    pub fn builder() -> EventsOptionsBuilder {
        EventsOptionsBuilder::default()
    }

    /// serialize options as a string. returns None if no options are defined
    pub fn serialize(&self) -> Option<String> {
        if self.params.is_empty() {
            None
        } else {
            Some(
                form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(&self.params)
                    .finish(),
            )
        }
    }
}

#[derive(Copy, Clone)]
pub enum EventFilterType {
    Container,
    Image,
    Volume,
    Network,
    Daemon,
}

fn event_filter_type_to_string(filter: EventFilterType) -> &'static str {
    match filter {
        EventFilterType::Container => "container",
        EventFilterType::Image => "image",
        EventFilterType::Volume => "volume",
        EventFilterType::Network => "network",
        EventFilterType::Daemon => "daemon",
    }
}

/// Filter options for image listings
pub enum EventFilter {
    Container(String),
    Event(String),
    Image(String),
    Label(String),
    Type(EventFilterType),
    Volume(String),
    Network(String),
    Daemon(String),
}

/// Builder interface for `EventOptions`
#[derive(Default)]
pub struct EventsOptionsBuilder {
    params: HashMap<&'static str, String>,
    events: Vec<String>,
    containers: Vec<String>,
    images: Vec<String>,
    labels: Vec<String>,
    volumes: Vec<String>,
    networks: Vec<String>,
    daemons: Vec<String>,
    types: Vec<String>,
}

impl EventsOptionsBuilder {
    /// Filter events since a given timestamp
    pub fn since(
        &mut self,
        ts: &u64,
    ) -> &mut Self {
        self.params.insert("since", ts.to_string());
        self
    }

    /// Filter events until a given timestamp
    pub fn until(
        &mut self,
        ts: &u64,
    ) -> &mut Self {
        self.params.insert("until", ts.to_string());
        self
    }

    pub fn filter(
        &mut self,
        filters: Vec<EventFilter>,
    ) -> &mut Self {
        let mut params = HashMap::new();
        for f in filters {
            match f {
                EventFilter::Container(n) => {
                    self.containers.push(n);
                    params.insert("container", self.containers.clone())
                }
                EventFilter::Event(n) => {
                    self.events.push(n);
                    params.insert("event", self.events.clone())
                }
                EventFilter::Image(n) => {
                    self.images.push(n);
                    params.insert("image", self.images.clone())
                }
                EventFilter::Label(n) => {
                    self.labels.push(n);
                    params.insert("label", self.labels.clone())
                }
                EventFilter::Volume(n) => {
                    self.volumes.push(n);
                    params.insert("volume", self.volumes.clone())
                }
                EventFilter::Network(n) => {
                    self.networks.push(n);
                    params.insert("network", self.networks.clone())
                }
                EventFilter::Daemon(n) => {
                    self.daemons.push(n);
                    params.insert("daemon", self.daemons.clone())
                }
                EventFilter::Type(n) => {
                    let event_type = event_filter_type_to_string(n).to_string();
                    self.types.push(event_type);
                    params.insert("type", self.types.clone())
                }
            };
        }
        self.params
            .insert("filters", serde_json::to_string(&params).unwrap());
        self
    }

    pub fn build(&self) -> EventsOptions {
        EventsOptions {
            params: self.params.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Version {
    pub version: String,
    pub api_version: String,
    pub git_commit: String,
    pub go_version: String,
    pub os: String,
    pub arch: String,
    pub kernel_version: String,
    #[cfg(feature = "chrono")]
    pub build_time: DateTime<Utc>,
    #[cfg(not(feature = "chrono"))]
    pub build_time: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Info {
    pub containers: u64,
    pub images: u64,
    pub driver: String,
    pub docker_root_dir: String,
    pub driver_status: Vec<Vec<String>>,
    #[serde(rename = "ID")]
    pub id: String,
    pub kernel_version: String,
    // pub Labels: Option<???>,
    pub mem_total: u64,
    pub memory_limit: bool,
    #[serde(rename = "NCPU")]
    pub n_cpu: u64,
    pub n_events_listener: u64,
    pub n_goroutines: u64,
    pub name: String,
    pub operating_system: String,
    // pub RegistryConfig:???
    pub swap_limit: bool,
    pub system_time: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    #[serde(rename = "Type")]
    pub typ: String,
    #[serde(rename = "Action")]
    pub action: String,
    #[serde(rename = "Actor")]
    pub actor: Actor,
    pub status: Option<String>,
    pub id: Option<String>,
    pub from: Option<String>,
    #[cfg(feature = "chrono")]
    #[serde(deserialize_with = "datetime_from_unix_timestamp")]
    pub time: DateTime<Utc>,
    #[cfg(not(feature = "chrono"))]
    pub time: u64,
    #[cfg(feature = "chrono")]
    #[serde(deserialize_with = "datetime_from_nano_timestamp", rename = "timeNano")]
    pub time_nano: DateTime<Utc>,
    #[cfg(not(feature = "chrono"))]
    #[serde(rename = "timeNano")]
    pub time_nano: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Actor {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Attributes")]
    pub attributes: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "unix-socket")]
    #[test]
    fn unix_host_env() {
        use super::Docker;
        use std::env;
        env::set_var("DOCKER_HOST", "unix:///docker.sock");
        let d = Docker::new();
        match d.transport {
            crate::transport::Transport::Unix { path, .. } => {
                assert_eq!(path, "/docker.sock");
            }
            _ => {
                panic!("Expected transport to be unix.");
            }
        }
        env::set_var("DOCKER_HOST", "http://localhost:8000");
        let d = Docker::new();
        match d.transport {
            crate::transport::Transport::Tcp { host, .. } => {
                assert_eq!(host, "http://localhost:8000");
            }
            _ => {
                panic!("Expected transport to be http.");
            }
        }
    }
}

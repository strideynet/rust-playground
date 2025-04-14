use axum::http::StatusCode;
use axum::Router;
use rustls::pki_types::{CertificateDer, IpAddr, PrivateKeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task;
use tokio_rustls::server::TlsStream;
use x509_parser::prelude::*;
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing_subscriber::fmt::format::FmtSpan;
use color_eyre::eyre::{eyre, Context, Report, Result};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    // construct a subscriber that prints formatted traces to stdout
    let subscriber = tracing_subscriber::fmt()
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .finish();
    // use that subscriber to process traces emitted after this point
    tracing::subscriber::set_global_default(subscriber)?;

    tracing::info!("Starting!");

    let mut client = spiffe::WorkloadApiClient::default().await.wrap_err("Opening SPIFFE Workload API")?;
    let ctx = client.fetch_x509_context().await?;
    let svid = ctx.default_svid().ok_or(Report::msg("no default SVID"))?;

    // Convert SPIFFE SVID to rustls expected DER vec of certificates
    let cert_chain = svid
        .cert_chain()
        .iter()
        .map(|cert| CertificateDer::from(cert.content().to_vec()))
        .collect::<Vec<_>>();

    let mut root_store = rustls::RootCertStore::empty();
    let trust_bundle= ctx.bundle_set()
        .get_bundle(svid.spiffe_id().trust_domain())
        .ok_or(Report::msg("no bundle for trust domain"))?
        .authorities();
    for authority in trust_bundle {
        root_store.add(authority.content().into())?;
    }
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(root_store)).build()?;

    let private_key = PrivateKeyDer::try_from(svid.private_key().content().to_vec()).map_err(Report::msg)?;
    let config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(
            cert_chain,
            private_key,
        )?;
    let config_arc = Arc::new(config);
    let acceptor = TlsAcceptor::from(config_arc);

    let connection_counter = prometheus::Counter::with_opts(
        prometheus::Opts::new(
            "connections_accepted", 
            "Number of connections accepted",
        )
    )?;
    prometheus::register(Box::new(connection_counter.clone()))?;
    let connection_gauge = prometheus::Gauge::with_opts(
        prometheus::Opts::new(
            "connections_active", 
            "Number of active connections",
        )
    )?;
    prometheus::register(Box::new(connection_gauge.clone()))?;

    let http_task: task::JoinHandle<Result<()>> = tokio::spawn(async {
        let router = Router::new()
        .route("/metrics", axum::routing::get(metrics_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:3884").await?;
        tracing::info!(
            addr = listener.local_addr()?.to_string(), 
            "Listening HTTP TLS connections",
        );

        axum::serve(listener, router).await?;
        Ok(())
    });

    let proxy_task: task::JoinHandle<Result<()>> = tokio::spawn(async move {
        let listener = TcpListener::bind("127.0.0.1:3883").await?;
        let local_addr = listener.local_addr()?;
        tracing::info!(
            addr = local_addr.to_string(),
            "Listening for TLS connections"
        );

        loop {
            let (stream, peer_addr) = listener.accept().await?;
            connection_counter.inc();            

            let connection_gauge = connection_gauge.clone();
            let acceptor = acceptor.clone();

            tracing::info!(peer_addr = peer_addr.to_string(), "New connection!");
            tokio::spawn(async move {
                connection_gauge.inc();
                if let Err(e) = handle_connection(acceptor, stream, peer_addr).await {
                    tracing::error!(error = %e, "Error handling connection");
                }
                connection_gauge.dec();
            });
        }
    });

    // Wait for either task to exit
    tokio::select! {
        result = http_task => {
            match result {
                Err(e) => tracing::error!(error = %e, "HTTP task panicked"),
                Ok(Ok(_)) => tracing::info!("HTTP task exited without error"),
                Ok(Err(e)) => tracing::error!(error = %e, "HTTP task exited with error"),
            }
        }
        result = proxy_task => {
            match result {
                Err(e) => tracing::error!(error = %e, "Proxy task panicked"),
                Ok(Ok(_)) => tracing::info!("Proxy task exited without error"),
                Ok(Err(e)) => tracing::error!(error = %e, "Proxy task exited with error"),
            }
        }
    };

    Ok(())
}

async fn metrics_handler() -> Result<String, StatusCode> {
    let encoder = prometheus::TextEncoder::new();
    let encoded = encoder.
        encode_to_string(&prometheus::gather())
        .or(Err(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(encoded)
}

#[tracing::instrument(skip(parsed_cert))]
fn extract_uri_san(
    parsed_cert: &X509Certificate
) -> Result<String> {
    let sans = parsed_cert
    .subject_alternative_name()?
    .ok_or(eyre!("No SAN"))?;

    for san in sans.value.general_names.iter() {
        if let x509_parser::extensions::GeneralName::URI(uri) = san {
            return Ok(uri.to_string());
        }
    }
    Err(eyre!("No URI SAN found"))
}

#[tracing::instrument]
fn authenticate_client(
    tls_stream: &TlsStream<TcpStream>
) -> Result<String> {
    let (_, conn_info) = tls_stream.get_ref();
    let peer_certs = conn_info.peer_certificates().ok_or(eyre!("No peer certificates"))?;
    let leaf_cert = peer_certs.first().ok_or(eyre!("No leaf certificate"))?;

    let (_, parsed_leaf) = x509_parser::prelude::X509Certificate::from_der(leaf_cert)?;
    let uri_san = extract_uri_san(&parsed_leaf)?;
    Ok(uri_san)
}

async fn handle_connection(
    acceptor: TlsAcceptor,
    downstream: TcpStream,
    peer_addr: std::net::SocketAddr,
) -> Result<()> {
    let mut downstream = acceptor.accept(downstream).await?;
    
    let uri_san = authenticate_client(&downstream)?;
    tracing::info!(peer_addr = peer_addr.to_string(), uri_san = uri_san, "Client authenticated");

    //let upstream_connector: &dyn UpstreamConnector = &TCPUpstreamConnector{
     //   addr: "localhost:3884".to_string(),
    //};
    let upstream_connector: &dyn UpstreamConnector = &TLSUpstreamConnector{
        addr: "google.com:443".to_string(),
    };
    let mut upstream = upstream_connector.connect().await?;

    tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await?;
    Ok(())
}

trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncStream for T {}

/// A trait for connecting to an upstream service.
#[async_trait::async_trait]
trait UpstreamConnector {
    async fn connect(&self) -> Result<Box<dyn AsyncStream>>;
}

/// An upstream connector that connects using plain TCP to a given address.
struct TCPUpstreamConnector {
    addr: String,
}

#[async_trait::async_trait]
impl UpstreamConnector for TCPUpstreamConnector {
    async fn connect(&self) -> Result<Box<dyn AsyncStream>> {
        let stream = TcpStream::connect(self.addr.clone()).await?;
        Ok(Box::new(stream))
    }
}


struct TLSUpstreamConnector {
    addr: String,
}

#[async_trait::async_trait]
impl UpstreamConnector for TLSUpstreamConnector {
    async fn connect(&self) -> Result<Box<dyn AsyncStream>> {
        let mut root_cert_store: RootCertStore = RootCertStore::empty();
        root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut config = ClientConfig::builder()
            .with_root_certificates(root_cert_store)
            .with_no_client_auth();

        // TODO: Make ALPN configurable.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];

        let server_name = ServerName::try_from("google.com")?;
        
        let connector = TlsConnector::from(Arc::new(config));

        let stream = TcpStream::connect(self.addr.clone()).await?;
        tracing::info!("Connected TCP to upstream server");
        let stream = connector.connect(
            server_name, 
            stream,
        ).await?;
        tracing::info!("Connected TLS to upstream server");

        Ok(Box::new(stream))
    }
}
use axum::http::StatusCode;
use axum::Router;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use tokio::task;
use tokio_rustls::server::TlsStream;
use x509_parser::prelude::*;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
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
        ))?;
    prometheus::register(Box::new(connection_counter.clone()))?;

    let listener = TcpListener::bind("127.0.0.1:3883").await?;
    let local_addr = listener.local_addr()?;
    tracing::info!(addr = local_addr.to_string(), "Listening");

    let http_task: task::JoinHandle<Result<()>> = tokio::spawn(async {
        let http_router = Router::new()
        .route("/metrics", axum::routing::get(metrics_handler));
        let http_listener = tokio::net::TcpListener::bind("127.0.0.1:3884").await?;

        axum::serve(http_listener, http_router).await?;
        Ok(())
    });

    let proxy_task: task::JoinHandle<Result<()>> = tokio::spawn(async move {
        loop {
            let (stream, peer_addr) = listener.accept().await?;
            connection_counter.inc();
            let acceptor = acceptor.clone();
            tracing::info!(peer_addr = peer_addr.to_string(), "New connection!");
            tokio::spawn(async move {
                if let Err(e) = handle_connection(acceptor, stream, peer_addr).await {
                    tracing::error!(error = %e, "Error handling connection");
                }
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
    stream: TcpStream,
    peer_addr: std::net::SocketAddr,
) -> Result<()> {
    let mut stream = acceptor.accept(stream).await?;
    
    let uri_san = authenticate_client(&stream)?;

    let message = format!(
        "Hello, world! Your SPIFFE ID is {} and your addr is {}",
        uri_san,
        peer_addr,
    );
    stream.write_all(message.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn metrics_handler() -> Result<String, StatusCode> {
    let encoder = prometheus::TextEncoder::new();
    let encoded = encoder.
        encode_to_string(&prometheus::gather())
        .or(Err(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(encoded)
}
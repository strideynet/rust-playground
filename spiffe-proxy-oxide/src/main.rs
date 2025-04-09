use rustls::pki_types::{CertificateDer, Der, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use x509_parser::prelude::*;
use std::io::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, sink};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing_subscriber::fmt::format::FmtSpan;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // construct a subscriber that prints formatted traces to stdout
    let subscriber = tracing_subscriber::fmt()
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .finish();
    // use that subscriber to process traces emitted after this point
    tracing::subscriber::set_global_default(subscriber)?;

    tracing::info!("Starting!");

    let mut client = spiffe::WorkloadApiClient::default().await?;
    let ctx = client.fetch_x509_context().await?;
    let svid = ctx.default_svid().ok_or("no default SVID")?;

    // Convert SPIFFE SVID to rustls expected DER vec of certificates
    let cert_chain = svid
        .cert_chain()
        .iter()
        .map(|cert| CertificateDer::from(cert.content().to_vec()))
        .collect::<Vec<_>>();

    let mut root_store = rustls::RootCertStore::empty();
    let trust_bundle= ctx.bundle_set()
        .get_bundle(svid.spiffe_id().trust_domain())
        .ok_or("no bundle for trust domain")?
        .authorities();
    for authority in trust_bundle {
        root_store.add(authority.content().into())?;
    }
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(root_store)).build()?;

    let config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(
            cert_chain,
            PrivateKeyDer::try_from(svid.private_key().content().to_vec())?,
        )?;
    let config_arc = Arc::new(config);
    let acceptor = TlsAcceptor::from(config_arc);

    let listener = TcpListener::bind("127.0.0.1:3883").await?;
    let local_addr = listener.local_addr()?;
    tracing::info!(addr = local_addr.to_string(), "Listening");

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let acceptor = acceptor.clone();
        tracing::info!(peer_addr = peer_addr.to_string(), "New connection!");
        tokio::spawn(async move {
            if let Err(e) = handle_connection(acceptor, stream, peer_addr).await {
                tracing::error!(error = %e, "Error handling connection");
            }
        });
    }
}

fn extract_uri_san(
    parsed_cert: &X509Certificate
) -> Result<String, Box<dyn std::error::Error>> {
    let sans = parsed_cert
    .subject_alternative_name()?
    .ok_or("No SANs")?;

    for san in sans.value.general_names.iter() {
        if let x509_parser::extensions::GeneralName::URI(uri) = san {
            return Ok(uri.to_string());
        }
    }
    Err("No URI SAN".into())
}

async fn handle_connection(
    acceptor: TlsAcceptor,
    stream: TcpStream,
    peer_addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = acceptor.accept(stream).await?;
    
    let (_, conn_info) = stream.get_ref();
    let peer_certs = conn_info.peer_certificates().ok_or("No peer certificates")?;
    let leaf_cert = peer_certs.first().ok_or("No leaf certificate")?;

    let (_, parsed_leaf) = x509_parser::prelude::X509Certificate::from_der(leaf_cert)?;
    let uri_san = extract_uri_san(&parsed_leaf)?;

    let message = format!(
        "Hello, world! Peer certificate: {} - your SPIFFE ID is {}",
        parsed_leaf.subject,
        uri_san
    );
    stream.write_all(message.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

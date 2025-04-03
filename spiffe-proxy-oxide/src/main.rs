use std::io::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use rustls::pki_types::{CertificateDer, Der, PrivateKeyDer};
use rustls::sign::{CertifiedKey, SingleCertAndKey};
use tokio::io::{sink, AsyncWriteExt};
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
    let ctx = Arc::new(client.fetch_x509_context().await?);
    let svid = ctx.default_svid().ok_or("no default SVID")?;

    // Convert SPIFFE SVID to rustls expected DER vec of certificates
    let cert_chain = svid.cert_chain()
        .iter()
        .map(|cert| CertificateDer::from_slice(cert.content()))
        .collect::<Vec<_>>();

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            cert_chain,
            PrivateKeyDer::try_from(svid.private_key().content())?
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

async fn handle_connection(
    acceptor: TlsAcceptor,
    stream: TcpStream,
    peer_addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = acceptor.accept(stream).await.unwrap();
    let mut output = sink();
    stream.write_all(b"Hello, world!").await.unwrap();
    stream.shutdown().await.unwrap();
    Ok(())
}
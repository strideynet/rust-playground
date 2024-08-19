use std::net::SocketAddr;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use tracing_subscriber::fmt::format::FmtSpan;
use futures::{future, SinkExt, StreamExt, TryStreamExt};
use tokio::sync::{broadcast};
use tokio::sync::broadcast::Sender;
use tokio::time;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // construct a subscriber that prints formatted traces to stdout
    let subscriber = tracing_subscriber::fmt()
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .finish();
    // use that subscriber to process traces emitted after this point
    tracing::subscriber::set_global_default(subscriber)?;

    tracing::info!("Starting!");

    let listener = TcpListener::bind("127.0.0.1:3883").await?;
    let local_addr = listener.local_addr()?;
    tracing::info!(addr = local_addr.to_string(), "Listening");

    let (tx, _) = broadcast::channel(10);

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        tracing::info!(peer_addr = peer_addr.to_string(), "New connection!");
        let tx = tx.clone();
        tokio::spawn(async move {
            handle_client(stream, peer_addr, tx).await.unwrap();
        });
    }
}

#[derive(Clone,Debug)]
struct Event {
    from: u16,
    message: String,
}

#[tracing::instrument(skip(stream))]
async fn handle_client(
    stream: TcpStream,
    peer_addr: SocketAddr,
    tx: Sender<Event>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ws_stream = accept_async(stream).await?;
    let (mut write_stream, mut read_stream) = ws_stream.split();

    let mut rx = tx.subscribe();

    loop {
        tokio::select! {
            msg = read_stream.next() => {
                if let Some(Ok(msg)) = msg {
                    let text = msg.to_text().unwrap().to_string();
                    tracing::info!(text = msg.to_text().unwrap(), "Received message!");
                    tx.send(Event {
                        from: peer_addr.port(),
                        message: text,
                    }).unwrap();
                }
            }
            msg = rx.recv() => {
                if let Ok(msg) = msg {
                    if msg.from == peer_addr.port() {
                        continue;
                    }
                    write_stream.send(Message::text(msg.message)).await?;
                }
            }
        }
    }

    Ok(())
}

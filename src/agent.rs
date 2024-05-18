// src/client.rs
use tonic::transport::Channel;
use tonic::Request;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;

use tunnel::tunnel_client::TunnelClient;
use tunnel::{Empty, Packet};

pub mod tunnel {
    tonic::include_proto!("tunnel");
}

// Custom packet processing function
fn process_packet(packet: Packet) -> Packet {
    // Example processing (reverse bytes)
    let mut reversed_packet = packet.tcp_packet.clone();
    reversed_packet.reverse();

    Packet {
        client: packet.client,
        tcp_packet: reversed_packet,
    }
}

async fn receive_and_process_packets(mut client: TunnelClient<Channel>) -> Result<(), Box<dyn std::error::Error>> {
    let mut response_stream: Streaming<Packet> = client.receive_packets(Request::new(Empty {})).await?.into_inner();
    let (tx, rx) = mpsc::channel(4);

    tokio::spawn(async move {
        while let Some(packet) = response_stream.message().await.unwrap_or(None) {
            let processed_packet = process_packet(packet);
            if tx.send(processed_packet).await.is_err() {
                break;
            }
        }
    });

    client.send_packets(ReceiverStream::new(rx)).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = TunnelClient::connect("http://127.0.0.1:50051").await?;
    receive_and_process_packets(client).await?;
    Ok(())
}
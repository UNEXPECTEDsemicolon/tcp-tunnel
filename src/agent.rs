use log::{debug, error, info};
use std::collections::HashMap;
use std::env;
use std::hash::Hash;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::Request;

use tunnel::tunnel_client::TunnelClient;
use tunnel::{Client, Empty, Packet};

pub mod tunnel {
    tonic::include_proto!("tunnel");
}

impl Hash for Client {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.client_id.hash(state);
    }
}

impl Eq for Client {
    fn assert_receiver_is_total_eq(&self) {
        self.client_id.assert_receiver_is_total_eq()
    }
}

#[derive(Debug)]
pub struct VirtualClient {
    client_sender: mpsc::Sender<Packet>,
}

impl VirtualClient {
    fn new(agent_sender: Arc<mpsc::Sender<Packet>>, web_url: Arc<String>) -> Self {
        let (tx, mut rx) = mpsc::channel(10);
        let res = Self { client_sender: tx };
        tokio::spawn(async move {
            let mut socket = TcpStream::connect(&*web_url).await.unwrap();
            while let Some(packet) = rx.recv().await {
                socket.write_all(&packet.tcp_packet).await.unwrap();
                let mut buffer = [0; 4096];
                while let Ok(bytes_read) = socket.read(&mut buffer).await {
                    if bytes_read == 0 {
                        // info!("Client {} successfully disconnected", addr);
                        break;
                    }
                    // debug!("Read packet of size {} from client {}", bytes_read, addr);
                    agent_sender
                        .send(Packet {
                            client: packet.client.clone(),
                            tcp_packet: buffer.to_vec(),
                        })
                        .await
                        .unwrap();
                }
            }
        });
        res
    }
}

#[derive(Debug)]
pub struct AgentService {
    agent: TunnelClient<Channel>,
    agent_sender: Arc<mpsc::Sender<Packet>>,
    clients: HashMap<Client, VirtualClient>,
    web_url: Arc<String>,
}

impl AgentService {
    async fn run(
        mut agent: TunnelClient<Channel>,
        web_url: String,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (tx, rx) = mpsc::channel(100);
        agent.send_packets(ReceiverStream::new(rx)).await?;
        Ok(Self {
            agent,
            agent_sender: Arc::new(tx), // TODO: remove rw lock
            clients: Default::default(),
            web_url: Arc::new(web_url),
        })
    }

    async fn receive_packets_from_server(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut packets_stream = self
            .agent
            .receive_packets(Request::new(Empty {}))
            .await?
            .into_inner();

        while let Some(packet) = packets_stream.message().await? {
            self.clients
                .entry(packet.client.as_ref().unwrap().clone())
                .or_insert_with(|| {
                    VirtualClient::new(self.agent_sender.clone(), self.web_url.clone())
                })
                .client_sender
                .send(packet)
                .await?;
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok();
    env_logger::init();
    let server_ip = env::var("SERVER_ADDRESS")?;
    let grpc_port = env::var("SERVER_GRPC_PORT")?;
    let web_url = env::var("AGENT_LOCAL_WEB_URL")?;
    let agent = TunnelClient::connect(format!("http://{}:{}", server_ip, grpc_port)).await?;
    let mut service = AgentService::run(agent, web_url).await?;
    service.receive_packets_from_server().await?;
    Ok(())
}

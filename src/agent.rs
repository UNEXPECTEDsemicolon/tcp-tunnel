use log::{debug, error, info, trace};
use std::collections::HashMap;
use std::env;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{self, TcpStream};
use tokio::sync::mpsc::Receiver;
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;
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

impl Copy for Client {}

#[derive(Debug)]
pub struct VirtualClient {
    client_sender: mpsc::Sender<Packet>,
}

impl VirtualClient {
    fn new(
        client: Client,
        clients: Arc<Mutex<HashMap<Client, VirtualClient>>>,
        agent_sender: Arc<mpsc::Sender<Packet>>,
        web_url: Arc<String>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(10);
        let res = Self { client_sender: tx };
        tokio::spawn(async move {
            Self::process(rx, client, agent_sender, web_url)
                .await
                .unwrap_or_else(|err| {
                    error!("Client processing failed: {}", err);
                });
            clients.lock().await.remove(&client);
        });
        res
    }

    async fn process(
        mut rx: Receiver<Packet>,
        client: Client,
        agent_sender: Arc<mpsc::Sender<Packet>>,
        web_url: Arc<String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        debug!(
            "Creating new connection to {} from client with hash {}",
            &*web_url, client.client_id
        );
        let addr = net::lookup_host(&*web_url)
            .await?
            .next()
            .ok_or_else(|| format!("Failed to resolve host {}", &*web_url))?;
        debug!("Host {} was resolved to {}", &*web_url, addr);
        let mut socket = TcpStream::connect(addr)
            .await
            .inspect_err(|err| error!("Failed to connect to {}: {}", &*web_url, err))?;
        info!(
            "New connection to {} from client with hash {}",
            &*web_url, client.client_id
        );
        while let Some(packet) = rx.recv().await {
            debug!(
                "Unqueued packet of size {} from client with hash {}",
                packet.tcp_packet.len(),
                client.client_id
            );
            socket
                .write_all(&packet.tcp_packet)
                .await
                .inspect_err(|err| {
                    error!(
                        "Failed to send packet of size {} to {} from client with hash {}: {}",
                        packet.tcp_packet.len(),
                        &*web_url,
                        client.client_id,
                        err,
                    );
                })?;
            debug!(
                "Sent packet of size {} from client with hash {} to {}",
                packet.tcp_packet.len(),
                client.client_id,
                &*web_url
            );
            let mut buffer = [0; 1024 * 64];
            while let Ok(bytes_read) = socket.read(&mut buffer).await {
                debug!(
                    "Read packet of size {} for client with hash {} from {}",
                    bytes_read, client.client_id, &*web_url
                );
                if bytes_read == 0 {
                    break;
                }
                agent_sender
                    .send(Packet {
                        client: packet.client,
                        tcp_packet: buffer[..bytes_read].to_vec(),
                    })
                    .await.inspect_err(|err| {
                        error!(
                            "Failed to send packet of size {} for client with hash {} to server: {}",
                            bytes_read,
                            client.client_id,
                            err,
                        );
                    })?;
                debug!(
                    "Sent packet of size {} for client with hash {} to server",
                    bytes_read, client.client_id,
                );
            }
            info!(
                "Connection to {} from client with hash {} was closed",
                &*web_url, client.client_id
            )
        }
        info!(
            "Connection to {} from client with hash {} was closed",
            &*web_url, client.client_id
        );
        Ok(())
    }
}

#[derive(Debug)]
pub struct AgentService {
    agent: TunnelClient<Channel>,
    agent_sender: Arc<mpsc::Sender<Packet>>,
    clients: Arc<Mutex<HashMap<Client, VirtualClient>>>,
    web_url: Arc<String>,
}

impl AgentService {
    async fn run(
        agent: TunnelClient<Channel>,
        web_url: String,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (tx, rx) = mpsc::channel(100);
        debug!("Streaming of packets back to server");
        tokio::spawn({
            let mut agent = agent.clone();
            async move {
                let _ = agent
                    .send_packets(ReceiverStream::new(rx))
                    .await
                    .inspect_err(|err| error!("Failed to stream packets back to server: {}", err));
            }
        });
        Ok(Self {
            agent,
            agent_sender: Arc::new(tx),
            clients: Default::default(),
            web_url: Arc::new(web_url),
        })
    }

    async fn receive_packets_from_server(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        info!("Receiving packets from server");
        let mut packets_stream = self
            .agent
            .receive_packets(Request::new(Empty {}))
            .await?
            .into_inner();

        while let Some(packet) = packets_stream.message().await? {
            let packet_client = packet.client.unwrap();
            debug!(
                "Got packet of size {} from client with hash {}",
                packet.tcp_packet.len(),
                packet_client.client_id,
            );
            self.clients
                .lock()
                .await
                .entry(packet_client)
                .or_insert_with(|| {
                    debug!("Client with hash {} is new", packet_client.client_id);
                    VirtualClient::new(
                        packet_client,
                        self.clients.clone(),
                        self.agent_sender.clone(),
                        self.web_url.clone(),
                    )
                })
                .client_sender
                .send(packet)
                .await?;
            debug!(
                "Packet from client with hash {} was pushed to process queue",
                packet_client.client_id
            );
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
    let grpc_url = format!("http://{}:{}", server_ip, grpc_port);
    let web_url = env::var("AGENT_LOCAL_WEB_URL")?;
    debug!("Connecting to server on {}", &grpc_url);
    let agent;
    loop {
        match TunnelClient::connect(grpc_url.clone()).await {
            Ok(a) => {
                agent = a;
                break;
            }
            Err(err) => {
                trace!("Failed to connect to server: {}", err);
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
    info!("Connected to server on {}", &grpc_url);
    let mut service = AgentService::run(agent, web_url).await?;
    service.receive_packets_from_server().await?;
    Ok(())
}

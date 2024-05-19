use log::{debug, error, info};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::env;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Server, Request, Response, Status, Streaming};

use tunnel::tunnel_server::{Tunnel, TunnelServer};
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

impl Client {
    fn new(addr: SocketAddr) -> Self {
        let mut hasher = DefaultHasher::new();
        addr.hash(&mut hasher);
        Client {
            client_id: hasher.finish(),
        }
    }
}

#[derive(Debug)]
pub struct ClientChannels {
    tcp_writer: Mutex<OwnedWriteHalf>,
}

#[derive(Debug, Default)]
pub struct TunnelService {
    agent_sender: RwLock<Option<mpsc::Sender<Result<Packet, Status>>>>,
    clients: RwLock<HashMap<Client, ClientChannels>>,
}

#[tonic::async_trait]
impl Tunnel for TunnelService {
    type ReceivePacketsStream = ReceiverStream<Result<Packet, Status>>;

    async fn send_packets(
        &self,
        request: Request<Streaming<Packet>>,
    ) -> Result<Response<Empty>, Status> {
        let agent_addr = request.remote_addr().unwrap();
        info!("Agent on {} starts to send packets", agent_addr);
        let mut stream = request.into_inner();
        while let Some(packet) = stream.message().await? {
            let packet_size = packet.tcp_packet.len();
            debug!(
                "Got packet of size {} from agent on {}",
                packet_size, agent_addr
            );
            let clients = self.clients.read().await;
            let client = clients.get(&packet.client.unwrap()).ok_or_else(|| {
                error!(
                    "Client for packet of size {} from agent on {} does not exist or disconnected",
                    packet_size, agent_addr,
                );
                Status::not_found("Client does not exist or disconnected")
            })?;
            client
                .tcp_writer
                .lock()
                .await
                .write_all(&packet.tcp_packet)
                .await
                .map_err(|err| {
                    error!("Failed to write to client: {}", err);
                    Status::internal("Failed to write to client")
                })?;
            debug!(
                "Packet of size {} from agent on {} was sent to client on {}",
                packet_size,
                agent_addr,
                client.tcp_writer.lock().await.local_addr().unwrap()
            );
        }
        info!("Agent finished streaming of packets");
        drop(self.agent_sender.write().await.take());
        Ok(Response::new(Empty {}))
    }

    async fn receive_packets(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<Self::ReceivePacketsStream>, Status> {
        let agent_addr = request.remote_addr().unwrap();
        debug!("Agent on {} is trying to connect", agent_addr);
        let mut sender = self.agent_sender.write().await;
        if sender.is_none() {
            let (tx, rx) = mpsc::channel(100);
            let _ = sender.insert(tx);
            info!("Agent receives packets on {}", agent_addr);
            Ok(Response::new(ReceiverStream::new(rx)))
        } else {
            error!("Another agent on {} failed to connect", agent_addr);
            Err(Status::resource_exhausted("Agent already connected"))
        }
    }
}

impl TunnelService {
    async fn handle_tcp_connection(&self, socket: TcpStream, addr: SocketAddr) {
        let (mut reader, writer) = socket.into_split();
        let client = Client::new(addr.clone());
        match self.clients.write().await.entry(client.clone()) {
            Entry::Vacant(e) => {
                e.insert(ClientChannels {
                    tcp_writer: Mutex::new(writer),
                });
                debug!("Client on {} saved", addr);
            }
            Entry::Occupied(_) => {
                error!("Client on {} already connected", addr);
                return;
            }
        }
        let mut buffer = [0; 1024 * 64];
        info!("Reading packets from client {}", addr);
        while let Ok(bytes_read) = reader.read(&mut buffer).await {
            if bytes_read == 0 {
                info!("Client {} successfully disconnected", addr);
                break;
            }
            debug!("Read packet of size {} from client {}", bytes_read, addr);

            if let Some(ref sender) = *self.agent_sender.read().await {
                if sender
                    .send(Ok(Packet {
                        client: Some(client.clone()),
                        tcp_packet: buffer[..bytes_read].to_vec(),
                    }))
                    .await
                    .is_ok()
                {
                    debug!(
                        "Pushed packet of size {} from client {} to gRPC channel",
                        bytes_read, addr
                    );
                } else {
                    error!("Agent disconnected");
                    break;
                }
            } else {
                error!("Agent is not connected yet");
                break;
            }
        }
        self.clients.write().await.remove(&client);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok();
    env_logger::init();
    let bind_ip = env::var("SERVER_BIND_ADDRESS")?.parse()?;
    let grpc_port = env::var("SERVER_GRPC_PORT")?.parse()?;
    let tcp_port = env::var("SERVER_TCP_PORT")?.parse()?;
    let addr = SocketAddr::new(bind_ip, grpc_port);
    let tunnel_service = Arc::new(TunnelService::default());
    let grpc_server = TunnelServer::from_arc(tunnel_service.clone());

    tokio::spawn(async move {
        info!("gRPC Server listening on {}", addr);
        Server::builder()
            .add_service(grpc_server)
            .serve(addr)
            .await
            .unwrap();
    });

    let tcp_addr = SocketAddr::new(bind_ip, tcp_port);
    let listener = TcpListener::bind(tcp_addr).await?;
    info!("TcpListener accepts incoming connections on {}", tcp_addr);

    while let Ok((socket, addr)) = listener.accept().await {
        info!("New connection from {}", addr);
        let tunnel_service = tunnel_service.clone();
        tokio::spawn(async move {
            tunnel_service.handle_tcp_connection(socket, addr).await;
        });
    }

    Ok(())
}

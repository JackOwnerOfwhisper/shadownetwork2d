use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{SocketAddr, AddrParseError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, RwLock, Mutex};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use bytes::Bytes;
use dashmap::DashMap;
use crossbeam::channel;

pub type ConnectionId = Uuid;
pub type RoomId = String;

#[derive(Debug, Clone)]
pub enum NetworkEvent {
    TravelerArrived { id: ConnectionId, addr: SocketAddr, room: Option<RoomId> },
    TravelerLeft { id: ConnectionId, room: Option<RoomId> },
    MessageReceived { from: ConnectionId, message: Message, transport: TransportType },
    RoomCreated { room_id: RoomId, host: ConnectionId },
    RoomDestroyed { room_id: RoomId },
    TravelerJoinedRoom { traveler_id: ConnectionId, room_id: RoomId },
    TravelerLeftRoom { traveler_id: ConnectionId, room_id: RoomId },
    HostMigrated { old_host: ConnectionId, new_host: ConnectionId, room_id: RoomId },
    RateLimitExceeded { connection: ConnectionId },
    ValidationFailed { connection: ConnectionId, reason: String },
    Error { connection: Option<ConnectionId>, error: NetworkError },
}

#[derive(Debug, Clone, Copy)]
pub enum TransportType {
    Tcp,
    Udp,
    WebSocket,
}

#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub max_connections: usize,
    pub buffer_size: usize,
    pub compression: bool,
    pub encryption: bool,
    pub heartbeat_interval: u64,
    pub rate_limit_per_second: u32,
    pub max_message_size: usize,
    pub enable_metrics: bool,
    pub connection_timeout: u64,
    pub max_rooms: usize,
    pub max_travelers_per_room: usize,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            max_connections: 1000,
            buffer_size: 8192,
            compression: false,
            encryption: false,
            heartbeat_interval: 30000,
            rate_limit_per_second: 60,
            max_message_size: 1024 * 1024,
            enable_metrics: false,
            connection_timeout: 60000,
            max_rooms: 100,
            max_travelers_per_room: 50,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub id: ConnectionId,
    pub addr: SocketAddr,
    pub connected_at: Instant,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub last_activity: Instant,
    pub room_id: Option<RoomId>,
    pub transport: TransportType,
    pub authenticated: bool,
    pub message_count: u32,
    pub last_rate_reset: Instant,
}

#[derive(Debug, Default, Clone)]
pub struct NetworkStats {
    pub active_connections: usize,
    pub total_connections: u64,
    pub messages_sent: u64,
    pub messages_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub errors: u64,
    pub active_rooms: usize,
    pub rate_limit_violations: u64,
    pub validation_failures: u64,
    pub host_migrations: u64,
    pub uptime_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct Room {
    pub id: RoomId,
    pub host: ConnectionId,
    pub travelers: Vec<ConnectionId>,
    pub created_at: Instant,
    pub max_travelers: usize,
    pub metadata: HashMap<String, String>,
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum NetworkError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),
    #[error("Serialization error: {0}")]
    Serialization(String),
    #[error("IO error: {0}")]
    Io(String),
    #[error("Timeout")]
    Timeout,
    #[error("Disconnected")]
    Disconnected,
    #[error("Room full")]
    RoomFull,
    #[error("Room not found: {0}")]
    RoomNotFound(String),
    #[error("Authentication failed")]
    AuthenticationFailed,
    #[error("Rate limit exceeded")]
    RateLimitExceeded,
    #[error("Message too large")]
    MessageTooLarge,
    #[error("Invalid message format")]
    InvalidMessage,
    #[error("Permission denied")]
    PermissionDenied,
    #[error("Address parse error: {0}")]
    AddressParse(#[from] AddrParseError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageType {
    Data,
    Heartbeat,
    Connect,
    Disconnect,
    JoinRoom,
    LeaveRoom,
    CreateRoom,
    DestroyRoom,
    Authentication,
    HostMigration,
    Ping,
    Pong,
    Custom(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub msg_type: MessageType,
    pub data: Bytes,
    pub timestamp: u64,
    pub priority: MessagePriority,
    pub reliable: bool,
    pub compressed: bool,
    pub encrypted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessagePriority {
    Low,
    Normal,
    High,
    Critical,
}

impl Message {
    pub fn new(msg_type: MessageType, data: Bytes) -> Self {
        Self {
            id: Uuid::new_v4(),
            msg_type,
            data,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            priority: MessagePriority::Normal,
            reliable: true,
            compressed: false,
            encrypted: false,
        }
    }

    pub fn with_priority(mut self, priority: MessagePriority) -> Self {
        self.priority = priority;
        self
    }

    pub fn unreliable(mut self) -> Self {
        self.reliable = false;
        self
    }

    pub fn compressed(mut self) -> Self {
        self.compressed = true;
        self
    }

    pub fn encrypted(mut self) -> Self {
        self.encrypted = true;
        self
    }

    pub fn serialize(&self) -> Result<Vec<u8>, NetworkError> {
        let mut data = bincode::serialize(self)
            .map_err(|e| NetworkError::Serialization(e.to_string()))?;

        if self.compressed {
            data = Self::compress(&data)?;
        }

        if self.encrypted {
            data = Self::encrypt(&data)?;
        }

        Ok(data)
    }

    pub fn deserialize(data: &[u8]) -> Result<Self, NetworkError> {
        let mut data = data.to_vec();

        let message: Self = bincode::deserialize(&data)
            .map_err(|e| NetworkError::Serialization(e.to_string()))?;

        if message.encrypted {
            data = Self::decrypt(&data)?;
        }

        if message.compressed {
            data = Self::decompress(&data)?;
        }

        bincode::deserialize(&data)
            .map_err(|e| NetworkError::Serialization(e.to_string()))
    }

    fn compress(data: &[u8]) -> Result<Vec<u8>, NetworkError> {
        #[cfg(feature = "compression")]
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use std::io::Write;

            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(data).map_err(|e| NetworkError::Io(e.to_string()))?;
            encoder.finish().map_err(|e| NetworkError::Io(e.to_string()))
        }
        #[cfg(not(feature = "compression"))]
        Ok(data.to_vec())
    }

    fn decompress(data: &[u8]) -> Result<Vec<u8>, NetworkError> {
        #[cfg(feature = "compression")]
        {
            use flate2::read::GzDecoder;
            use std::io::Read;

            let mut decoder = GzDecoder::new(data);
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed)
                .map_err(|e| NetworkError::Io(e.to_string()))?;
            Ok(decompressed)
        }
        #[cfg(not(feature = "compression"))]
        Ok(data.to_vec())
    }

    fn encrypt(data: &[u8]) -> Result<Vec<u8>, NetworkError> {
        #[cfg(feature = "encryption")]
        {
            Ok(data.to_vec())
        }
        #[cfg(not(feature = "encryption"))]
        Ok(data.to_vec())
    }

    fn decrypt(data: &[u8]) -> Result<Vec<u8>, NetworkError> {
        #[cfg(feature = "encryption")]
        {
            Ok(data.to_vec())
        }
        #[cfg(not(feature = "encryption"))]
        Ok(data.to_vec())
    }
}

#[derive(Clone)]
pub struct RateLimiter {
    requests: DashMap<ConnectionId, (u32, Instant)>,
    limit: u32,
}

impl RateLimiter {
    pub fn new(limit: u32) -> Self {
        Self {
            requests: DashMap::new(),
            limit,
        }
    }

    pub fn check(&self, connection_id: ConnectionId) -> bool {
        let now = Instant::now();
        let mut entry = self.requests.entry(connection_id).or_insert((0, now));

        if now.duration_since(entry.1) >= Duration::from_secs(1) {
            entry.0 = 1;
            entry.1 = now;
            true
        } else if entry.0 < self.limit {
            entry.0 += 1;
            true
        } else {
            false
        }
    }
}

#[derive(Clone)]
pub struct MessageValidator {
    max_size: usize,
}

impl MessageValidator {
    pub fn new(max_size: usize) -> Self {
        Self { max_size }
    }

    pub fn validate(&self, message: &Message) -> Result<(), NetworkError> {
        if message.data.len() > self.max_size {
            return Err(NetworkError::MessageTooLarge);
        }

        match &message.msg_type {
            MessageType::Data | MessageType::Custom(_) => {
                if message.data.is_empty() {
                    return Err(NetworkError::InvalidMessage);
                }
            }
            _ => {}
        }

        Ok(())
    }
}

pub struct Host {
    config: NetworkConfig,
    connections: Arc<RwLock<HashMap<ConnectionId, ConnectionInfo>>>,
    rooms: Arc<RwLock<HashMap<RoomId, Room>>>,
    stats: Arc<RwLock<NetworkStats>>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
    event_rx: Option<mpsc::UnboundedReceiver<NetworkEvent>>,
    rate_limiter: RateLimiter,
    validator: MessageValidator,
    start_time: Instant,
    tcp_streams: Arc<DashMap<ConnectionId, Arc<Mutex<TcpStream>>>>,
    udp_socket: Option<Arc<UdpSocket>>,
}

impl Host {
    pub fn new(config: NetworkConfig) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let rate_limiter = RateLimiter::new(config.rate_limit_per_second);
        let validator = MessageValidator::new(config.max_message_size);

        Self {
            config,
            connections: Arc::new(RwLock::new(HashMap::new())),
            rooms: Arc::new(RwLock::new(HashMap::new())),
            stats: Arc::new(RwLock::new(NetworkStats::default())),
            event_tx,
            event_rx: Some(event_rx),
            rate_limiter,
            validator,
            start_time: Instant::now(),
            tcp_streams: Arc::new(DashMap::new()),
            udp_socket: None,
        }
    }

    pub async fn start_traveling_tcp(&mut self, addr: &str) -> Result<(), NetworkError> {
        let socket_addr: SocketAddr = addr.parse()?;

        let listener = TcpListener::bind(socket_addr).await
            .map_err(|e| NetworkError::ConnectionFailed(e.to_string()))?;

        info!("Host started TCP traveling on {}", socket_addr);

        let connections = self.connections.clone();
        let tcp_streams = self.tcp_streams.clone();
        let event_tx = self.event_tx.clone();
        let rate_limiter = Arc::new(self.rate_limiter.clone());
        let validator = Arc::new(self.validator.clone());

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        let connection_id = Uuid::new_v4();
                        let connection_info = ConnectionInfo {
                            id: connection_id,
                            addr: peer_addr,
                            connected_at: Instant::now(),
                            bytes_sent: 0,
                            bytes_received: 0,
                            last_activity: Instant::now(),
                            room_id: None,
                            transport: TransportType::Tcp,
                            authenticated: false,
                            message_count: 0,
                            last_rate_reset: Instant::now(),
                        };

                        connections.write().await.insert(connection_id, connection_info);
                        tcp_streams.insert(connection_id, Arc::new(Mutex::new(stream)));

                        let _ = event_tx.send(NetworkEvent::TravelerArrived {
                            id: connection_id,
                            addr: peer_addr,
                            room: None,
                        });

                        let connections_clone = connections.clone();
                        let tcp_streams_clone = tcp_streams.clone();
                        let event_tx_clone = event_tx.clone();
                        let rate_limiter_clone = rate_limiter.clone();
                        let validator_clone = validator.clone();

                        tokio::spawn(async move {
                            Self::handle_tcp_traveler(
                                connection_id,
                                connections_clone,
                                tcp_streams_clone,
                                event_tx_clone,
                                rate_limiter_clone,
                                validator_clone,
                            ).await;
                        });
                    }
                    Err(e) => {
                        error!("Failed to accept TCP traveler: {}", e);
                    }
                }
            }
        });

        Ok(())
    }

    pub async fn start_traveling_udp(&mut self, addr: &str) -> Result<(), NetworkError> {
        let socket_addr: SocketAddr = addr.parse()?;

        let socket = UdpSocket::bind(socket_addr).await
            .map_err(|e| NetworkError::ConnectionFailed(e.to_string()))?;

        info!("Host started UDP traveling on {}", socket_addr);

        let socket = Arc::new(socket);
        self.udp_socket = Some(socket.clone());

        let event_tx = self.event_tx.clone();
        let rate_limiter = Arc::new(self.rate_limiter.clone());
        let validator = Arc::new(self.validator.clone());

        tokio::spawn(async move {
            let mut buffer = vec![0; 65536];

            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((n, peer_addr)) => {
                        if let Ok(message) = Message::deserialize(&buffer[..n]) {
                            let connection_id = Uuid::new_v4();

                            if rate_limiter.check(connection_id) {
                                if validator.validate(&message).is_ok() {
                                    let _ = event_tx.send(NetworkEvent::MessageReceived {
                                        from: connection_id,
                                        message,
                                        transport: TransportType::Udp,
                                    });
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("UDP receive error: {}", e);
                    }
                }
            }
        });

        Ok(())
    }

    async fn handle_tcp_traveler(
        connection_id: ConnectionId,
        connections: Arc<RwLock<HashMap<ConnectionId, ConnectionInfo>>>,
        tcp_streams: Arc<DashMap<ConnectionId, Arc<Mutex<TcpStream>>>>,
        event_tx: mpsc::UnboundedSender<NetworkEvent>,
        rate_limiter: Arc<RateLimiter>,
        validator: Arc<MessageValidator>,
    ) {
        let stream = match tcp_streams.get(&connection_id) {
            Some(stream) => stream.clone(),
            None => return,
        };

        let mut buffer = vec![0; 8192];

        loop {
            let read_result = {
                let mut stream_guard = stream.lock().await;
                stream_guard.read(&mut buffer).await
            };

            match read_result {
                Ok(0) => break,
                Ok(n) => {
                    if rate_limiter.check(connection_id) {
                        if let Ok(message) = Message::deserialize(&buffer[..n]) {
                            if validator.validate(&message).is_ok() {
                                let _ = event_tx.send(NetworkEvent::MessageReceived {
                                    from: connection_id,
                                    message,
                                    transport: TransportType::Tcp,
                                });

                                if let Some(mut info) = connections.write().await.get_mut(&connection_id) {
                                    info.bytes_received += n as u64;
                                    info.last_activity = Instant::now();
                                    info.message_count += 1;
                                }
                            } else {
                                let _ = event_tx.send(NetworkEvent::ValidationFailed {
                                    connection: connection_id,
                                    reason: "Message validation failed".to_string(),
                                });
                            }
                        }
                    } else {
                        let _ = event_tx.send(NetworkEvent::RateLimitExceeded {
                            connection: connection_id,
                        });
                    }
                }
                Err(_) => break,
            }
        }

        tcp_streams.remove(&connection_id);
        connections.write().await.remove(&connection_id);
        let _ = event_tx.send(NetworkEvent::TravelerLeft {
            id: connection_id,
            room: None,
        });
    }

    pub async fn next_event(&mut self) -> Option<NetworkEvent> {
        if let Some(rx) = &mut self.event_rx {
            rx.recv().await
        } else {
            None
        }
    }

    pub async fn send_to_traveler(&self, traveler_id: ConnectionId, message: Message) -> Result<(), NetworkError> {
        if let Some(stream_arc) = self.tcp_streams.get(&traveler_id) {
            let data = message.serialize()?;
            let mut stream = stream_arc.lock().await;
            stream.write_all(&data).await
                .map_err(|e| NetworkError::Io(e.to_string()))?;

            if let Some(mut info) = self.connections.write().await.get_mut(&traveler_id) {
                info.bytes_sent += data.len() as u64;
                info.last_activity = Instant::now();
            }

            Ok(())
        } else {
            Err(NetworkError::Disconnected)
        }
    }

    pub async fn broadcast_to_all(&self, message: Message) -> Result<(), NetworkError> {
        let connections = self.connections.read().await;
        for connection_id in connections.keys() {
            let _ = self.send_to_traveler(*connection_id, message.clone()).await;
        }
        Ok(())
    }

    pub async fn broadcast_to_room(&self, room_id: &RoomId, message: Message) -> Result<(), NetworkError> {
        let rooms = self.rooms.read().await;
        if let Some(room) = rooms.get(room_id) {
            for traveler_id in &room.travelers {
                let _ = self.send_to_traveler(*traveler_id, message.clone()).await;
            }
            let _ = self.send_to_traveler(room.host, message).await;
        }
        Ok(())
    }

    pub async fn create_room(&self, room_id: RoomId, host_id: ConnectionId, max_travelers: usize) -> Result<(), NetworkError> {
        let mut rooms = self.rooms.write().await;
        if rooms.len() >= self.config.max_rooms {
            return Err(NetworkError::RoomFull);
        }

        if rooms.contains_key(&room_id) {
            return Err(NetworkError::RoomNotFound("Room already exists".to_string()));
        }

        let room = Room {
            id: room_id.clone(),
            host: host_id,
            travelers: Vec::new(),
            created_at: Instant::now(),
            max_travelers: max_travelers.min(self.config.max_travelers_per_room),
            metadata: HashMap::new(),
        };

        rooms.insert(room_id.clone(), room);

        if let Some(mut connection) = self.connections.write().await.get_mut(&host_id) {
            connection.room_id = Some(room_id.clone());
        }

        let _ = self.event_tx.send(NetworkEvent::RoomCreated {
            room_id,
            host: host_id,
        });

        Ok(())
    }

    pub async fn join_room(&self, room_id: &RoomId, traveler_id: ConnectionId) -> Result<(), NetworkError> {
        let mut rooms = self.rooms.write().await;
        let room = rooms.get_mut(room_id).ok_or_else(|| NetworkError::RoomNotFound(room_id.clone()))?;

        if room.travelers.len() >= room.max_travelers {
            return Err(NetworkError::RoomFull);
        }

        if !room.travelers.contains(&traveler_id) {
            room.travelers.push(traveler_id);
        }

        if let Some(mut connection) = self.connections.write().await.get_mut(&traveler_id) {
            connection.room_id = Some(room_id.clone());
        }

        let _ = self.event_tx.send(NetworkEvent::TravelerJoinedRoom {
            traveler_id,
            room_id: room_id.clone(),
        });

        Ok(())
    }

    pub async fn leave_room(&self, room_id: &RoomId, traveler_id: ConnectionId) -> Result<(), NetworkError> {
        let mut rooms = self.rooms.write().await;
        let room = rooms.get_mut(room_id).ok_or_else(|| NetworkError::RoomNotFound(room_id.clone()))?;

        room.travelers.retain(|&id| id != traveler_id);

        if let Some(mut connection) = self.connections.write().await.get_mut(&traveler_id) {
            connection.room_id = None;
        }

        let _ = self.event_tx.send(NetworkEvent::TravelerLeftRoom {
            traveler_id,
            room_id: room_id.clone(),
        });

        if room.host == traveler_id && !room.travelers.is_empty() {
            let new_host = room.travelers[0];
            room.host = new_host;

            let _ = self.event_tx.send(NetworkEvent::HostMigrated {
                old_host: traveler_id,
                new_host,
                room_id: room_id.clone(),
            });
        }

        if room.travelers.is_empty() && room.host == traveler_id {
            rooms.remove(room_id);
            let _ = self.event_tx.send(NetworkEvent::RoomDestroyed {
                room_id: room_id.clone(),
            });
        }

        Ok(())
    }

    pub async fn stats(&self) -> NetworkStats {
        let mut stats = self.stats.read().await.clone();
        stats.active_connections = self.connections.read().await.len();
        stats.active_rooms = self.rooms.read().await.len();
        stats.uptime_seconds = self.start_time.elapsed().as_secs();
        stats
    }

    pub async fn rooms(&self) -> Vec<Room> {
        self.rooms.read().await.values().cloned().collect()
    }

    pub async fn connections(&self) -> Vec<ConnectionInfo> {
        self.connections.read().await.values().cloned().collect()
    }
}

pub struct Traveler {
    connection_id: Option<ConnectionId>,
    tcp_stream: Option<TcpStream>,
    udp_socket: Option<UdpSocket>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
    event_rx: Option<mpsc::UnboundedReceiver<NetworkEvent>>,
    host_addr: Option<SocketAddr>,
    current_room: Option<RoomId>,
    authenticated: bool,
}

impl Traveler {
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        Self {
            connection_id: None,
            tcp_stream: None,
            udp_socket: None,
            event_tx,
            event_rx: Some(event_rx),
            host_addr: None,
            current_room: None,
            authenticated: false,
        }
    }

    pub async fn travel_to_tcp(&mut self, addr: &str) -> Result<(), NetworkError> {
        let socket_addr: SocketAddr = addr.parse()?;

        let stream = TcpStream::connect(socket_addr).await
            .map_err(|e| NetworkError::ConnectionFailed(e.to_string()))?;

        self.connection_id = Some(Uuid::new_v4());
        self.tcp_stream = Some(stream);
        self.host_addr = Some(socket_addr);

        info!("Successfully traveled to TCP host at {}", socket_addr);


        self.start_tcp_receiver().await;

        Ok(())
    }

    pub async fn travel_to_udp(&mut self, addr: &str) -> Result<(), NetworkError> {
        let socket_addr: SocketAddr = addr.parse()?;

        let socket = UdpSocket::bind("0.0.0.0:0").await
            .map_err(|e| NetworkError::ConnectionFailed(e.to_string()))?;

        socket.connect(socket_addr).await
            .map_err(|e| NetworkError::ConnectionFailed(e.to_string()))?;

        self.connection_id = Some(Uuid::new_v4());
        self.udp_socket = Some(socket);
        self.host_addr = Some(socket_addr);

        info!("Successfully traveled to UDP host at {}", socket_addr);

        Ok(())
    }

    async fn start_tcp_receiver(&mut self) {
        if let Some(stream) = self.tcp_stream.take() {
            let event_tx = self.event_tx.clone();
            let connection_id = self.connection_id.unwrap();

            tokio::spawn(async move {
                let mut stream = stream;
                let mut buffer = vec![0; 8192];

                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(message) = Message::deserialize(&buffer[..n]) {
                                let _ = event_tx.send(NetworkEvent::MessageReceived {
                                    from: connection_id,
                                    message,
                                    transport: TransportType::Tcp,
                                });
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    }

    pub async fn send_tcp(&mut self, message: Message) -> Result<(), NetworkError> {
        if let Some(stream) = &mut self.tcp_stream {
            let data = message.serialize()?;
            stream.write_all(&data).await
                .map_err(|e| NetworkError::Io(e.to_string()))?;
            Ok(())
        } else {
            Err(NetworkError::Disconnected)
        }
    }

    pub async fn send_udp(&self, message: Message) -> Result<(), NetworkError> {
        if let Some(socket) = &self.udp_socket {
            let data = message.serialize()?;
            socket.send(&data).await
                .map_err(|e| NetworkError::Io(e.to_string()))?;
            Ok(())
        } else {
            Err(NetworkError::Disconnected)
        }
    }

    pub async fn join_room(&mut self, room_id: RoomId) -> Result<(), NetworkError> {
        let message = Message::new(
            MessageType::JoinRoom,
            Bytes::from(room_id.clone()),
        );

        self.send_tcp(message).await?;
        self.current_room = Some(room_id);
        Ok(())
    }

    pub async fn leave_room(&mut self) -> Result<(), NetworkError> {
        if let Some(room_id) = &self.current_room {
            let message = Message::new(
                MessageType::LeaveRoom,
                Bytes::from(room_id.clone()),
            );

            self.send_tcp(message).await?;
            self.current_room = None;
        }
        Ok(())
    }

    pub async fn create_room(&mut self, room_id: RoomId, max_travelers: usize) -> Result<(), NetworkError> {
        let room_data = format!("{}:{}", room_id, max_travelers);
        let message = Message::new(
            MessageType::CreateRoom,
            Bytes::from(room_data),
        );

        self.send_tcp(message).await?;
        self.current_room = Some(room_id);
        Ok(())
    }

    pub async fn authenticate(&mut self, auth_token: String) -> Result<(), NetworkError> {
        let message = Message::new(
            MessageType::Authentication,
            Bytes::from(auth_token),
        );

        self.send_tcp(message).await?;
        Ok(())
    }

    pub async fn ping(&mut self) -> Result<(), NetworkError> {
        let message = Message::new(
            MessageType::Ping,
            Bytes::new(),
        );

        self.send_tcp(message).await
    }

    pub async fn receive(&mut self) -> Option<NetworkEvent> {
        if let Some(rx) = &mut self.event_rx {
            rx.recv().await
        } else {
            None
        }
    }

    pub async fn disconnect(&mut self) -> Result<(), NetworkError> {
        if self.current_room.is_some() {
            self.leave_room().await?;
        }

        let disconnect_msg = Message::new(MessageType::Disconnect, Bytes::new());
        let _ = self.send_tcp(disconnect_msg).await;

        self.tcp_stream = None;
        self.udp_socket = None;
        self.connection_id = None;
        self.host_addr = None;
        self.current_room = None;
        self.authenticated = false;

        Ok(())
    }

    pub fn connection_id(&self) -> Option<ConnectionId> {
        self.connection_id
    }

    pub fn current_room(&self) -> Option<&RoomId> {
        self.current_room.as_ref()
    }

    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    pub fn host_address(&self) -> Option<SocketAddr> {
        self.host_addr
    }
}

pub struct NetworkManager {
    config: NetworkConfig,
    connections: Arc<RwLock<HashMap<ConnectionId, ConnectionInfo>>>,
    rooms: Arc<RwLock<HashMap<RoomId, Room>>>,
    stats: Arc<RwLock<NetworkStats>>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
    event_rx: Option<mpsc::UnboundedReceiver<NetworkEvent>>,
    start_time: Instant,
}

impl NetworkManager {
    pub fn new(config: NetworkConfig) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        Self {
            config,
            connections: Arc::new(RwLock::new(HashMap::new())),
            rooms: Arc::new(RwLock::new(HashMap::new())),
            stats: Arc::new(RwLock::new(NetworkStats::default())),
            event_tx,
            event_rx: Some(event_rx),
            start_time: Instant::now(),
        }
    }

    pub fn create_host(&self) -> Host {
        Host::new(self.config.clone())
    }

    pub fn create_traveler(&self) -> Traveler {
        Traveler::new()
    }

    pub fn config(&self) -> &NetworkConfig {
        &self.config
    }

    pub async fn stats(&self) -> NetworkStats {
        let mut stats = self.stats.read().await.clone();
        stats.uptime_seconds = self.start_time.elapsed().as_secs();
        stats
    }

    pub async fn global_connections(&self) -> Vec<ConnectionInfo> {
        self.connections.read().await.values().cloned().collect()
    }

    pub async fn global_rooms(&self) -> Vec<Room> {
        self.rooms.read().await.values().cloned().collect()
    }

    pub fn take_event_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<NetworkEvent>> {
        self.event_rx.take()
    }

    pub fn event_sender(&self) -> mpsc::UnboundedSender<NetworkEvent> {
        self.event_tx.clone()
    }
}

pub struct AntiCheat {
    suspicious_connections: DashMap<ConnectionId, u32>,
    message_patterns: DashMap<ConnectionId, Vec<(MessageType, Instant)>>,
}

impl AntiCheat {
    pub fn new() -> Self {
        Self {
            suspicious_connections: DashMap::new(),
            message_patterns: DashMap::new(),
        }
    }

    pub fn analyze_message(&self, connection_id: ConnectionId, message: &Message) -> bool {
        // Check for suspicious message patterns
        let now = Instant::now();
        let mut patterns = self.message_patterns.entry(connection_id)
            .or_insert_with(Vec::new);

        patterns.push((message.msg_type.clone(), now));

        patterns.retain(|&(_, timestamp)| now.duration_since(timestamp) < Duration::from_secs(10));

        if patterns.len() > 100 {
            self.flag_suspicious(connection_id);
            return false;
        }

        let recent_identical = patterns.iter()
            .filter(|(msg_type, timestamp)| {
                std::mem::discriminant(msg_type) == std::mem::discriminant(&message.msg_type) &&
                    now.duration_since(*timestamp) < Duration::from_secs(1)
            })
            .count();

        if recent_identical > 10 {
            self.flag_suspicious(connection_id);
            return false;
        }

        true
    }

    fn flag_suspicious(&self, connection_id: ConnectionId) {
        let mut entry = self.suspicious_connections.entry(connection_id).or_insert(0);
        *entry += 1;
    }

    pub fn is_suspicious(&self, connection_id: ConnectionId) -> bool {
        self.suspicious_connections.get(&connection_id)
            .map(|count| *count > 5)
            .unwrap_or(false)
    }

    pub fn clear_flags(&self, connection_id: ConnectionId) {
        self.suspicious_connections.remove(&connection_id);
        self.message_patterns.remove(&connection_id);
    }
}

pub struct LagCompensation {
    client_states: DashMap<ConnectionId, Vec<(Instant, Bytes)>>,
    max_history: usize,
}

impl LagCompensation {
    pub fn new(max_history: usize) -> Self {
        Self {
            client_states: DashMap::new(),
            max_history,
        }
    }

    pub fn record_state(&self, connection_id: ConnectionId, state: Bytes) {
        let now = Instant::now();
        let mut states = self.client_states.entry(connection_id)
            .or_insert_with(Vec::new);

        states.push((now, state));

        if states.len() > self.max_history {
            states.remove(0);
        }

        states.retain(|(timestamp, _)| now.duration_since(*timestamp) < Duration::from_secs(1));
    }

    pub fn get_state_at(&self, connection_id: ConnectionId, timestamp: Instant) -> Option<Bytes> {
        let states = self.client_states.get(&connection_id)?;

        states.iter()
            .min_by_key(|(state_time, _)| {
                if timestamp > *state_time {
                    timestamp.duration_since(*state_time)
                } else {
                    state_time.duration_since(timestamp)
                }
            })
            .map(|(_, state)| state.clone())
    }

    pub fn predict_state(&self, connection_id: ConnectionId, future_time: Duration) -> Option<Bytes> {
        let states = self.client_states.get(&connection_id)?;
        if states.len() < 2 {
            return None;
        }

        states.last().map(|(_, state)| state.clone())
    }
}

pub struct NATTraversal {
    stun_servers: Vec<String>,
    port_mappings: DashMap<u16, SocketAddr>,
}

impl NATTraversal {
    pub fn new() -> Self {
        Self {
            stun_servers: vec![
                "stun.l.google.com:19302".to_string(),
                "stun1.l.google.com:19302".to_string(),
                "stun2.l.google.com:19302".to_string(),
            ],
            port_mappings: DashMap::new(),
        }
    }

    pub async fn discover_external_address(&self) -> Result<SocketAddr, NetworkError> {
        "0.0.0.0:0".parse()
            .map_err(NetworkError::from)
    }

    pub async fn create_port_mapping(&self, internal_port: u16, external_port: u16) -> Result<(), NetworkError> {
        let internal_addr: SocketAddr = format!("127.0.0.1:{}", internal_port).parse()?;

        self.port_mappings.insert(external_port, internal_addr);
        Ok(())
    }

    pub async fn hole_punch(&self, local_addr: SocketAddr, remote_addr: SocketAddr) -> Result<UdpSocket, NetworkError> {
        let socket = UdpSocket::bind(local_addr).await
            .map_err(|e| NetworkError::ConnectionFailed(e.to_string()))?;

        for _ in 0..5 {
            let _ = socket.send_to(b"PUNCH", remote_addr).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Ok(socket)
    }
}

// Benchmarking utilities
#[cfg(feature = "metrics")]
pub mod metrics {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub struct NetworkMetrics {
        pub messages_per_second: AtomicU64,
        pub bytes_per_second: AtomicU64,
        pub connection_count: AtomicU64,
        pub latency_samples: Arc<RwLock<Vec<Duration>>>,
    }

    impl NetworkMetrics {
        pub fn new() -> Self {
            Self {
                messages_per_second: AtomicU64::new(0),
                bytes_per_second: AtomicU64::new(0),
                connection_count: AtomicU64::new(0),
                latency_samples: Arc::new(RwLock::new(Vec::new())),
            }
        }

        pub fn record_message(&self, size: u64) {
            self.messages_per_second.fetch_add(1, Ordering::Relaxed);
            self.bytes_per_second.fetch_add(size, Ordering::Relaxed);
        }

        pub fn record_connection(&self) {
            self.connection_count.fetch_add(1, Ordering::Relaxed);
        }

        pub fn record_disconnection(&self) {
            self.connection_count.fetch_sub(1, Ordering::Relaxed);
        }

        pub async fn record_latency(&self, latency: Duration) {
            let mut samples = self.latency_samples.write().await;
            samples.push(latency);

            if samples.len() > 1000 {
                samples.remove(0);
            }
        }

        pub async fn average_latency(&self) -> Option<Duration> {
            let samples = self.latency_samples.read().await;
            if samples.is_empty() {
                return None;
            }

            let total: Duration = samples.iter().sum();
            Some(total / samples.len() as u32)
        }

        pub fn reset_counters(&self) {
            self.messages_per_second.store(0, Ordering::Relaxed);
            self.bytes_per_second.store(0, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_host_traveler_connection() {
        let config = NetworkConfig::default();
        let mut host = Host::new(config.clone());
        let mut traveler = Traveler::new();

        tokio::spawn(async move {
            let _ = host.start_traveling_tcp("127.0.0.1:8080").await;
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let result = traveler.travel_to_tcp("127.0.0.1:8080").await;
        assert!(result.is_ok());

        let message = Message::new(MessageType::Data, Bytes::from("Hello World"));
        let send_result = traveler.send_tcp(message).await;
        assert!(send_result.is_ok());
    }

    #[tokio::test]
    async fn test_room_management() {
        let config = NetworkConfig::default();
        let host = Host::new(config);
        let traveler_id = Uuid::new_v4();

        let result = host.create_room("test_room".to_string(), traveler_id, 10).await;
        assert!(result.is_ok());

        let result = host.join_room(&"test_room".to_string(), Uuid::new_v4()).await;
        assert!(result.is_ok());

        let rooms = host.rooms().await;
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].id, "test_room");

    }

    #[test]
    fn test_rate_limiter() {
        let limiter = RateLimiter::new(5);
        let connection_id = Uuid::new_v4();

        for _ in 0..5 {
            assert!(limiter.check(connection_id));
        }

        assert!(!limiter.check(connection_id));
    }

    #[test]
    fn test_message_validation() {
        let validator = MessageValidator::new(1024);

        let valid_msg = Message::new(MessageType::Data, Bytes::from("test"));
        assert!(validator.validate(&valid_msg).is_ok());

        let large_data = vec![0u8; 2048];
        let invalid_msg = Message::new(MessageType::Data, Bytes::from(large_data));
        assert!(validator.validate(&invalid_msg).is_err());
    }
}
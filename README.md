# ShadowNetwork2D

A high-performance, async networking library for 2D games built in Rust. Features TCP/UDP support, room-based multiplayer, anti-cheat detection, and comprehensive connection management.

## Features

- **Dual Transport**: Both TCP and UDP support for different networking needs
- **Room System**: Built-in room management with host migration
- **Anti-Cheat**: Message pattern analysis and suspicious behavior detection
- **Rate Limiting**: Configurable per-connection rate limiting
- **Lag Compensation**: State history and prediction capabilities
- **NAT Traversal**: STUN server support for peer-to-peer connections
- **Compression**: Optional message compression (feature gated)
- **Encryption**: Hooks for message encryption (feature gated)
- **Metrics**: Built-in performance monitoring and statistics

## Quick Start

Add to your `Cargo.toml`:
```

## Room-Based Multiplayer

```rust
use shadownetwork2d::*;

#[tokio::main]
async fn main() -> Result<(), NetworkError> {
    let mut traveler = Traveler::new();
    traveler.travel_to_tcp("127.0.0.1:8080").await?;
    
    traveler.create_room("game_lobby".to_string(), 10).await?;
    
    println!("Created room: game_lobby");
    Ok(())
}
```

## Configuration

```rust
use shadownetwork2d::*;

let config = NetworkConfig {
    max_connections: 500,
    buffer_size: 16384,
    compression: true,
    encryption: false,
    heartbeat_interval: 30000,
    rate_limit_per_second: 100,
    max_message_size: 2048,
    enable_metrics: true,
    connection_timeout: 120000,
    max_rooms: 50,
    max_travelers_per_room: 20,
};

let host = Host::new(config);
```

## Message Types

- `Data` - General game data
- `Heartbeat` - Keep-alive packets
- `Connect/Disconnect` - Connection management
- `JoinRoom/LeaveRoom` - Room operations
- `CreateRoom/DestroyRoom` - Room management
- `Authentication` - User authentication
- `HostMigration` - Automatic host switching
- `Ping/Pong` - Latency measurement
- `Custom(String)` - Application-specific messages

## Features

### Optional Features
```toml
[dependencies]
shadownetwork2d = { version = "0.1.0", features = ["compression", "encryption", "metrics"] }
```

- `compression` - Enable message compression
- `encryption` - Enable message encryption hooks
- `metrics` - Enable detailed performance metrics

## License

MIT

## Author

[dependencies]
shadownetwork2d = "0.1.0"
tokio = { version = "1.0", features = ["full"] }
bytes = "1.0"
```


[dependencies]
shadownetwork2d = "0.1.0"
tokio = { version = "1.0", features = ["full"] }
bytes = "1.0"
```

### Basic Server (Host)

```rust
use shadownetwork2d::*;
use tokio;

#[tokio::main]
async fn main() -> Result<(), NetworkError> {
    let message = Message::new(MessageType::Data, Bytes::from("Hello Server"));
    traveler.send_tcp(message).await?;
    
    while let Some(event) = traveler.receive().await {
        match event {
            NetworkEvent::MessageReceived { message, .. } => {
                println!("Received: {:?}", message.msg_type);
                break;
            }
            _ => {}
        }
    }
    
    Ok(())
} config = NetworkConfig::default();
    let mut host = Host::new(config);
    
    host.start_traveling_tcp("127.0.0.1:8080").await?;
    
    while let Some(event) = host.next_event().await {
        match event {
            NetworkEvent::TravelerArrived { id, addr, .. } => {
                println!("New traveler connected: {} from {}", id, addr);
            }
            NetworkEvent::MessageReceived { from, message, .. } => {
                println!("Message from {}: {:?}", from, message.msg_type);
                host.send_to_traveler(from, message).await?;
            }
            NetworkEvent::TravelerLeft { id, .. } => {
                println!("Traveler disconnected: {}", id);
            }
            _ => {}
        }
    }
    
    Ok(())
}
```



```rust
use shadownetwork2d::*;
use bytes::Bytes;

#[tokio::main]
async fn main() -> Result<(), NetworkError> {
    let mut traveler = Traveler::new();
    
    traveler.travel_to_tcp("127.0.0.1:8080").await?;
    
    let
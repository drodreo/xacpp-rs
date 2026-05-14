# xacpp

[English](./README.md)

Agent Control Plane Protocol — Rust 实现。

xacpp 定义了 Agent 与对端之间的通信协议。它提供了分层架构，支持基于请求-响应的消息传递、会话管理，以及多种传输后端。

## 架构

```
┌──────────────────────────────────────────────────────────────┐
│  Peer（协议层）                                                │
│  类型化操作 + 会话路由                                          │
├──────────────────────────────────────────────────────────────┤
│  Session（会话层）                                             │
│  独立会话上下文，直达 Transport 收发                              │
├──────────────────────────────────────────────────────────────┤
│  Transport（传输层）                                           │
│  信封装拆、id 关联、pending 匹配                                 │
├──────────────────────────────────────────────────────────────┤
│  Stdio / TCP / WebSocket                                     │
└──────────────────────────────────────────────────────────────┘
```

## 使用

在 `Cargo.toml` 中添加：

```toml
[dependencies]
xacpp = "0.1"
```

### 建立会话（发起方）

```rust
use std::sync::Arc;
use xacpp::commands::XacppCommand;
use xacpp::events::XacppEvent;
use xacpp::handler::{EstablishHandler, XacppSessionHandler};
use xacpp::message::{XacppRequest, XacppResponse};
use xacpp::peer::XacppPeer;
use xacpp::transport::stdio::StdioTransport;
use xacpp::transport::XacppTransport;

// 创建 Transport + Peer
let transport: Arc<dyn XacppTransport> = Arc::new(StdioTransport::new(/* ... */));

struct MyEstablishHandler;
#[async_trait::async_trait]
impl EstablishHandler for MyEstablishHandler {
    async fn on_establish(
        &self,
        transport: Arc<dyn XacppTransport>,
        credentials: Option<String>,
    ) -> Result<(String, Arc<dyn XacppSessionHandler>), xacpp::error::XacppError> {
        Ok(("session-1".into(), Arc::new(MySessionHandler)))
    }
}

let peer = XacppPeer::new(transport, Arc::new(MyEstablishHandler));
peer.connect().await?;

// 建立逻辑会话
let session = peer.establish(None, Arc::new(my_session_handler)).await?;

// 通过会话发送命令/事件
let response = session.request_command(XacppCommand::NewActivity).await?;
session.request_event(XacppEvent::Think { content: "Hello!".into() }).await?;
```

### 处理入站请求（响应方）

```rust
struct MySessionHandler;

#[async_trait::async_trait]
impl XacppSessionHandler for MySessionHandler {
    async fn on_command(&self, command: XacppCommand) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::Acknowledge)
    }

    async fn on_event(&self, event: XacppEvent) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::Acknowledge)
    }
}
```

### TCP 传输（网络通信）

```rust
use xacpp::transport::socket::SocketTransport;

// 客户端
let client = SocketTransport::connect_to("127.0.0.1:8080".into());

// 服务端（使用已 accept 的 TcpStream）
let server = SocketTransport::new(accepted_stream);
```

## API

### 核心类型

| 类型 | 说明 |
|------|------|
| `XacppTransport` | 传输层 trait（`connect`、`disconnect`、`send`、`on_request`） |
| `XacppPeer` | 协议端点，含会话路由 |
| `XacppSession` | 逻辑会话，直达 Transport 收发 |
| `XacppSessionHandler` | 处理会话内的入站 Command/Event |
| `EstablishHandler` | 处理 Establish 握手请求 |
| `XacppCommand` | 协议命令（`Establish`、`NewActivity` 等） |
| `XacppEvent` | 协议事件（Think、ActionRequest、Question 等） |
| `XacppRequest` | 请求载荷（Command 或 Event） |
| `XacppResponse` | 响应载荷（Established、Acknowledge、Action 等） |
| `XacppError` | 错误枚举，含机器可读错误码 |
| `PeerState` | Peer 状态枚举（Disconnected、Connected） |

### 传输实现

| 类型 | 说明 |
|------|------|
| `StdioTransport` | 异步 stdin/stdout JSONL 管道 |
| `SocketTransport` | TCP 传输（`TcpStream`），spawn-per-request 并发模型 |

## 线路协议

JSONL（每行一个 JSON 对象），信封结构：

```json
{"type":"request","id":"r1","payload":{"kind":"command","payload":{"establish":{"credentials":null}}}}
{"type":"response","id":"r1","payload":{"kind":"established","sessionId":"s1"}}
```

## 许可证

MIT

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
| `ActivityRef` | 命令/事件信封共用的活动标识引用 |
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
{"type":"request","id":"r2","payload":{"kind":"command","payload":{"generic":{"name":"report_to_user","arguments":{},"activity":{"id":"act-1"}}}}}
{"type":"request","id":"r3","payload":{"kind":"event","payload":{"activity":{"id":"act-1"},"event":{"name":"think","data":{"content":"hi"}}}}}
{"type":"response","id":"r1","payload":{"kind":"established","sessionId":"s1"}}
```

Generic 命令携带可选的 `activity` 字段，形态为结构化 `{id}` 引用；省略时该字段
不会出现在线路格式中。协议层不强制该字段——校验权归命令实现方。事件信封使用
相同的结构化 `{id}` 引用标识其所属活动。

### 命令声明约定

命令声明 schema 可携带 `dispatcher` 字段，取值 `"bridge"` 或 `"tool"`，缺省视为
`"bridge"`。`bridge` 表示事件桥系统路径接入（不进模型工具面）；`tool` 表示注入
接收方模型工具面（模型直调）。声明 schema 为透传 JSON，协议库不强制类型。

## 许可证

MIT

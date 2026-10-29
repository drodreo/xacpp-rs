# xacpp

[English](./README.md)

Agent Control Plane Protocol — Rust 实现。

xacpp 定义了 Agent 与对端之间的通信协议。它提供了分层架构，支持基于请求-响应的消息传递、会话管理，以及多种传输后端。

## 架构

```
┌──────────────────────────────────────────────────────────────┐
│  Peer（生命周期编排，发起方）                                    │
│  negotiate → connect → establish → disconnect                │
├──────────────────────────────────────────────────────────────┤
│  Session（通信门面 + 传输状态）                                  │
│  request_command / send_event / send_binary；                 │
│  持有 TransferSession（ACK/NACK/RESUME/心跳），                 │
│  自行挂载到当前 transport                                       │
├──────────────────────────────────────────────────────────────┤
│  Transport（连接实体）                                         │
│  物理 IO、二进制组帧、单一 FIFO 出口                             │
├──────────────────────────────────────────────────────────────┤
│  Stdio / TCP                                                  │
└──────────────────────────────────────────────────────────────┘
```

会话的生存期长于单条连接：断连时会话立即从失效 transport 摘除传输挂载——死链上仍在途的帧被丢弃，绝不进入会话状态机——传输状态保留在会话中；重连后会话挂载到新 transport，中断的传输经 RESUME 握手恢复（裁决门控重传，见 SPEC §5）。

## 使用

在 `Cargo.toml` 中添加：

```toml
[dependencies]
xacpp = "1.2"
```

### 建立会话（发起方）

```rust
use std::sync::Arc;
use xacpp::capability::Capabilities;
use xacpp::handler::{EstablishHandler, NegotiateHandler, XacppSessionHandler};
use xacpp::peer::XacppPeer;
use xacpp::session::XacppSession;
use xacpp::transport::socket::SocketTransport;
use xacpp::transport::XacppTransport;

// 会话持有自己的传输状态：独立于任何连接创建，随连接的建立与断开
// 自行挂载到传输上。
let session = Arc::new(XacppSession::new(None)); // None = TransferConfig::default()

let transport: Arc<dyn XacppTransport> =
    Arc::new(SocketTransport::connect_to("127.0.0.1:8080".into()));

// Peer 编排生命周期，并在 connect 时自动挂载会话。
let caps = Capabilities {
    commands: vec![/* 本端可处理的 JSON Schemas */],
    produce_events: vec![],
    accept_events: vec![],
    // 声明 fetch 扩展支持（SPEC §8.7）；省略（None）则禁用 fetch。
    fetch_ext: None,
};
let peer = XacppPeer::new(
    caps,
    transport,
    Some(Arc::clone(&session)),
    Arc::new(MyNegotiateHandler),
    Arc::new(MyEstablishHandler),
);
peer.connect().await?;

// 建立逻辑会话（向会话填充身份/可发事件）。
peer.establish(None, Arc::new(my_session_handler), |_| Ok(())).await?;

// 经会话门面发送命令/事件/二进制。
let response = session.request_command(cmd).await?;
session.send_event(event).await?;
let txid = session
    .send_binary(payload, Some(Arc::new(|result| { /* BinarySendResult */ })))
    .await?;
```

### 处理入站连接（响应方）

```rust
// 响应方没有 Peer：accept 连接、创建会话、重连时恢复身份，
// 然后把会话挂载到传输上。
let transport: Arc<dyn XacppTransport> = Arc::new(SocketTransport::new(stream));
let session = Arc::new(
    XacppSession::new(None)
        .with_identity("session-1".into(), credentials),
);
session.wire(Arc::clone(&transport)).await?;
session.on_binary(Arc::new(|txid, payload| { /* 消费二进制 */ }));

transport
    .on_request(Arc::new(move |_sid, payload| {
        // 对入站命令/事件返回 XacppResponse
    }))
    .expect("connect 之前必须注册请求处理器");
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
| `XacppPeer` | 生命周期编排（negotiate/connect/establish/disconnect）；connect 时自动把会话挂载到传输（发起方） |
| `XacppSession` | 通信门面（`request_command`/`send_event`/`send_binary`）；持有传输状态、跨重连存活，`wire`/`unwire` 挂载到传输 |
| `TransferSession` | 传输事务状态机（ACK/NACK/RESUME/心跳/背压）；由 `XacppSession` 持有 |
| `TransferConfig` | 传输调优（心跳间隔、静默超时、ACK 批量、tick） |
| `XacppTransport` | 连接实体：物理 IO、二进制组帧、单一 FIFO 出口 |
| `XacppSessionHandler` | 处理会话内的入站 Command/Event |
| `NegotiateHandler` / `EstablishHandler` | 握手处理器 |
| `XacppCommand` / `XacppEvent` | 协议命令与事件 |
| `ActivityRef` | 命令/事件信封共用的活动标识引用 |
| `XacppResponse` | 响应载荷（Established、Acknowledge、Action 等） |
| `XacppError` | 错误枚举，含机器可读错误码 |
| `PeerState` | Peer 状态枚举（Disconnected、Connected） |

### 传输实现

| 类型 | 说明 |
|------|------|
| `StdioTransport` | 子进程管道，二进制组帧 |
| `SocketTransport` | TCP 传输（`TcpStream`），spawn-per-request 并发模型 |

## 线路协议

组帧采用 XACPP 二进制帧协议（docs/SPEC.md §3/§4）：每个信封作为 `DATA_JSON` 帧的载荷传输。信封结构：

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

## 性能

基线由 `cargo run --release --example bench_protocol` 测得（TCP 回环，release 构建）；通过 `XACPP_BENCH_N` 调节规模、`XACPP_BENCH_ONLY` 选择分组。以下数字是后续优化工作的参照基准。

| 指标 | 基线 2026-10-14 | 复测 2026-10-23 |
|---|---|---|
| 编解码 ~200 B（帧 vs JSONL） | 持平 | 帧 492.9 vs 315.1 MB/s |
| 编解码 ~2 KiB（帧 vs JSONL） | +49% | — |
| 请求-响应 RTT | p50 0.070 ms @ ~13.5k events/s | p50 0.071 ms @ ~13.3k events/s |
| 64 MiB `sendBinary`（1 MiB 工作帧） | 463–638 MB/s | 198–209 MB/s |
| 4 × 16 MiB 并发 | 638–732 MB/s | 447 MB/s |
| 批量传输下心跳 p50 | 与空闲持平（10.2 ms） | 10.2 ms = 空闲 |

## 许可证

MIT

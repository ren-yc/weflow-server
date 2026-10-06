//! SSE 重放缓冲（从 `server` 层搬来）。
//!
//! 为什么在 `sync`：历史由**生产者**在广播前写入（订阅端各自 append 会让 id 随在线
//! 连接数跳号、且零订阅者时事件根本进不了历史 —— 断线重连的客户端就永远收不到那段，
//! 还以为是自己的窗口没问题）。生产者 `AccountSync` 住在 `sync`，类型若留在
//! `server`，`sync` 反向引用 `server` 会成环。`server` 以 `pub use` 保留原路径。

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::sync::Event;

/// One buffered SSE event (WeFlow contract: replay cap 1000, TTL 10 min).
///
/// **存的是原始事件，不是序列化后的载荷。** 载荷形状是**视图**的事：两个面对同一个事件有不同的
/// 形状要求（WeFlow 兼容面发完整消息，ChatLab 面只发元信息）。存序列化结果的话，后加的那个面
/// 重放时会吐出**另一个面的形状** —— 而且这种错在只连新面时看不出来。
pub struct HistoryItem {
    pub id: u64,
    pub at: std::time::Instant,
    pub event: Event,
}

#[derive(Default)]
pub struct HistoryBuf {
    items: std::collections::VecDeque<HistoryItem>,
    last_id: u64,
}

impl HistoryBuf {
    pub const MAX: usize = 1000;
    pub const TTL: std::time::Duration = std::time::Duration::from_secs(600);

    /// Append an event and return its id (monotonic).
    pub fn append(&mut self, event: Event) -> u64 {
        self.last_id += 1;
        self.items.push_back(HistoryItem {
            id: self.last_id,
            at: std::time::Instant::now(),
            event,
        });
        while self.items.len() > Self::MAX {
            self.items.pop_front();
        }
        self.last_id
    }

    /// 清空缓冲，但**不动 id 计数器**。
    ///
    /// 注销时用它：旧账号的事件对下一个账号没有意义，而计数器若一起归零，带着旧
    /// `Last-Event-ID` 重连的客户端会把新事件当成「已经收过」而丢掉 —— 那比跳号更难查。
    /// 跳号是看得见的，丢事件是看不见的。
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Events with id > `since`, still within the TTL window.
    ///
    /// 返回**事件本身** —— 由调用它的那个面决定怎么序列化（见 [`HistoryItem`] 的说明）。
    pub fn replay_since(&self, since: u64) -> Vec<(u64, Event)> {
        let now = std::time::Instant::now();
        self.items
            .iter()
            .filter(|i| i.id > since && now.duration_since(i.at) < Self::TTL)
            .map(|i| (i.id, i.event.clone()))
            .collect()
    }
}

/// 分配好历史 id 的广播载荷。
///
/// 订阅端直接拿 `id` 当 SSE 帧的 `id:`，不再各自编号 —— 编号是总线级的单调序列，
/// 每个订阅端各 append 会让它随在线连接数跳号，还把同一事件塞进缓冲多次。
#[derive(Debug, Clone)]
pub struct Stamped {
    pub id: u64,
    pub event: Event,
}

/// 重放历史 ＋ 广播通道，绑成一个不可拆的发布面。
///
/// 为什么绑在一起：把裸 `Sender` 交给生产者，它就会忘记写历史；忘记的后果（重放窗口
/// 里没有断线期间的事件）要等第一次重连才显形，而且那时已经丢了。`publish` 是唯一
/// 入口：先单点写历史、再广播，零订阅者时 send 的 Err 被吞（tokio broadcast 语义），
/// 而历史**已经写下**。
#[derive(Clone)]
pub struct EventBus {
    history: Arc<Mutex<HistoryBuf>>,
    tx: broadcast::Sender<Stamped>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        EventBus { history: Arc::new(Mutex::new(HistoryBuf::default())), tx }
    }

    /// 单点发布：写入历史并广播带 id 的载荷，返回分配的 id。
    pub fn publish(&self, event: Event) -> u64 {
        let id = self.history.lock().append(event.clone());
        let _ = self.tx.send(Stamped { id, event });
        id
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Stamped> {
        self.tx.subscribe()
    }

    /// 服务层读重放窗口 / 注销清条目用：同一份历史的句柄。
    pub fn history(&self) -> &Arc<Mutex<HistoryBuf>> {
        &self.history
    }
}
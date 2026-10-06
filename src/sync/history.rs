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
        // append 与 send 必须在**同一把历史锁的临界区**内：并发 publish 时若先释放历史锁
        // 再 send，两条事件的投递顺序可能与编号顺序相反 —— 客户端带着后一条事件的
        // Last-Event-ID 断线重连，先前那条「已分配编号却尚未投递」的事件会被重放窗口
        // 永久滤掉（它的编号更小、落在客户端已见水位之前）。tokio 的 send 非阻塞
        // （写进各接收槽即返回），跨它持锁无代价；订阅端不在持历史锁时取 store（无 ABBA）。
        let mut hist = self.history.lock();
        let id = hist.append(event.clone());
        let _ = self.tx.send(Stamped { id, event });
        id
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Stamped> {
        self.tx.subscribe()
    }

    /// 建立一条 SSE 订阅：在**同一把历史锁的临界区内**先订阅广播、再取重放快照。
    ///
    /// 为什么必须是一步：`publish` 也在同一把锁里 append＋send。若「取快照」与「订阅」
    /// 分两步，落在两步之间的那条发布既不在快照（已拍完）也进不了新的接收端（还没订阅）
    /// —— 该连接永久漏收且毫无信号，只有它主动带旧 `Last-Event-ID` 重连才补得回，而它
    /// 不知道自己漏了。焊进一个临界区后按锁的先后只剩两种情形，且都不丢不重：
    /// 发布先拿到锁 ⇒ 该条进了快照、而新的接收端还不存在（收不到重复）；我们先用快照
    /// ⇒ 该条进不了快照、但 send 时接收端已建好（回归：`subscribe_with_replay_delivers_every_event_exactly_once`）。
    pub fn subscribe_with_replay(&self, since: u64) -> (broadcast::Receiver<Stamped>, Vec<(u64, Event)>) {
        let hist = self.history.lock();
        let rx = self.tx.subscribe();
        let replay = hist.replay_since(since);
        (rx, replay)
    }

    /// 服务层读重放窗口 / 注销清条目用：同一份历史的句柄。
    pub fn history(&self) -> &Arc<Mutex<HistoryBuf>> {
        &self.history
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 快照通道与广播通道的并集必须**不丢不重**。
    ///
    /// 订阅与取快照若分成两步，落在两步之间的那条发布既不在快照（已拍完）也进不了
    /// 接收端（还没订阅）—— 该连接永久漏收且毫无信号，只有它主动带旧 Last-Event-ID
    /// 重连才补得回，而它不知道自己漏了。焊进同一把历史锁后，按锁的先后只有两种情形。
    #[test]
    fn subscribe_with_replay_delivers_every_event_exactly_once() {
        let bus = EventBus::new(16);
        // 发布先拿到历史锁 ⇒ 进快照；此刻接收端还不存在，不会重复。
        let before_id = bus.publish(Event::Sync(vec![]));
        let (mut rx, replay) = bus.subscribe_with_replay(0);
        assert!(
            replay.iter().any(|(id, _)| *id == before_id),
            "订阅之前发布的必须进快照",
        );
        // 订阅之后发布 ⇒ 只能从广播收到，快照里没有。
        let after_id = bus.publish(Event::Sync(vec![]));
        assert!(
            !replay.iter().any(|(id, _)| *id == after_id),
            "不得同时出现在两条通道上",
        );
        let stamped = rx.try_recv().expect("订阅之后的发布必须从广播收到");
        assert_eq!(stamped.id, after_id);
    }
}
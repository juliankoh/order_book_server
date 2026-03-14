use crate::{
    HL_NODE,
    listeners::{directory::DirectoryListener, order_book::state::OrderBookState},
    order_book::{
        Coin, Px, Snapshot,
        multi_book::{Snapshots, load_snapshots_from_json},
    },
    prelude::*,
    types::{
        L4Order, OrderDiff,
        inner::{InnerL4Order, InnerLevel},
        node_data::{Batch, EventSource, NodeDataFill, NodeDataOrderDiff, NodeDataOrderStatus},
    },
};
use alloy::primitives::Address;
use fs::File;
use log::{error, info};
use notify::{Event, RecursiveMode, Watcher, recommended_watcher};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet, VecDeque},
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{
        Mutex,
        broadcast::Sender,
        mpsc::{UnboundedSender, unbounded_channel},
    },
    time::{Instant, interval_at, sleep},
};
use utils::{BatchQueue, EventBatch, process_rmp_file, validate_snapshot_consistency};

mod state;
mod utils;

/// Maximum number of price levels to include in L4Book updates.
const L4_BOOK_MAX_LEVELS: usize = 10;

// WARNING - this code assumes no other file system operations are occurring in the watched directories
// if there are scripts running, this may not work as intended
pub(crate) async fn hl_listen(listener: Arc<Mutex<OrderBookListener>>, dir: PathBuf) -> Result<()> {
    let streaming = listener.lock().await.streaming;
    let order_statuses_dir = EventSource::OrderStatuses.event_source_dir_streaming(&dir, streaming).canonicalize()?;
    let fills_dir = EventSource::Fills.event_source_dir_streaming(&dir, streaming).canonicalize()?;
    let order_diffs_dir = EventSource::OrderDiffs.event_source_dir_streaming(&dir, streaming).canonicalize()?;
    info!("Monitoring order status directory: {}", order_statuses_dir.display());
    info!("Monitoring order diffs directory: {}", order_diffs_dir.display());
    info!("Monitoring fills directory: {}", fills_dir.display());

    // monitoring the directory via the notify crate (gives file system events)
    let (fs_event_tx, mut fs_event_rx) = unbounded_channel();
    let mut watcher = recommended_watcher(move |res| {
        let fs_event_tx = fs_event_tx.clone();
        if let Err(err) = fs_event_tx.send(res) {
            error!("Error sending fs event to processor via channel: {err}");
        }
    })?;

    let ignore_spot = {
        let listener = listener.lock().await;
        listener.ignore_spot
    };

    // every so often, we fetch a new snapshot and the snapshot_fetch_task starts running.
    // Result is sent back along this channel (if error, we want to return to top level)
    let (snapshot_fetch_task_tx, mut snapshot_fetch_task_rx) = unbounded_channel::<Result<()>>();

    watcher.watch(&order_statuses_dir, RecursiveMode::Recursive)?;
    watcher.watch(&fills_dir, RecursiveMode::Recursive)?;
    watcher.watch(&order_diffs_dir, RecursiveMode::Recursive)?;
    let start = Instant::now() + Duration::from_secs(5);
    let mut ticker = interval_at(start, Duration::from_secs(10));
    let mut received_fs_event = false;
    loop {
        tokio::select! {
            event = fs_event_rx.recv() =>  match event {
                Some(Ok(event)) => {
                    received_fs_event = true;
                    if event.kind.is_create() || event.kind.is_modify() {
                        let new_path = &event.paths[0];
                        if new_path.is_dir() {
                            watcher.watch(new_path, RecursiveMode::Recursive)?;
                            continue;
                        }
                        if new_path.starts_with(&order_statuses_dir) && new_path.is_file() {
                            listener
                                .lock()
                                .await
                                .process_update(&event, new_path, EventSource::OrderStatuses)
                                .map_err(|err| format!("Order status processing error: {err}"))?;
                        } else if new_path.starts_with(&fills_dir) && new_path.is_file() {
                            listener
                                .lock()
                                .await
                                .process_update(&event, new_path, EventSource::Fills)
                                .map_err(|err| format!("Fill update processing error: {err}"))?;
                        } else if new_path.starts_with(&order_diffs_dir) && new_path.is_file() {
                            listener
                                .lock()
                                .await
                                .process_update(&event, new_path, EventSource::OrderDiffs)
                                .map_err(|err| format!("Book diff processing error: {err}"))?;
                        }
                    }
                }
                Some(Err(err)) => {
                    error!("Watcher error: {err}");
                    return Err(format!("Watcher error: {err}").into());
                }
                None => {
                    error!("Channel closed. Listener exiting");
                    return Err("Channel closed.".into());
                }
            },
            snapshot_fetch_res = snapshot_fetch_task_rx.recv() => {
                match snapshot_fetch_res {
                    None => {
                        return Err("Snapshot fetch task sender dropped".into());
                    }
                    Some(Err(err)) => {
                        return Err(format!("Abci state reading error: {err}").into());
                    }
                    Some(Ok(())) => {}
                }
            }
            _ = ticker.tick() => {
                let listener = listener.clone();
                let snapshot_fetch_task_tx = snapshot_fetch_task_tx.clone();
                fetch_snapshot(dir.clone(), listener, snapshot_fetch_task_tx, ignore_spot);
            }
            () = sleep(Duration::from_secs(5)) => {
                let listener = listener.lock().await;
                if listener.is_ready() && received_fs_event {
                    return Err(format!("Stream has fallen behind ({HL_NODE} failed?)").into());
                }
            }
        }
    }
}

fn fetch_snapshot(
    dir: PathBuf,
    listener: Arc<Mutex<OrderBookListener>>,
    tx: UnboundedSender<Result<()>>,
    ignore_spot: bool,
) {
    let tx = tx.clone();
    tokio::spawn(async move {
        let res = match process_rmp_file(&dir).await {
            Ok(output_fln) => {
                let state = {
                    let mut listener = listener.lock().await;
                    listener.begin_caching();
                    listener.clone_state()
                };
                let snapshot = load_snapshots_from_json::<InnerL4Order, (Address, L4Order)>(&output_fln).await;
                info!("Snapshot fetched");
                // sleep to let some updates build up.
                sleep(Duration::from_secs(1)).await;
                let mut cache = {
                    let mut listener = listener.lock().await;
                    listener.take_cache()
                };
                info!("Cache has {} elements", cache.len());
                match snapshot {
                    Ok((height, expected_snapshot)) => {
                        if let Some(mut state) = state {
                            while state.height() < height {
                                if let Some((order_statuses, order_diffs)) = cache.pop_front() {
                                    state.apply_updates(order_statuses, order_diffs)?;
                                } else {
                                    return Err::<(), Error>("Not enough cached updates".into());
                                }
                            }
                            if state.height() > height {
                                return Err("Fetched snapshot lagging stored state".into());
                            }
                            let stored_snapshot = state.compute_snapshot().snapshot;
                            info!("Validating snapshot");
                            validate_snapshot_consistency(&stored_snapshot, expected_snapshot, ignore_spot)
                        } else {
                            listener.lock().await.init_from_snapshot(expected_snapshot, height);
                            Ok(())
                        }
                    }
                    Err(err) => Err(err),
                }
            }
            Err(err) => Err(err),
        };
        let _unused = tx.send(res);
        Ok(())
    });
}

pub(crate) struct OrderBookListener {
    ignore_spot: bool,
    streaming: bool,
    fill_status_file: Option<File>,
    order_status_file: Option<File>,
    order_diff_file: Option<File>,
    // None if we haven't seen a valid snapshot yet
    order_book_state: Option<OrderBookState>,
    last_fill: Option<u64>,
    fill_pending: String,
    order_status_pending: String,
    order_diff_pending: String,
    order_diff_cache: BatchQueue<NodeDataOrderDiff>,
    order_status_cache: BatchQueue<NodeDataOrderStatus>,
    // Only Some when we want it to collect updates
    fetched_snapshot_cache: Option<VecDeque<(Batch<NodeDataOrderStatus>, Batch<NodeDataOrderDiff>)>>,
    internal_message_tx: Option<Sender<Arc<InternalMessage>>>,
    // Tracks open orders per user: address -> (oid -> order)
    open_orders: HashMap<Address, HashMap<u64, L4Order>>,
}

impl OrderBookListener {
    pub(crate) fn new(
        internal_message_tx: Option<Sender<Arc<InternalMessage>>>,
        ignore_spot: bool,
        streaming: bool,
    ) -> Self {
        Self {
            ignore_spot,
            streaming,
            fill_status_file: None,
            order_status_file: None,
            order_diff_file: None,
            order_book_state: None,
            last_fill: None,
            fill_pending: String::new(),
            order_status_pending: String::new(),
            order_diff_pending: String::new(),
            fetched_snapshot_cache: None,
            internal_message_tx,
            order_diff_cache: BatchQueue::new(),
            order_status_cache: BatchQueue::new(),
            open_orders: HashMap::new(),
        }
    }

    fn clone_state(&self) -> Option<OrderBookState> {
        self.order_book_state.clone()
    }

    pub(crate) const fn is_ready(&self) -> bool {
        self.order_book_state.is_some()
    }

    pub(crate) fn universe(&self) -> HashSet<Coin> {
        self.order_book_state.as_ref().map_or_else(HashSet::new, OrderBookState::compute_universe)
    }

    #[allow(clippy::type_complexity)]
    // pops earliest pair of cached updates that have the same timestamp if possible
    fn pop_cache(&mut self) -> Option<(Batch<NodeDataOrderStatus>, Batch<NodeDataOrderDiff>)> {
        // synchronize to same block
        match (self.order_diff_cache.front(), self.order_status_cache.front()) {
            (Some(diffs), Some(statuses)) => {
                match diffs.block_number().cmp(&statuses.block_number()) {
                    Ordering::Equal => {
                        // In streaming mode, both caches must have a newer block
                        // to guarantee no more events for this block will arrive
                        // (status and diff files are written independently)
                        if self.streaming
                            && (!self.order_diff_cache.is_front_complete()
                                || !self.order_status_cache.is_front_complete())
                        {
                            return None;
                        }
                        self.order_status_cache
                            .pop_front()
                            .and_then(|t| self.order_diff_cache.pop_front().map(|s| (t, s)))
                    }
                    Ordering::Less => {
                        // Diffs block N has no matching statuses (statuses already at block M > N).
                        // Still need diffs to be complete (a newer diff block exists).
                        if self.streaming && !self.order_diff_cache.is_front_complete() {
                            return None;
                        }
                        let diffs = self.order_diff_cache.pop_front().unwrap();
                        let empty_statuses = diffs.empty_with_metadata();
                        Some((empty_statuses, diffs))
                    }
                    Ordering::Greater => {
                        // Statuses block N has no matching diffs (diffs already at block M > N).
                        // Still need statuses to be complete (a newer status block exists).
                        if self.streaming && !self.order_status_cache.is_front_complete() {
                            return None;
                        }
                        let statuses = self.order_status_cache.pop_front().unwrap();
                        let empty_diffs = statuses.empty_with_metadata();
                        Some((statuses, empty_diffs))
                    }
                }
            }
            _ => None,
        }
    }

    fn receive_batch(&mut self, updates: EventBatch) -> Result<()> {
        match updates {
            EventBatch::Orders(batch) => {
                self.order_status_cache.push(batch);
            }
            EventBatch::BookDiffs(batch) => {
                // In streaming mode, immediately forward raw diffs for low-latency consumption
                if self.streaming {
                    if let Some(tx) = &self.internal_message_tx {
                        let diffs = batch.events_ref().to_vec();
                        let time = batch.block_time();
                        let height = batch.block_number();
                        let msg = Arc::new(InternalMessage::StreamingBookDiffs { diffs, time, height });
                        let _unused = tx.send(msg);
                    }
                }
                self.order_diff_cache.push(batch);
            }
            EventBatch::Fills(batch) => {
                if self.last_fill.is_none_or(|height| height < batch.block_number()) {
                    // send fill updates if we received a new update
                    if let Some(tx) = &self.internal_message_tx {
                        let snapshot = Arc::new(InternalMessage::Fills { batch });
                        let _unused = tx.send(snapshot);
                    }
                }
            }
        }
        if self.is_ready() {
            while let Some((order_statuses, order_diffs)) = self.pop_cache() {
                self.order_book_state
                    .as_mut()
                    .map(|book| book.apply_updates(order_statuses.clone(), order_diffs.clone()))
                    .transpose()?;
                if let Some(cache) = &mut self.fetched_snapshot_cache {
                    cache.push_back((order_statuses.clone(), order_diffs.clone()));
                }
                // Update open orders state and collect changed users
                let changed_users = self.update_open_orders(&order_statuses, &order_diffs);
                if let Some(tx) = &self.internal_message_tx {
                    let open_orders_update = if !changed_users.is_empty() {
                        let mut updates = HashMap::new();
                        for user in &changed_users {
                            updates.insert(*user, self.get_open_orders(user));
                        }
                        Some(updates)
                    } else {
                        None
                    };
                    let price_boundaries = self
                        .order_book_state
                        .as_ref()
                        .map(|s| s.price_boundaries(L4_BOOK_MAX_LEVELS))
                        .unwrap_or_default();
                    let updates = Arc::new(InternalMessage::L4BookUpdates {
                        diff_batch: order_diffs,
                        price_boundaries,
                    });
                    let _unused = tx.send(updates);
                    if let Some(oo_updates) = open_orders_update {
                        let msg = Arc::new(InternalMessage::OpenOrdersUpdate { changed_users: oo_updates });
                        let _unused = tx.send(msg);
                    }
                }
            }
        }
        Ok(())
    }

    fn begin_caching(&mut self) {
        self.fetched_snapshot_cache = Some(VecDeque::new());
    }

    // tkae the cached updates and stop collecting updates
    fn take_cache(&mut self) -> VecDeque<(Batch<NodeDataOrderStatus>, Batch<NodeDataOrderDiff>)> {
        self.fetched_snapshot_cache.take().unwrap_or_default()
    }

    fn init_from_snapshot(&mut self, snapshot: Snapshots<InnerL4Order>, height: u64) {
        info!("No existing snapshot");
        // Initialize open orders from snapshot before it's consumed
        self.open_orders.clear();
        for (_, book) in snapshot.as_ref() {
            for side_orders in book.as_ref() {
                for order in side_orders {
                    let l4_order = L4Order::from(order.clone());
                    self.open_orders.entry(order.user).or_default().insert(order.oid, l4_order);
                }
            }
        }
        info!("Initialized open orders for {} users", self.open_orders.len());
        let mut new_order_book = OrderBookState::from_snapshot(snapshot, height, 0, true, self.ignore_spot);
        let mut retry = false;
        while let Some((order_statuses, order_diffs)) = self.pop_cache() {
            self.update_open_orders(&order_statuses, &order_diffs);
            if new_order_book.apply_updates(order_statuses, order_diffs).is_err() {
                info!(
                    "Failed to apply updates to this book (likely missing older updates). Waiting for next snapshot."
                );
                retry = true;
                break;
            }
        }
        if !retry {
            self.order_book_state = Some(new_order_book);
            info!("Order book ready");
        } else {
            self.open_orders.clear();
        }
    }

    // compute snapshot for specific coins only
    pub(crate) fn compute_snapshot_for_coins(&self, coins: &[Coin]) -> Option<TimedSnapshots> {
        self.order_book_state.as_ref().map(|o| o.compute_snapshot_for_coins(coins))
    }

    // prevent snapshotting mutiple times at the same height
    fn l2_snapshots(&mut self, prevent_future_snaps: bool) -> Option<(u64, L2Snapshots)> {
        self.order_book_state.as_mut().and_then(|o| o.l2_snapshots(prevent_future_snaps))
    }

    /// Returns the current open orders for a given user address
    pub(crate) fn get_open_orders(&self, user: &Address) -> Vec<L4Order> {
        self.open_orders.get(user).map(|orders| orders.values().cloned().collect()).unwrap_or_default()
    }

    fn pending_mut(&mut self, event_source: EventSource) -> &mut String {
        match event_source {
            EventSource::Fills => &mut self.fill_pending,
            EventSource::OrderStatuses => &mut self.order_status_pending,
            EventSource::OrderDiffs => &mut self.order_diff_pending,
        }
    }

    /// Applies order status and book diff events to the open orders map.
    /// Returns the set of users whose open orders changed in this block.
    fn update_open_orders(
        &mut self,
        order_statuses: &Batch<NodeDataOrderStatus>,
        order_diffs: &Batch<NodeDataOrderDiff>,
    ) -> HashSet<Address> {
        let mut changed_users = HashSet::new();

        // Build map of newly-inserted orders from order statuses
        let mut new_orders: HashMap<u64, &NodeDataOrderStatus> = HashMap::new();
        for status in order_statuses.events_ref() {
            if status.is_inserted_into_book() {
                new_orders.insert(status.order.oid, status);
            }
        }

        // Process book diffs to update open orders
        for diff in order_diffs.events_ref() {
            let oid = diff.raw_oid();
            let user = diff.user();

            match &diff.raw_book_diff {
                OrderDiff::New { sz } => {
                    if let Some(status) = new_orders.remove(&oid) {
                        let mut order = status.order.clone();
                        order.user = Some(status.user);
                        order.sz = sz.clone();
                        self.open_orders.entry(status.user).or_default().insert(oid, order);
                        changed_users.insert(status.user);
                    }
                }
                OrderDiff::Update { new_sz, .. } => {
                    if let Some(user_orders) = self.open_orders.get_mut(&user) {
                        if let Some(order) = user_orders.get_mut(&oid) {
                            order.sz = new_sz.clone();
                            changed_users.insert(user);
                        }
                    }
                }
                OrderDiff::Remove => {
                    if let Some(user_orders) = self.open_orders.get_mut(&user) {
                        if user_orders.remove(&oid).is_some() {
                            changed_users.insert(user);
                            if user_orders.is_empty() {
                                self.open_orders.remove(&user);
                            }
                        }
                    }
                }
            }
        }

        changed_users
    }
}

impl OrderBookListener {
    fn process_update(&mut self, event: &Event, new_path: &PathBuf, event_source: EventSource) -> Result<()> {
        if event.kind.is_create() {
            info!("-- Event: {} created --", new_path.display());
            self.on_file_creation(new_path.clone(), event_source)?;
            // Some platforms deliver create after bytes have already been written.
            // Read the new file once immediately so we do not miss its initial contents.
            self.on_file_modification(event_source)?;
        }
        // Check for `Modify` event (only if the file is already initialized)
        else {
            // If we are not tracking anything right now, we treat a file update as declaring that it has been created.
            // Unfortunately, we miss the update that occurs at this time step.
            // We go to the end of the file to read for updates after that.
            if self.is_reading(event_source) {
                self.on_file_modification(event_source)?;
            } else {
                info!("-- Event: {} modified, tracking it now --", new_path.display());
                let file = self.file_mut(event_source);
                let mut new_file = File::open(new_path)?;
                new_file.seek(SeekFrom::End(0))?;
                *file = Some(new_file);
                self.pending_mut(event_source).clear();
            }
        }
        Ok(())
    }
}

impl DirectoryListener for OrderBookListener {
    fn is_reading(&self, event_source: EventSource) -> bool {
        match event_source {
            EventSource::Fills => self.fill_status_file.is_some(),
            EventSource::OrderStatuses => self.order_status_file.is_some(),
            EventSource::OrderDiffs => self.order_diff_file.is_some(),
        }
    }

    fn file_mut(&mut self, event_source: EventSource) -> &mut Option<File> {
        match event_source {
            EventSource::Fills => &mut self.fill_status_file,
            EventSource::OrderStatuses => &mut self.order_status_file,
            EventSource::OrderDiffs => &mut self.order_diff_file,
        }
    }

    fn on_file_creation(&mut self, new_file: PathBuf, event_source: EventSource) -> Result<()> {
        if let Some(file) = self.file_mut(event_source).as_mut() {
            let mut buf = String::new();
            file.read_to_string(&mut buf)?;
            if !buf.is_empty() {
                self.process_data(buf, event_source)?;
            }
            if !self.pending_mut(event_source).is_empty() {
                let tail = std::mem::take(self.pending_mut(event_source));
                self.process_data(format!("{tail}\n"), event_source)?;
                if !self.pending_mut(event_source).is_empty() {
                    return Err(format!("{event_source} file ended with a partial JSON record").into());
                }
            }
        }
        *self.file_mut(event_source) = Some(File::open(new_file)?);
        self.pending_mut(event_source).clear();
        Ok(())
    }

    fn process_data(&mut self, data: String, event_source: EventSource) -> Result<()> {
        let mut pending = std::mem::take(self.pending_mut(event_source));
        pending.push_str(&data);

        loop {
            let Some(newline_idx) = pending.find('\n') else {
                break;
            };

            let mut line = pending.drain(..=newline_idx).collect::<String>();
            if line.ends_with('\n') {
                line.pop();
            }
            if line.ends_with('\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }

            let res = match event_source {
                EventSource::Fills => serde_json::from_str::<Batch<NodeDataFill>>(&line).map(|batch| {
                    let height = batch.block_number();
                    (height, EventBatch::Fills(batch))
                }),
                EventSource::OrderStatuses => serde_json::from_str(&line)
                    .map(|batch: Batch<NodeDataOrderStatus>| (batch.block_number(), EventBatch::Orders(batch))),
                EventSource::OrderDiffs => serde_json::from_str(&line)
                    .map(|batch: Batch<NodeDataOrderDiff>| (batch.block_number(), EventBatch::BookDiffs(batch))),
            };

            let (height, event_batch) = match res {
                Ok(data) => data,
                Err(err) => {
                    let preview = line.chars().take(100).collect::<String>();
                    error!(
                        "{event_source} serialization error {err}, height: {:?}, line: {:?}",
                        self.order_book_state.as_ref().map(OrderBookState::height),
                        preview,
                    );
                    *self.pending_mut(event_source) = pending;
                    return Err(format!("{event_source} serialization error: {err}").into());
                }
            };

            if height % 100 == 0 {
                info!("{event_source} block: {height}");
            }
            if let Err(err) = self.receive_batch(event_batch) {
                self.order_book_state = None;
                *self.pending_mut(event_source) = pending;
                return Err(err);
            }
        }

        *self.pending_mut(event_source) = pending;

        let snapshot = self.l2_snapshots(true);
        if let Some(snapshot) = snapshot {
            if let Some(tx) = &self.internal_message_tx {
                let snapshot = Arc::new(InternalMessage::Snapshot { l2_snapshots: snapshot.1, time: snapshot.0 });
                let _unused = tx.send(snapshot);
            }
        }
        Ok(())
    }
}

pub(crate) struct L2Snapshots(HashMap<Coin, HashMap<L2SnapshotParams, Snapshot<InnerLevel>>>);

impl L2Snapshots {
    pub(crate) const fn as_ref(&self) -> &HashMap<Coin, HashMap<L2SnapshotParams, Snapshot<InnerLevel>>> {
        &self.0
    }
}

pub(crate) struct TimedSnapshots {
    pub(crate) time: u64,
    pub(crate) height: u64,
    pub(crate) snapshot: Snapshots<InnerL4Order>,
}

// Messages sent from node data listener to websocket dispatch to support streaming
pub(crate) enum InternalMessage {
    Snapshot {
        l2_snapshots: L2Snapshots,
        time: u64,
    },
    Fills {
        batch: Batch<NodeDataFill>,
    },
    L4BookUpdates {
        diff_batch: Batch<NodeDataOrderDiff>,
        price_boundaries: HashMap<Coin, [Option<Px>; 2]>,
    },
    OpenOrdersUpdate {
        changed_users: HashMap<Address, Vec<L4Order>>,
    },
    /// Immediate intra-block order diffs forwarded in streaming mode (before block completion)
    StreamingBookDiffs {
        diffs: Vec<NodeDataOrderDiff>,
        time: u64,
        height: u64,
    },
}

#[derive(Eq, PartialEq, Hash)]
pub(crate) struct L2SnapshotParams {
    n_sig_figs: Option<u32>,
    mantissa: Option<u64>,
}

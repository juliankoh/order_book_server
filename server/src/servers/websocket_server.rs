use crate::{
    listeners::order_book::{
        InternalMessage, L2SnapshotParams, L2Snapshots, OrderBookListener, TimedSnapshots, hl_listen,
    },
    order_book::{Coin, Px, Snapshot},
    prelude::*,
    types::{
        L2Book, L4Book, L4BookStream, L4BookUpdates, L4Order, OpenOrdersData, Trade,
        inner::{InnerL4Order, InnerLevel},
        node_data::{Batch, NodeDataFill, NodeDataOrderDiff},
        subscription::{ClientMessage, DEFAULT_LEVELS, ServerResponse, Subscription, SubscriptionManager},
    },
};
use alloy::primitives::Address;
use axum::{Router, response::IntoResponse, routing::get};
use futures_util::{FutureExt, SinkExt, StreamExt};
use log::{error, info};
use std::{
    collections::{HashMap, HashSet},
    env::home_dir,
    sync::Arc,
};
use tokio::select;
use tokio::{
    net::TcpListener,
    sync::{
        Mutex,
        broadcast::{Sender, channel},
    },
};
use yawc::{FrameView, OpCode, WebSocket};

pub async fn run_websocket_server(
    address: &str,
    ignore_spot: bool,
    compression_level: u32,
    streaming: bool,
) -> Result<()> {
    let (internal_message_tx, _) = channel::<Arc<InternalMessage>>(4096);

    // Central task: listen to messages and forward them for distribution
    let home_dir = home_dir().ok_or("Could not find home directory")?;
    let listener = {
        let internal_message_tx = internal_message_tx.clone();
        OrderBookListener::new(Some(internal_message_tx), ignore_spot, streaming)
    };
    let listener = Arc::new(Mutex::new(listener));
    {
        let listener = listener.clone();
        tokio::spawn(async move {
            if let Err(err) = hl_listen(listener, home_dir).await {
                error!("Listener fatal error: {err}");
                std::process::exit(1);
            }
        });
    }

    let websocket_opts =
        yawc::Options::default().with_compression_level(yawc::CompressionLevel::new(compression_level));
    let app = Router::new().route(
        "/ws",
        get({
            let internal_message_tx = internal_message_tx.clone();
            async move |ws_upgrade| {
                ws_handler(ws_upgrade, internal_message_tx.clone(), listener.clone(), ignore_spot, websocket_opts)
            }
        }),
    );

    let listener = TcpListener::bind(address).await?;
    info!("WebSocket server running at ws://{address}");

    if let Err(err) = axum::serve(listener, app.into_make_service()).await {
        error!("Server fatal error: {err}");
        std::process::exit(2);
    }

    Ok(())
}

fn ws_handler(
    incoming: yawc::IncomingUpgrade,
    internal_message_tx: Sender<Arc<InternalMessage>>,
    listener: Arc<Mutex<OrderBookListener>>,
    ignore_spot: bool,
    websocket_opts: yawc::Options,
) -> impl IntoResponse {
    let (resp, fut) = incoming.upgrade(websocket_opts).unwrap();
    tokio::spawn(async move {
        let ws = match fut.await {
            Ok(ok) => ok,
            Err(err) => {
                log::error!("failed to upgrade websocket connection: {err}");
                return;
            }
        };

        handle_socket(ws, internal_message_tx, listener, ignore_spot).await
    });

    resp
}

async fn handle_socket(
    mut socket: WebSocket,
    internal_message_tx: Sender<Arc<InternalMessage>>,
    listener: Arc<Mutex<OrderBookListener>>,
    ignore_spot: bool,
) {
    let mut internal_message_rx = internal_message_tx.subscribe();
    let is_ready = listener.lock().await.is_ready();
    let mut manager = SubscriptionManager::default();
    let mut universe = listener.lock().await.universe().into_iter().map(|c| c.value()).collect();
    if !is_ready {
        let msg = ServerResponse::Error("Order book not ready for streaming (waiting for snapshot)".to_string());
        send_socket_message(&mut socket, msg).await;
        return;
    }
    loop {
        // biased: prefer draining client messages before processing internal broadcasts,
        // so a burst of subscribes isn't interleaved with update messages.
        select! {
            biased;

            msg = socket.next() => {
                // Collect the first frame
                let first_frame = match msg {
                    Some(frame) => frame,
                    None => {
                        info!("Client connection closed");
                        return;
                    }
                };

                // Drain any additional pending client frames to process as a batch
                let mut frames = vec![first_frame];
                while let Some(maybe_frame) = socket.next().now_or_never() {
                    match maybe_frame {
                        Some(frame) => frames.push(frame),
                        None => {
                            info!("Client connection closed during drain");
                            return;
                        }
                    }
                }

                if frames.len() > 1 {
                    info!("Processing batch of {} client messages", frames.len());
                }

                // Parse all frames into client messages
                let mut client_messages = Vec::new();
                for frame in frames {
                    match frame.opcode {
                        OpCode::Text => {
                            let text = match std::str::from_utf8(&frame.payload) {
                                Ok(text) => text,
                                Err(err) => {
                                    log::warn!("unable to parse websocket content: {err}: {:?}", frame.payload.as_ref());
                                    return;
                                }
                            };
                            info!("Client message: {text}");
                            match serde_json::from_str::<ClientMessage>(text) {
                                Ok(value) => client_messages.push(value),
                                Err(_) => {
                                    let msg = ServerResponse::Error(format!("Error parsing JSON into valid websocket request: {text}"));
                                    if !send_socket_message(&mut socket, msg).await {
                                        info!("Client connection lost, cleaning up");
                                        return;
                                    }
                                }
                            }
                        }
                        OpCode::Close => {
                            info!("Client disconnected");
                            return;
                        }
                        _ => {}
                    }
                }

                if client_messages.is_empty() {
                    continue;
                }

                // Pre-compute snapshot data with a single lock acquisition
                let l4_coins: Vec<Coin> = client_messages
                    .iter()
                    .filter_map(|msg| {
                        if let ClientMessage::Subscribe { subscription: Subscription::L4Book { coin } } = msg {
                            Some(Coin::new(coin))
                        } else {
                            None
                        }
                    })
                    .collect();
                let open_order_addrs: Vec<Address> = client_messages
                    .iter()
                    .filter_map(|msg| {
                        if let ClientMessage::Subscribe { subscription: Subscription::OpenOrders { user } } = msg {
                            user.parse::<Address>().ok()
                        } else {
                            None
                        }
                    })
                    .collect();

                let (precomputed_snapshot, precomputed_open_orders) =
                    if !l4_coins.is_empty() || !open_order_addrs.is_empty() {
                        let guard = listener.lock().await;
                        let snapshot = if !l4_coins.is_empty() { guard.compute_snapshot_for_coins(&l4_coins) } else { None };
                        let open_orders: HashMap<Address, Vec<L4Order>> = open_order_addrs
                            .iter()
                            .map(|addr| (*addr, guard.get_open_orders(addr)))
                            .collect();
                        drop(guard);
                        (snapshot, open_orders)
                    } else {
                        (None, HashMap::new())
                    };

                // Process all messages using precomputed data (no further listener locks needed)
                for msg in client_messages {
                    if !receive_client_message(
                        &mut socket,
                        &mut manager,
                        msg,
                        &universe,
                        &precomputed_snapshot,
                        &precomputed_open_orders,
                    )
                    .await
                    {
                        info!("Client connection lost, cleaning up");
                        return;
                    }
                }
            }

            recv_result = internal_message_rx.recv() => {
                match recv_result {
                    Ok(msg) => {
                        let ok = match msg.as_ref() {
                            InternalMessage::Snapshot{ l2_snapshots, time } => {
                                universe = new_universe(l2_snapshots, ignore_spot);
                                let mut ok = true;
                                for sub in manager.l2_books() {
                                    if !send_ws_data_from_snapshot(&mut socket, sub, l2_snapshots.as_ref(), *time).await {
                                        ok = false;
                                        break;
                                    }
                                }
                                ok
                            },
                            InternalMessage::Fills{ batch } => {
                                let mut trades = coin_to_trades(batch);
                                let mut ok = true;
                                for sub in manager.trades() {
                                    if !send_ws_data_from_trades(&mut socket, sub, &mut trades).await {
                                        ok = false;
                                        break;
                                    }
                                }
                                ok
                            },
                            InternalMessage::L4BookUpdates{ diff_batch, price_boundaries } => {
                                let mut book_updates = coin_to_book_updates(diff_batch, price_boundaries);
                                let mut ok = true;
                                for sub in manager.l4_books() {
                                    if !send_ws_data_from_book_updates(&mut socket, sub, &mut book_updates).await {
                                        ok = false;
                                        break;
                                    }
                                }
                                ok
                            },
                            InternalMessage::OpenOrdersUpdate { changed_users } => {
                                let mut ok = true;
                                for sub in manager.open_orders() {
                                    if !send_ws_data_from_open_orders(&mut socket, sub, changed_users).await {
                                        ok = false;
                                        break;
                                    }
                                }
                                ok
                            },
                            InternalMessage::StreamingBookDiffs { diffs, time, height } => {
                                let mut updates = coin_to_streaming_book_updates(diffs, *time, *height);
                                let mut ok = true;
                                for sub in manager.l4_book_streams() {
                                    if !send_ws_data_from_streaming_book_updates(&mut socket, sub, &mut updates).await {
                                        ok = false;
                                        break;
                                    }
                                }
                                ok
                            },
                        };
                        if !ok {
                            info!("Client connection lost, cleaning up");
                            return;
                        }
                    }
                    Err(err) => {
                        error!("Receiver error: {err}");
                        return;
                    }
                }
            }
        }
    }
}

/// Returns `true` if the connection is still alive.
/// Uses precomputed snapshot/open_orders data so no listener lock is needed.
async fn receive_client_message(
    socket: &mut WebSocket,
    manager: &mut SubscriptionManager,
    client_message: ClientMessage,
    universe: &HashSet<String>,
    precomputed_snapshot: &Option<TimedSnapshots>,
    precomputed_open_orders: &HashMap<Address, Vec<L4Order>>,
) -> bool {
    if matches!(client_message, ClientMessage::Ping) {
        let msg = ServerResponse::Pong;
        return send_socket_message(socket, msg).await;
    }
    let subscription = match &client_message {
        ClientMessage::Unsubscribe { subscription } | ClientMessage::Subscribe { subscription } => subscription.clone(),
        ClientMessage::Ping => unreachable!(),
    };
    // this is used for display purposes only, hence unwrap_or_default. It also shouldn't fail
    let sub = serde_json::to_string(&subscription).unwrap_or_default();
    if !subscription.validate(universe) {
        let msg = ServerResponse::Error(format!("Invalid subscription: {sub}"));
        return send_socket_message(socket, msg).await;
    }
    let (word, success) = match &client_message {
        ClientMessage::Subscribe { .. } => ("", manager.subscribe(subscription)),
        ClientMessage::Unsubscribe { .. } => ("un", manager.unsubscribe(subscription)),
        ClientMessage::Ping => unreachable!(),
    };
    if success {
        let snapshot_msg = if let ClientMessage::Subscribe { subscription } = &client_message {
            let msg = subscription.build_snapshot_response(precomputed_snapshot, precomputed_open_orders);
            match msg {
                Ok(msg) => msg,
                Err(err) => {
                    manager.unsubscribe(subscription.clone());
                    let msg = ServerResponse::Error(format!("Unable to grab order book snapshot: {err}"));
                    return send_socket_message(socket, msg).await;
                }
            }
        } else {
            None
        };
        let msg = ServerResponse::SubscriptionResponse(client_message);
        if !send_socket_message(socket, msg).await {
            return false;
        }
        if let Some(snapshot_msg) = snapshot_msg {
            if !send_socket_message(socket, snapshot_msg).await {
                return false;
            }
        }
    } else {
        let msg = ServerResponse::Error(format!("Already {word}subscribed: {sub}"));
        return send_socket_message(socket, msg).await;
    }
    true
}

/// Returns `true` if the message was sent successfully, `false` if the connection is dead.
async fn send_socket_message(socket: &mut WebSocket, msg: ServerResponse) -> bool {
    let channel = msg.channel_name();
    let msg = serde_json::to_string(&msg);
    match msg {
        Ok(msg) => {
            let size = msg.len();
            if size > 500_000 {
                info!("Sending large message: channel={channel} size={size} bytes");
            }
            if let Err(err) = socket.send(FrameView::text(msg)).await {
                error!("Failed to send: channel={channel} size={size} err={err}");
                return false;
            }
        }
        Err(err) => {
            error!("Server response serialization error: {err}");
            return false;
        }
    }
    true
}

// derive it from l2_snapshots because thats convenient
fn new_universe(l2_snapshots: &L2Snapshots, ignore_spot: bool) -> HashSet<String> {
    l2_snapshots
        .as_ref()
        .iter()
        .filter_map(|(c, _)| if !c.is_spot() || !ignore_spot { Some(c.clone().value()) } else { None })
        .collect()
}

async fn send_ws_data_from_snapshot(
    socket: &mut WebSocket,
    subscription: &Subscription,
    snapshot: &HashMap<Coin, HashMap<L2SnapshotParams, Snapshot<InnerLevel>>>,
    time: u64,
) -> bool {
    if let Subscription::L2Book { coin, n_sig_figs, n_levels, mantissa } = subscription {
        let snapshot = snapshot.get(&Coin::new(coin));
        if let Some(snapshot) =
            snapshot.and_then(|snapshot| snapshot.get(&L2SnapshotParams::new(*n_sig_figs, *mantissa)))
        {
            let n_levels = n_levels.unwrap_or(DEFAULT_LEVELS);
            let snapshot = snapshot.truncate(n_levels);
            let snapshot = snapshot.export_inner_snapshot();
            let l2_book = L2Book::from_l2_snapshot(coin.clone(), snapshot, time);
            let msg = ServerResponse::L2Book(l2_book);
            return send_socket_message(socket, msg).await;
        } else {
            error!("Coin {coin} not found");
        }
    }
    true
}

fn coin_to_trades(batch: &Batch<NodeDataFill>) -> HashMap<String, Vec<Trade>> {
    let fills = batch.events_ref();
    let mut trades = HashMap::new();
    for pair in fills.chunks_exact(2) {
        let mut fill_map = HashMap::new();
        fill_map.insert(pair[0].1.side, pair[0].clone());
        fill_map.insert(pair[1].1.side, pair[1].clone());
        let trade = Trade::from_fills(fill_map);
        let coin = trade.coin.clone();
        trades.entry(coin).or_insert_with(Vec::new).push(trade);
    }
    trades
}

fn coin_to_book_updates(
    diff_batch: &Batch<NodeDataOrderDiff>,
    price_boundaries: &HashMap<Coin, [Option<Px>; 2]>,
) -> HashMap<String, L4BookUpdates> {
    let time = diff_batch.block_time();
    let height = diff_batch.block_number();
    let mut updates = HashMap::new();
    for diff in diff_batch.events_ref() {
        let coin = diff.coin();
        // Filter: only include diffs near top of book
        if let Some([bid_floor, ask_ceiling]) = price_boundaries.get(&coin) {
            if let Ok(px) = Px::parse_from_str(diff.px()) {
                let dominated_by_bids = bid_floor.is_some_and(|b| px < b);
                let dominated_by_asks = ask_ceiling.is_some_and(|a| px > a);
                if dominated_by_bids && dominated_by_asks {
                    continue;
                }
            }
            // on parse failure, include conservatively
        }
        let coin = coin.value();
        updates.entry(coin).or_insert_with(|| L4BookUpdates::new(time, height)).book_diffs.push(diff.clone());
    }
    updates
}

fn coin_to_streaming_book_updates(
    diffs: &[NodeDataOrderDiff],
    time: u64,
    height: u64,
) -> HashMap<String, L4BookStream> {
    let mut updates = HashMap::new();
    for diff in diffs {
        let coin = diff.coin().value();
        updates
            .entry(coin.clone())
            .or_insert_with(|| L4BookStream { coin, time, height, book_diffs: Vec::new() })
            .book_diffs
            .push(diff.clone());
    }
    updates
}

async fn send_ws_data_from_book_updates(
    socket: &mut WebSocket,
    subscription: &Subscription,
    book_updates: &mut HashMap<String, L4BookUpdates>,
) -> bool {
    if let Subscription::L4Book { coin } = subscription {
        if let Some(updates) = book_updates.remove(coin) {
            let msg = ServerResponse::L4Book(L4Book::Updates(updates));
            return send_socket_message(socket, msg).await;
        }
    }
    true
}

async fn send_ws_data_from_streaming_book_updates(
    socket: &mut WebSocket,
    subscription: &Subscription,
    book_updates: &mut HashMap<String, L4BookStream>,
) -> bool {
    if let Subscription::L4BookStream { coin } = subscription {
        if let Some(updates) = book_updates.remove(coin) {
            let msg = ServerResponse::L4BookStream(updates);
            return send_socket_message(socket, msg).await;
        }
    }
    true
}

async fn send_ws_data_from_open_orders(
    socket: &mut WebSocket,
    subscription: &Subscription,
    changed_users: &HashMap<Address, Vec<L4Order>>,
) -> bool {
    if let Subscription::OpenOrders { user } = subscription {
        if let Ok(addr) = user.parse::<Address>() {
            if let Some(orders) = changed_users.get(&addr) {
                let msg = ServerResponse::OpenOrders(OpenOrdersData { user: addr, open_orders: orders.clone() });
                return send_socket_message(socket, msg).await;
            }
        }
    }
    true
}

async fn send_ws_data_from_trades(
    socket: &mut WebSocket,
    subscription: &Subscription,
    trades: &mut HashMap<String, Vec<Trade>>,
) -> bool {
    if let Subscription::Trades { coin } = subscription {
        if let Some(trades) = trades.remove(coin) {
            let msg = ServerResponse::Trades(trades);
            return send_socket_message(socket, msg).await;
        }
    }
    true
}

/// Maximum number of price levels to include in an L4Book initial snapshot.
const L4_SNAPSHOT_MAX_LEVELS: usize = 10;

/// Truncate a list of orders (sorted by price priority) to the top N distinct price levels.
fn truncate_to_n_levels(orders: Vec<InnerL4Order>, n_levels: usize) -> Vec<InnerL4Order> {
    let mut result = Vec::new();
    let mut seen_levels = 0usize;
    let mut last_px = None;
    for order in orders {
        let px = order.limit_px;
        if last_px != Some(px) {
            seen_levels += 1;
            if seen_levels > n_levels {
                break;
            }
            last_px = Some(px);
        }
        result.push(order);
    }
    result
}

impl Subscription {
    /// Build snapshot response from precomputed data (no listener lock needed).
    /// L4Book snapshots are truncated to the top N price levels per side.
    fn build_snapshot_response(
        &self,
        precomputed_snapshot: &Option<TimedSnapshots>,
        precomputed_open_orders: &HashMap<Address, Vec<L4Order>>,
    ) -> Result<Option<ServerResponse>> {
        if let Self::L4Book { coin } = self {
            if let Some(TimedSnapshots { time, height, snapshot }) = precomputed_snapshot {
                let snap = snapshot.as_ref().get(&Coin::new(coin));
                if let Some(snap) = snap {
                    let [full_bids, full_asks] = snap.as_ref();
                    let total_orders = full_bids.len() + full_asks.len();
                    let levels: [Vec<L4Order>; 2] = snap.as_ref().clone().map(|orders| {
                        truncate_to_n_levels(orders, L4_SNAPSHOT_MAX_LEVELS)
                            .into_iter()
                            .map(L4Order::from)
                            .collect()
                    });
                    let truncated_orders = levels[0].len() + levels[1].len();
                    if truncated_orders < total_orders {
                        info!(
                            "L4Book snapshot truncated for {coin}: {total_orders} -> {truncated_orders} orders (top {L4_SNAPSHOT_MAX_LEVELS} levels)"
                        );
                    }
                    return Ok(Some(ServerResponse::L4Book(L4Book::Snapshot {
                        coin: coin.clone(),
                        time: *time,
                        height: *height,
                        levels,
                    })));
                }
            }
            return Err("Snapshot Failed".into());
        }
        if let Self::OpenOrders { user } = self {
            let addr = user.parse::<Address>().map_err(|e| format!("Invalid address: {e}"))?;
            if let Some(orders) = precomputed_open_orders.get(&addr) {
                return Ok(Some(ServerResponse::OpenOrders(OpenOrdersData {
                    user: addr,
                    open_orders: orders.clone(),
                })));
            }
            return Err("Open orders not available".into());
        }
        Ok(None)
    }
}

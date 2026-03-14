use crate::{
    listeners::order_book::{L2SnapshotParams, L2Snapshots},
    order_book::{
        multi_book::{OrderBooks, Snapshots},
        types::InnerOrder,
    },
    prelude::*,
    types::node_data::{Batch, NodeDataFill, NodeDataOrderDiff, NodeDataOrderStatus},
};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use reqwest::Client;
use serde_json::json;
use std::collections::VecDeque;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub(super) async fn process_rmp_file(dir: &Path) -> Result<PathBuf> {
    let output_path = dir.join("out.json");
    let payload = json!({
        "type": "fileSnapshot",
        "request": {
            "type": "l4Snapshots",
            "includeUsers": true,
            "includeTriggerOrders": false
        },
        "outPath": output_path,
        "includeHeightInOutput": true
    });

    let client = Client::new();
    client
        .post("http://localhost:3001/info")
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?
        .error_for_status()?;
    Ok(output_path)
}

pub(super) fn validate_snapshot_consistency<O: Clone + PartialEq + Debug>(
    snapshot: &Snapshots<O>,
    expected: Snapshots<O>,
    ignore_spot: bool,
) -> Result<()> {
    let mut snapshot_map: HashMap<_, _> =
        expected.value().into_iter().filter(|(c, _)| !c.is_spot() || !ignore_spot).collect();

    for (coin, book) in snapshot.as_ref() {
        if ignore_spot && coin.is_spot() {
            continue;
        }
        let book1 = book.as_ref();
        if let Some(book2) = snapshot_map.remove(coin) {
            for (orders1, orders2) in book1.as_ref().iter().zip(book2.as_ref()) {
                for (order1, order2) in orders1.iter().zip(orders2.iter()) {
                    if *order1 != *order2 {
                        return Err(
                            format!("Orders do not match, expected: {:?} received: {:?}", *order2, *order1).into()
                        );
                    }
                }
            }
        } else if !book1[0].is_empty() || !book1[1].is_empty() {
            return Err(format!("Missing {} book", coin.value()).into());
        }
    }
    if !snapshot_map.is_empty() {
        return Err("Extra orderbooks detected".to_string().into());
    }
    Ok(())
}

impl L2SnapshotParams {
    pub(crate) const fn new(n_sig_figs: Option<u32>, mantissa: Option<u64>) -> Self {
        Self { n_sig_figs, mantissa }
    }
}

pub(super) fn compute_l2_snapshots<O: InnerOrder + Send + Sync>(order_books: &OrderBooks<O>) -> L2Snapshots {
    L2Snapshots(
        order_books
            .as_ref()
            .par_iter()
            .map(|(coin, order_book)| {
                let snapshot = order_book.to_l2_snapshot(None, None, None);
                let mut entries = HashMap::new();
                entries.insert(L2SnapshotParams { n_sig_figs: None, mantissa: None }, snapshot);
                (coin.clone(), entries)
            })
            .collect(),
    )
}

pub(super) enum EventBatch {
    Orders(Batch<NodeDataOrderStatus>),
    BookDiffs(Batch<NodeDataOrderDiff>),
    Fills(Batch<NodeDataFill>),
}

pub(super) enum PushResult {
    /// A new block was started in the queue
    NewBlock,
    /// Events were merged into the existing last batch (same block number)
    Merged,
    /// The batch was stale (block number older than what we've seen)
    Stale,
}

pub(super) struct BatchQueue<T> {
    deque: VecDeque<Batch<T>>,
    last_ts: Option<u64>,
}

impl<T> BatchQueue<T> {
    pub(super) const fn new() -> Self {
        Self { deque: VecDeque::new(), last_ts: None }
    }

    pub(super) fn push(&mut self, block: Batch<T>) -> PushResult {
        if let Some(last_ts) = self.last_ts {
            if block.block_number() < last_ts {
                return PushResult::Stale;
            }
            if block.block_number() == last_ts {
                // Streaming mode: merge events into the existing batch for this block
                if let Some(back) = self.deque.back_mut() {
                    back.extend_events(block);
                    return PushResult::Merged;
                }
            }
        }
        self.last_ts = Some(block.block_number());
        self.deque.push_back(block);
        PushResult::NewBlock
    }

    pub(super) fn pop_front(&mut self) -> Option<Batch<T>> {
        self.deque.pop_front()
    }

    pub(super) fn front(&self) -> Option<&Batch<T>> {
        self.deque.front()
    }

    /// Returns true if the front block is complete (a newer block exists behind it)
    pub(super) fn is_front_complete(&self) -> bool {
        self.deque.len() >= 2
    }

    #[allow(dead_code)]
    pub(super) fn len(&self) -> usize {
        self.deque.len()
    }
}

use std::path::{Path, PathBuf};

use alloy::primitives::Address;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::{
    order_book::{Coin, Oid},
    types::{Fill, L4Order, OrderDiff},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NodeDataOrderDiff {
    user: Address,
    oid: u64,
    px: String,
    coin: String,
    pub(crate) raw_book_diff: OrderDiff,
}

impl NodeDataOrderDiff {
    pub(crate) fn diff(&self) -> OrderDiff {
        self.raw_book_diff.clone()
    }
    pub(crate) const fn oid(&self) -> Oid {
        Oid::new(self.oid)
    }

    pub(crate) fn coin(&self) -> Coin {
        Coin::new(&self.coin)
    }

    pub(crate) const fn user(&self) -> Address {
        self.user
    }

    pub(crate) const fn raw_oid(&self) -> u64 {
        self.oid
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NodeDataFill(pub Address, pub Fill);

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct NodeDataOrderStatus {
    pub time: NaiveDateTime,
    pub user: Address,
    pub status: String,
    pub order: L4Order,
}

impl NodeDataOrderStatus {
    pub(crate) fn is_inserted_into_book(&self) -> bool {
        (self.status == "open" && !self.order.is_trigger && (self.order.tif != Some("Ioc".to_string())))
            || (self.order.is_trigger && self.status == "triggered")
    }
}

#[derive(Clone, Copy, strum_macros::Display)]
pub(crate) enum EventSource {
    Fills,
    OrderStatuses,
    OrderDiffs,
}

impl EventSource {
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub(crate) fn event_source_dir(self, dir: &Path) -> PathBuf {
        self.event_source_dir_streaming(dir, false)
    }

    #[must_use]
    pub(crate) fn event_source_dir_streaming(self, dir: &Path, streaming: bool) -> PathBuf {
        let suffix = if streaming { "_streaming" } else { "_by_block" };
        match self {
            Self::Fills => dir.join(format!("hl/data/node_fills{suffix}")),
            Self::OrderStatuses => dir.join(format!("hl/data/node_order_statuses{suffix}")),
            Self::OrderDiffs => dir.join(format!("hl/data/node_raw_book_diffs{suffix}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Batch<E> {
    local_time: NaiveDateTime,
    block_time: NaiveDateTime,
    block_number: u64,
    events: Vec<E>,
}

impl<E> Batch<E> {
    #[allow(clippy::unwrap_used)]
    pub(crate) fn block_time(&self) -> u64 {
        self.block_time.and_utc().timestamp_millis().try_into().unwrap()
    }

    pub(crate) const fn block_number(&self) -> u64 {
        self.block_number
    }

    pub(crate) fn events(self) -> Vec<E> {
        self.events
    }

    pub(crate) fn events_ref(&self) -> &[E] {
        &self.events
    }

    pub(crate) fn extend_events(&mut self, other: Self) {
        self.events.extend(other.events);
    }

    pub(crate) fn empty_with_metadata<T>(&self) -> Batch<T> {
        Batch {
            local_time: self.local_time,
            block_time: self.block_time,
            block_number: self.block_number,
            events: Vec::new(),
        }
    }
}

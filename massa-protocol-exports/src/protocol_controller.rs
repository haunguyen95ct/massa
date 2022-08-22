// Copyright (c) 2022 MASSA LABS <info@massa.net>

use crate::error::ProtocolError;
use massa_logging::massa_trace;

use massa_models::{
    prehash::{Map, Set},
    Slot,
};
use massa_models::{
    BlockId, EndorsementId, OperationId, WrappedEndorsement, WrappedHeader, WrappedOperation,
};
use massa_network_exports::NetworkEventReceiver;
use massa_storage::Storage;
use serde::Serialize;
use std::collections::VecDeque;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tracing::debug;

/// Possible types of events that can happen.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum ProtocolEvent {
    /// A block with a valid signature has been received.
    ReceivedBlock {
        /// block ID
        block_id: BlockId,
        /// block slot
        slot: Slot,
        /// storage instance containing the block and its dependencies (except the parents)
        storage: Storage,
    },
    /// A block header with a valid signature has been received.
    ReceivedBlockHeader {
        /// its id
        block_id: BlockId,
        /// The header
        header: WrappedHeader,
    },
}
/// Possible types of pool events that can happen.
#[derive(Debug)]
pub enum ProtocolPoolEvent {
    /// Operations were received
    ReceivedOperations {
        /// the operations
        operations: Map<OperationId, WrappedOperation>,
        /// whether or not to signal the end of processing the batch.
        done_signal: Option<oneshot::Sender<()>>,
    },
    /// Endorsements were received
    ReceivedEndorsements {
        /// the endorsements
        endorsements: Map<EndorsementId, WrappedEndorsement>,
        /// whether or not to propagate endorsements
        propagate: bool,
    },
}

/// block result: map block id to
/// ```md
/// Option(
///     Option(set(operation id)),
///     Option(Vec(endorsement id))
/// )
/// ```
pub type BlocksResults =
    Map<BlockId, Option<(Option<Set<OperationId>>, Option<Vec<EndorsementId>>)>>;

/// Commands that protocol worker can process
#[derive(Debug)]
pub enum ProtocolCommand {
    /// Notify block integration of a given block.
    IntegratedBlock {
        /// block id
        block_id: BlockId,
        /// block storage
        storage: Storage,
    },
    /// A block, or it's header, amounted to an attempted attack.
    AttackBlockDetected(BlockId),
    /// Wish list delta
    WishlistDelta {
        /// add to wish list
        new: Set<BlockId>,
        /// remove from wish list
        remove: Set<BlockId>,
    },
    /// Propagate operations (send batches)
    /// note: Set<OperationId> are replaced with OperationPrefixIds
    ///       by the controller
    PropagateOperations(Set<OperationId>),
    /// Propagate endorsements
    PropagateEndorsements(Map<EndorsementId, WrappedEndorsement>),
}

/// protocol management commands
#[derive(Debug, Serialize)]
pub enum ProtocolManagementCommand {}

/// protocol command sender
#[derive(Clone)]
pub struct ProtocolCommandSender(pub mpsc::Sender<ProtocolCommand>);

impl ProtocolCommandSender {
    /// Sends the order to propagate the header of a block
    ///
    /// # Arguments
    /// * block_id : ID of the block
    /// * storage: Storage instance containing references to the block and all its dependencies
    pub async fn integrated_block(
        &mut self,
        block_id: BlockId,
        storage: Storage,
    ) -> Result<(), ProtocolError> {
        massa_trace!("protocol.command_sender.integrated_block", {
            "block_id": block_id
        });
        self.0
            .send(ProtocolCommand::IntegratedBlock { block_id, storage })
            .await
            .map_err(|_| ProtocolError::ChannelError("block_integrated command send error".into()))
    }

    /// Notify to protocol an attack attempt.
    pub async fn notify_block_attack(&mut self, block_id: BlockId) -> Result<(), ProtocolError> {
        massa_trace!("protocol.command_sender.notify_block_attack", {
            "block_id": block_id
        });
        self.0
            .send(ProtocolCommand::AttackBlockDetected(block_id))
            .await
            .map_err(|_| {
                ProtocolError::ChannelError("notify_block_attack command send error".into())
            })
    }

    /// update the block wish list
    pub async fn send_wishlist_delta(
        &mut self,
        new: Set<BlockId>,
        remove: Set<BlockId>,
    ) -> Result<(), ProtocolError> {
        massa_trace!("protocol.command_sender.send_wishlist_delta", { "new": new, "remove": remove });
        self.0
            .send(ProtocolCommand::WishlistDelta { new, remove })
            .await
            .map_err(|_| {
                ProtocolError::ChannelError("send_wishlist_delta command send error".into())
            })
    }

    /// Propagate a batch of operation ids (from pool).
    ///
    /// note: Full `OperationId` is replaced by a `OperationPrefixId` later by the worker.
    pub async fn propagate_operations(
        &mut self,
        operation_ids: Set<OperationId>,
    ) -> Result<(), ProtocolError> {
        massa_trace!("protocol.command_sender.propagate_operations", {
            "operations": operation_ids
        });
        self.0
            .send(ProtocolCommand::PropagateOperations(operation_ids))
            .await
            .map_err(|_| {
                ProtocolError::ChannelError("propagate_operation command send error".into())
            })
    }

    /// propagate endorsements to connected node
    pub async fn propagate_endorsements(
        &mut self,
        endorsements: Map<EndorsementId, WrappedEndorsement>,
    ) -> Result<(), ProtocolError> {
        massa_trace!("protocol.command_sender.propagate_endorsements", {
            "endorsements": endorsements
        });
        self.0
            .send(ProtocolCommand::PropagateEndorsements(endorsements))
            .await
            .map_err(|_| {
                ProtocolError::ChannelError("propagate_endorsements command send error".into())
            })
    }
}

/// Protocol event receiver
pub struct ProtocolEventReceiver(pub mpsc::Receiver<ProtocolEvent>);

impl ProtocolEventReceiver {
    /// Receives the next `ProtocolEvent` from connected Node.
    /// None is returned when all Sender halves have dropped,
    /// indicating that no further values can be sent on the channel
    pub async fn wait_event(&mut self) -> Result<ProtocolEvent, ProtocolError> {
        massa_trace!("protocol.event_receiver.wait_event", {});
        self.0.recv().await.ok_or_else(|| {
            ProtocolError::ChannelError(
                "DefaultProtocolController wait_event channel recv failed".into(),
            )
        })
    }

    /// drains remaining events and returns them in a `VecDeque`
    /// note: events are sorted from oldest to newest
    pub async fn drain(mut self) -> VecDeque<ProtocolEvent> {
        let mut remaining_events: VecDeque<ProtocolEvent> = VecDeque::new();
        while let Some(evt) = self.0.recv().await {
            debug!(
                "after receiving event from ProtocolEventReceiver.0 in protocol_controller drain"
            );
            remaining_events.push_back(evt);
        }
        remaining_events
    }
}

/// Protocol pool event receiver
pub struct ProtocolPoolEventReceiver(pub mpsc::Receiver<ProtocolPoolEvent>);

impl ProtocolPoolEventReceiver {
    /// Receives the next `ProtocolPoolEvent`
    /// None is returned when all Sender halves have dropped,
    /// indicating that no further values can be sent on the channel
    pub async fn wait_event(&mut self) -> Result<ProtocolPoolEvent, ProtocolError> {
        massa_trace!("protocol.pool_event_receiver.wait_event", {});
        self.0.recv().await.ok_or_else(|| {
            ProtocolError::ChannelError(
                "DefaultProtocolController wait_pool_event channel recv failed".into(),
            )
        })
    }

    /// drains remaining events and returns them in a `VecDeque`
    /// note: events are sorted from oldest to newest
    pub async fn drain(mut self) -> VecDeque<ProtocolPoolEvent> {
        let mut remaining_events: VecDeque<ProtocolPoolEvent> = VecDeque::new();
        while let Some(evt) = self.0.recv().await {
            debug!(
                "after receiving event from ProtocolPoolEventReceiver.0 in protocol_controller drain"
            );
            remaining_events.push_back(evt);
        }
        remaining_events
    }
}

/// protocol manager used to stop the protocol
pub struct ProtocolManager {
    join_handle: JoinHandle<Result<NetworkEventReceiver, ProtocolError>>,
    manager_tx: mpsc::Sender<ProtocolManagementCommand>,
}

impl ProtocolManager {
    /// new protocol manager
    pub fn new(
        join_handle: JoinHandle<Result<NetworkEventReceiver, ProtocolError>>,
        manager_tx: mpsc::Sender<ProtocolManagementCommand>,
    ) -> Self {
        ProtocolManager {
            join_handle,
            manager_tx,
        }
    }

    /// Stop the protocol controller
    pub async fn stop(
        self,
        protocol_event_receiver: ProtocolEventReceiver,
        //protocol_pool_event_receiver: ProtocolPoolEventReceiver,
    ) -> Result<NetworkEventReceiver, ProtocolError> {
        drop(self.manager_tx);
        let _remaining_events = protocol_event_receiver.drain().await;
        //let _remaining_events = protocol_pool_event_receiver.drain().await;
        let network_event_receiver = self.join_handle.await??;
        Ok(network_event_receiver)
    }
}

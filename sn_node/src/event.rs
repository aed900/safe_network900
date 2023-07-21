// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use sn_dbc::DbcId;
use sn_protocol::storage::{ChunkAddress, RegisterAddress};
use tokio::sync::broadcast;

const NODE_EVENT_CHANNEL_SIZE: usize = 10_000;

/// Channel where users of the public API can listen to events broadcasted by the node.
#[derive(Clone, Debug)]
pub struct NodeEventsChannel(broadcast::Sender<NodeEvent>);

/// Type of channel receiver where events are broadcasted to by the node.
pub type NodeEventsReceiver = broadcast::Receiver<NodeEvent>;

impl Default for NodeEventsChannel {
    fn default() -> Self {
        Self(broadcast::channel(NODE_EVENT_CHANNEL_SIZE).0)
    }
}

impl NodeEventsChannel {
    /// Returns a new receiver to listen to the channel.
    /// Multiple receivers can be actively listening.
    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.0.subscribe()
    }

    // Broadcast a new event, meant to be a helper only used by the sn_node's internals.
    pub(crate) fn broadcast(&self, event: NodeEvent) {
        if let Err(err) = self.0.send(event.clone()) {
            trace!("Error occurred when trying to broadcast a node event ({event:?}): {err}");
        }
    }
}

/// Type of events broadcasted by the node to the public API.
#[derive(Clone, Debug)]
pub enum NodeEvent {
    /// Dialing an initial peer and attempting to connect to the network
    AttemptingNetworkConnection,
    /// Attempt to dial initial peers and connect to network is timing out after a period of time
    NetworkConnectionTimingOut,
    /// The node has been connected to the network
    ConnectedToNetwork,
    /// A Chunk has been stored in local storage
    ChunkStored(ChunkAddress),
    /// A Register has been created in local storage
    RegisterCreated(RegisterAddress),
    /// A Register edit operation has been applied in local storage
    RegisterEdited(RegisterAddress),
    /// A DBC Spend has been stored in local storage
    SpendStored(DbcId),
    /// One of the sub event channel closed and unrecoverable.
    ChannelClosed,
    /// AutoNAT discovered we are behind a NAT, thus private.
    BehindNat,
}

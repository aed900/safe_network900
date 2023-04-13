// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::{error::Result, event::NodeEventsChannel, Node, NodeEvent};

use crate::{
    network::{NetworkEvent, SwarmDriver, CLOSE_GROUP_SIZE},
    protocol::{
        messages::{Cmd, CmdResponse, Event, Query, QueryResponse, Request, Response},
        types::{
            address::{dbc_address, DbcAddress},
            error::Error as ProtocolError,
            register::User,
        },
    },
    storage::DataStorage,
};

use sn_dbc::{DbcTransaction, SignedSpend};

use futures::future::select_all;
use libp2p::{request_response::ResponseChannel, PeerId};
use std::{collections::BTreeSet, net::SocketAddr, time::Duration};
use tokio::task::spawn;
use xor_name::XorName;

impl Node {
    /// Write to storage.
    pub async fn write(&self, cmd: &Cmd) -> CmdResponse {
        info!("Write: {cmd:?}");
        self.storage.write(cmd).await
    }

    /// Read from storage.
    pub async fn read(&self, query: &Query) -> QueryResponse {
        self.storage.read(query, User::Anyone).await
    }

    /// Asynchronously runs a new node instance, setting up the swarm driver,
    /// creating a data storage, and handling network events. Returns the
    /// created node and a `NodeEventsChannel` for listening to node-related
    /// events.
    ///
    /// # Returns
    ///
    /// A tuple containing a `Node` instance and a `NodeEventsChannel`.
    ///
    /// # Errors
    ///
    /// Returns an error if there is a problem initializing the `SwarmDriver`.
    pub async fn run(addr: SocketAddr) -> Result<(Self, NodeEventsChannel)> {
        let (network, mut network_event_receiver, swarm_driver) = SwarmDriver::new(addr)?;
        let storage = DataStorage::new();
        let node_events_channel = NodeEventsChannel::default();
        let node = Self {
            network,
            storage,
            events_channel: node_events_channel.clone(),
        };
        let mut node_clone = node.clone();

        let _handle = spawn(swarm_driver.run());
        let _handle = spawn(async move {
            loop {
                let event = match network_event_receiver.recv().await {
                    Some(event) => event,
                    None => {
                        error!("The `NetworkEvent` channel has been closed");
                        continue;
                    }
                };
                if let Err(err) = node_clone.handle_network_event(event).await {
                    warn!("Error handling network event: {err}");
                }
            }
        });

        Ok((node, node_events_channel))
    }

    async fn handle_network_event(&mut self, event: NetworkEvent) -> Result<()> {
        match event {
            NetworkEvent::RequestReceived { req, channel } => {
                self.handle_request(req, channel).await?
            }
            NetworkEvent::PeerAdded => {
                self.events_channel.broadcast(NodeEvent::ConnectedToNetwork);
                let target = {
                    let mut rng = rand::thread_rng();
                    XorName::random(&mut rng)
                };

                let network = self.network.clone();
                let _handle = spawn(async move {
                    trace!("Getting closest peers for target {target:?}");
                    let result = network.node_get_closest_peers(target).await;
                    trace!("For target {target:?}, get closest peers {result:?}");
                });
            }
        }

        Ok(())
    }

    async fn handle_request(
        &mut self,
        request: Request,
        response_channel: ResponseChannel<Response>,
    ) -> Result<()> {
        trace!("Handling request: {request:?}");
        match request {
            Request::Cmd(Cmd::Dbc {
                signed_spend,
                source_tx,
            }) => {
                self.add_if_valid(signed_spend, source_tx, response_channel)
                    .await?
            }
            Request::Cmd(cmd) => {
                let resp = self.storage.write(&cmd).await;
                self.send_response(Response::Cmd(resp), response_channel)
                    .await;
            }
            Request::Query(query) => {
                let resp = self.storage.read(&query, User::Anyone).await;
                self.send_response(Response::Query(resp), response_channel)
                    .await;
            }
            Request::Event(event) => {
                match event {
                    Event::DoubleSpendAttempted(a_spend, b_spend) => {
                        self.storage
                            .try_add_double(a_spend.as_ref(), b_spend.as_ref())
                            .await?;
                    }
                };
            }
        }

        Ok(())
    }

    /// This function will validate the parents of the provided spend,
    /// as well as the actual spend.
    /// A response will be sent if a response channel is provided.
    async fn add_if_valid(
        &mut self,
        signed_spend: Box<SignedSpend>,
        source_tx: Box<DbcTransaction>,
        response_channel: ResponseChannel<Response>,
    ) -> Result<()> {
        // Ensure that the provided src tx is the same as the one we have the hash of in the signed spend.
        let provided_src_tx_hash = source_tx.hash();
        let signed_src_tx_hash = signed_spend.src_tx_hash();

        if provided_src_tx_hash != signed_src_tx_hash {
            let error = ProtocolError::SignedSrcTxHashDoesNotMatchProvidedSrcTxHash {
                signed_src_tx_hash,
                provided_src_tx_hash,
            };
            self.send_response(
                Response::Cmd(CmdResponse::Spend(Err(error))),
                response_channel,
            )
            .await;
            return Ok(());
        }

        // First we need to validate the parents of the spend.
        // This also ensures that all parent's dst tx's are the same as the src tx of this spend.
        match self
            .validate_spend_parents(signed_spend.as_ref(), source_tx.as_ref())
            .await
        {
            Ok(()) => (),
            Err(super::Error::Protocol(error)) => {
                // We drop spend attempts with invalid parents,
                // and return an error to the client.
                self.send_response(
                    Response::Cmd(CmdResponse::Spend(Err(error))),
                    response_channel,
                )
                .await;
                return Ok(());
            }
            // This should be unreachable, as we only return protocol errors.
            other => other?,
        };

        let response = match self
            .storage
            .write(&Cmd::Dbc {
                signed_spend,
                source_tx,
            })
            .await
        {
            CmdResponse::Spend(Err(ProtocolError::DoubleSpendAttempt { new, existing })) => {
                warn!("Double spend attempted! New: {new:?}. Existing:  {existing:?}");

                let request =
                    Request::Event(Event::double_spend_attempt(new.clone(), existing.clone())?);
                let _resp = self.send_to_closest(&request).await?;

                CmdResponse::Spend(Err(ProtocolError::DoubleSpendAttempt { new, existing }))
            }
            other => other,
        };

        self.send_response(Response::Cmd(response), response_channel)
            .await;

        Ok(())
    }

    /// The src_tx is the tx where the dbc to spend, was created.
    /// The signed_spend.dbc_id() shall exist among its outputs.
    async fn validate_spend_parents(
        &self,
        signed_spend: &SignedSpend,
        source_tx: &DbcTransaction,
    ) -> Result<()> {
        // These will be different spends, one for each input that went into
        // creating the above spend passed in to this function.
        let mut all_parent_spends = BTreeSet::new();

        // First we fetch all parent spends from the network.
        // They shall naturally all exist as valid spends for this current
        // spend attempt to be valid.
        for parent_input in &source_tx.inputs {
            let parent_address = dbc_address(&parent_input.dbc_id());
            // This call makes sure we get the same spend from all in the close group.
            // If we receive a spend here, it is assumed to be valid. But we will verify
            // that anyway, in the code right after this for loop.
            let parent_spend_at_close_group = self.get_spend(parent_address).await?;
            // The dst tx of the parent must be the src tx of the spend.
            if signed_spend.src_tx_hash() != parent_spend_at_close_group.dst_tx_hash() {
                return Err(super::Error::Protocol(
                    ProtocolError::SpendSrcTxHashParentTxHashMismatch {
                        signed_src_tx_hash: signed_spend.src_tx_hash(),
                        parent_dst_tx_hash: parent_spend_at_close_group.dst_tx_hash(),
                    },
                ));
            }
            let _ = all_parent_spends.insert(parent_spend_at_close_group);
        }

        // We have gotten all the parent inputs from the network, so the network consider them all valid.
        // But the source tx corresponding to the signed_spend, might not match the parents' details, so that's what we check here.
        let known_parent_blinded_amounts: Vec<_> = all_parent_spends
            .iter()
            .map(|s| s.spend.blinded_amount)
            .collect();
        // Here we check that the spend that is attempted, was created in a valid tx.
        let src_tx_validity = source_tx.verify(&known_parent_blinded_amounts);
        if src_tx_validity.is_err() {
            return Err(super::Error::Protocol(
                ProtocolError::InvalidSourceTxProvided {
                    signed_src_tx_hash: signed_spend.src_tx_hash(),
                    provided_src_tx_hash: source_tx.hash(),
                },
            ));
        }

        // All parents check out.

        Ok(())
    }

    /// Retrieve a `Spend` from the closest peers
    async fn get_spend(&self, address: DbcAddress) -> Result<SignedSpend> {
        let request = Request::Query(Query::GetDbcSpend(address));
        info!("Getting the closest peers to {:?}", request.dst());

        let responses = self.send_to_closest(&request).await?;

        // Get all Ok results of the expected response type `GetDbcSpend`.
        let spends: Vec<_> = responses
            .iter()
            .flatten()
            .flat_map(|resp| {
                if let Response::Query(QueryResponse::GetDbcSpend(Ok(signed_spend))) = resp {
                    Some(signed_spend.clone())
                } else {
                    None
                }
            })
            .collect();

        if spends.len() >= CLOSE_GROUP_SIZE {
            // All nodes in the close group returned an Ok response.
            let spends: BTreeSet<_> = spends.into_iter().collect();
            // All nodes in the close group returned
            // the same spend. It is thus valid.
            if spends.len() == 1 {
                return Ok(spends
                    .first()
                    .expect("This will contain a single item, due to the check before this.")
                    .clone());
            }
            // Different spends returned, the parent is not valid.
        }

        // The parent is not recognised by all peers in its close group.
        // Thus, the parent is not valid.
        info!("The spend could not be verified as valid: {address:?}");

        // If not enough spends were gotten, we try error the first
        // error to the expected query returned from nodes.
        for resp in responses.iter().flatten() {
            if let Response::Query(QueryResponse::GetDbcSpend(result)) = resp {
                let _ = result.clone()?;
            };
        }

        // If there were no success or fail to the expected query,
        // we check if there were any send errors.
        for resp in responses {
            let _ = resp?;
        }

        // If there was none of the above, then we had unexpected responses.
        Err(super::Error::Protocol(ProtocolError::UnexpectedResponses))
    }

    async fn send_response(&mut self, resp: Response, response_channel: ResponseChannel<Response>) {
        if let Err(err) = self.network.send_response(resp, response_channel).await {
            warn!("Error while sending response: {err:?}");
        }
    }

    async fn send_to_closest(&self, request: &Request) -> Result<Vec<Result<Response>>> {
        info!("Sending {:?} to the closest peers.", request.dst());
        // todo: if `self` is present among the closest peers, the request should be routed to self?
        let closest_peers = self
            .network
            .node_get_closest_peers(*request.dst().name())
            .await?;

        Ok(self
            .send_and_get_responses(closest_peers, request, true)
            .await)
    }

    // Send a `Request` to the provided set of peers and wait for their responses concurrently.
    // If `get_all_responses` is true, we wait for the responses from all the peers. Will return an
    // error if the request timeouts.
    // If `get_all_responses` is false, we return the first successful response that we get
    async fn send_and_get_responses(
        &self,
        peers: Vec<PeerId>,
        req: &Request,
        get_all_responses: bool,
    ) -> Vec<Result<Response>> {
        let mut list_of_futures = Vec::new();
        for peer in peers {
            let future = Box::pin(tokio::time::timeout(
                Duration::from_secs(10),
                self.network.send_request(req.clone(), peer),
            ));
            list_of_futures.push(future);
        }

        let mut responses = Vec::new();
        while !list_of_futures.is_empty() {
            match select_all(list_of_futures).await {
                (Ok(res), _, remaining_futures) => {
                    let res = res.map_err(super::Error::Network);
                    info!("Got response for the req: {req:?}, res: {res:?}");
                    // return the first successful response
                    if !get_all_responses && res.is_ok() {
                        return vec![res];
                    }
                    responses.push(res);
                    list_of_futures = remaining_futures;
                }
                (Err(timeout_err), _, remaining_futures) => {
                    responses.push(Err(super::Error::ResponseTimeout(timeout_err)));
                    list_of_futures = remaining_futures;
                }
            }
        }

        responses
    }
}
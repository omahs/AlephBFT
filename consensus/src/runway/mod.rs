use crate::{
    alerts::{self, Alert, AlertConfig, AlertMessage, ForkProof, ForkingNotification},
    consensus,
    member::UnitMessage,
    units::{
        ControlHash, FullUnit, PreUnit, SignedUnit, UncheckedSignedUnit, Unit, UnitCoord,
        UnitStore, UnitStoreStatus, Validator,
    },
    Config, Data, DataProvider, FinalizationHandler, Hasher, Index, MultiKeychain, NodeCount,
    NodeIndex, NodeMap, Receiver, Recipient, Round, Sender, SessionId,
    Signature, Signed, SpawnHandle, Terminator, UncheckedSigned,
};
use futures::{
    channel::{mpsc, oneshot},
    future::FusedFuture,
    pin_mut, Future, FutureExt, StreamExt,
};
use futures_timer::Delay;
use log::{debug, error, info, trace, warn};
use std::{
    collections::HashSet,
    convert::TryFrom,
    fmt,
    io::{Read, Write},
    marker::PhantomData,
    time::Duration,
};

mod backup;
mod collection;

use backup::{UnitLoader, UnitSaver};
#[cfg(feature = "initial_unit_collection")]
use collection::{Collection, IO as CollectionIO};
pub use collection::{NewestUnitResponse, Salt};

/// Type for incoming notifications: Runway to Consensus.
#[derive(Clone, Eq, PartialEq)]
pub(crate) enum NotificationIn<H: Hasher> {
    /// A notification carrying units. This might come either from multicast or
    /// from a response to a request. This is of no importance at this layer.
    NewUnits(Vec<Unit<H>>),
    /// Response to a request to decode parents when the control hash is wrong.
    UnitParents(H::Hash, Vec<H::Hash>),
}

/// Type for outgoing notifications: Consensus to Runway.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum NotificationOut<H: Hasher> {
    /// Notification about a preunit created by this Consensus Node. Member is meant to
    /// disseminate this preunit among other nodes.
    CreatedPreUnit(PreUnit<H>, Vec<H::Hash>),
    /// Notification that some units are needed but missing. The role of the Member
    /// is to fetch these unit (somehow).
    MissingUnits(Vec<UnitCoord>),
    /// Notification that Consensus has parents incompatible with the control hash.
    WrongControlHash(H::Hash),
    /// Notification that a new unit has been added to the DAG, list of decoded parents provided
    AddedToDag(H::Hash, Vec<H::Hash>),
}

/// Possible requests for information from other nodes.
pub enum Request<H: Hasher> {
    Coord(UnitCoord),
    Parents(H::Hash),
    NewestUnit(Salt),
}

pub(crate) enum Response<H: Hasher, D: Data, S: Signature> {
    Coord(UncheckedSignedUnit<H, D, S>),
    Parents(H::Hash, Vec<UncheckedSignedUnit<H, D, S>>),
    NewestUnit(UncheckedSigned<NewestUnitResponse<H, D, S>, S>),
}

pub(crate) enum RunwayNotificationOut<H: Hasher, D: Data, S: Signature> {
    NewUnit(UncheckedSignedUnit<H, D, S>),
    Request(Request<H>, Recipient),
    Response(Response<H, D, S>, NodeIndex),
}

pub(crate) enum RunwayNotificationIn<H: Hasher, D: Data, S: Signature> {
    NewUnit(UncheckedSignedUnit<H, D, S>),
    Request(Request<H>, NodeIndex),
    Response(Response<H, D, S>),
}

impl<H: Hasher, D: Data, S: Signature> TryFrom<UnitMessage<H, D, S>>
    for RunwayNotificationIn<H, D, S>
{
    type Error = ();

    fn try_from(message: UnitMessage<H, D, S>) -> Result<Self, Self::Error> {
        let result = match message {
            UnitMessage::NewUnit(u) => RunwayNotificationIn::NewUnit(u),
            UnitMessage::RequestCoord(node_id, coord) => {
                RunwayNotificationIn::Request(Request::Coord(coord), node_id)
            }
            UnitMessage::RequestParents(node_id, u_hash) => {
                RunwayNotificationIn::Request(Request::Parents(u_hash), node_id)
            }
            UnitMessage::ResponseCoord(u) => RunwayNotificationIn::Response(Response::Coord(u)),
            UnitMessage::ResponseParents(u_hash, parents) => {
                RunwayNotificationIn::Response(Response::Parents(u_hash, parents))
            }
            UnitMessage::RequestNewest(node_id, salt) => {
                RunwayNotificationIn::Request(Request::NewestUnit(salt), node_id)
            }
            UnitMessage::ResponseNewest(response) => {
                RunwayNotificationIn::Response(Response::NewestUnit(response))
            }
        };
        Ok(result)
    }
}

struct Runway<'a, H, D, US, DP, FH, MK>
where
    H: Hasher,
    D: Data,
    US: Write,
    DP: DataProvider<D>,
    FH: FinalizationHandler<D>,
    MK: MultiKeychain,
{
    missing_coords: HashSet<UnitCoord>,
    missing_parents: HashSet<H::Hash>,
    session_id: SessionId,
    store: UnitStore<'a, H, D, MK>,
    keychain: &'a MK,
    validator: &'a Validator<'a, MK>,
    alerts_for_alerter: Sender<Alert<H, D, MK::Signature>>,
    notifications_from_alerter: Receiver<ForkingNotification<H, D, MK::Signature>>,
    unit_messages_from_network: Receiver<RunwayNotificationIn<H, D, MK::Signature>>,
    unit_messages_for_network: Sender<RunwayNotificationOut<H, D, MK::Signature>>,
    responses_for_collection:
        Sender<UncheckedSigned<NewestUnitResponse<H, D, MK::Signature>, MK::Signature>>,
    resolved_requests: Sender<Request<H>>,
    tx_consensus: Sender<NotificationIn<H>>,
    rx_consensus: Receiver<NotificationOut<H>>,
    ordered_batch_rx: Receiver<Vec<H::Hash>>,
    data_provider: DP,
    finalization_handler: FH,
    unit_saver: UnitSaver<US, H, D, MK::Signature>,
    exiting: bool,
}

struct RunwayStatus<'a, H: Hasher> {
    status: UnitStoreStatus<'a>,
    missing_coords: &'a HashSet<UnitCoord>,
    missing_parents: &'a HashSet<H::Hash>,
}

impl<'a, H: Hasher> RunwayStatus<'a, H> {
    fn new(
        status: UnitStoreStatus<'a>,
        missing_coords: &'a HashSet<UnitCoord>,
        missing_parents: &'a HashSet<H::Hash>,
    ) -> Self {
        Self {
            status,
            missing_coords,
            missing_parents,
        }
    }
}

impl<'a, H: Hasher> fmt::Display for RunwayStatus<'a, H> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Runway status report: ")?;
        write!(f, "{}", self.status)?;
        if !self.missing_coords.is_empty() {
            let mut v_coords: Vec<(usize, Round)> = self
                .missing_coords
                .iter()
                .map(|uc| (uc.creator().into(), uc.round()))
                .collect();
            v_coords.sort();
            write!(f, "; missing coords - {:?}", v_coords)?;
        }
        if !self.missing_parents.is_empty() {
            write!(f, "; missing parents - {:?}", self.missing_parents)?;
        }
        write!(f, ".")?;
        Ok(())
    }
}

struct RunwayConfig<
    H: Hasher,
    D: Data,
    US: Write,
    DP: DataProvider<D>,
    FH: FinalizationHandler<D>,
    MK: MultiKeychain,
> {
    session_id: SessionId,
    max_round: Round,
    data_provider: DP,
    finalization_handler: FH,
    unit_saver: UnitSaver<US, H, D, MK::Signature>,
    alerts_for_alerter: Sender<Alert<H, D, MK::Signature>>,
    notifications_from_alerter: Receiver<ForkingNotification<H, D, MK::Signature>>,
    tx_consensus: Sender<NotificationIn<H>>,
    rx_consensus: Receiver<NotificationOut<H>>,
    unit_messages_from_network: Receiver<RunwayNotificationIn<H, D, MK::Signature>>,
    unit_messages_for_network: Sender<RunwayNotificationOut<H, D, MK::Signature>>,
    responses_for_collection:
        Sender<UncheckedSigned<NewestUnitResponse<H, D, MK::Signature>, MK::Signature>>,
    ordered_batch_rx: Receiver<Vec<H::Hash>>,
    resolved_requests: Sender<Request<H>>,
}

impl<'a, H, D, US, DP, FH, MK> Runway<'a, H, D, US, DP, FH, MK>
where
    H: Hasher,
    D: Data,
    US: Write,
    DP: DataProvider<D>,
    FH: FinalizationHandler<D>,
    MK: MultiKeychain,
{
    fn new(
        config: RunwayConfig<H, D, US, DP, FH, MK>,
        keychain: &'a MK,
        validator: &'a Validator<'a, MK>,
    ) -> Self {
        let n_members = keychain.node_count();
        let RunwayConfig {
            session_id,
            max_round,
            data_provider,
            finalization_handler,
            unit_saver,
            alerts_for_alerter,
            notifications_from_alerter,
            tx_consensus,
            rx_consensus,
            unit_messages_from_network,
            unit_messages_for_network,
            responses_for_collection,
            ordered_batch_rx,
            resolved_requests,
        } = config;
        let store = UnitStore::new(n_members, max_round);

        Runway {
            store,
            keychain,
            validator,
            missing_coords: HashSet::new(),
            missing_parents: HashSet::new(),
            resolved_requests,
            alerts_for_alerter,
            notifications_from_alerter,
            unit_messages_from_network,
            unit_messages_for_network,
            tx_consensus,
            rx_consensus,
            ordered_batch_rx,
            data_provider,
            finalization_handler,
            unit_saver,
            responses_for_collection,
            session_id,
            exiting: false,
        }
    }

    fn index(&self) -> NodeIndex {
        self.keychain.index()
    }

    fn node_count(&self) -> NodeCount {
        self.keychain.node_count()
    }

    async fn on_unit_message(&mut self, message: RunwayNotificationIn<H, D, MK::Signature>) {
        match message {
            RunwayNotificationIn::NewUnit(u) => {
                trace!(target: "AlephBFT-runway", "{:?} New unit received {:?}.", self.index(), &u);
                self.on_unit_received(u, false)
            }
            RunwayNotificationIn::Request(request, node_id) => match request {
                Request::Coord(coord) => {
                    trace!(target: "AlephBFT-runway", "{:?} Coords request received {:?}.", self.index(), coord);
                    self.on_request_coord(node_id, coord)
                }
                Request::Parents(u_hash) => {
                    trace!(target: "AlephBFT-runway", "{:?} Parents request received {:?}.", self.index(), u_hash);
                    self.on_request_parents(node_id, u_hash)
                }
                Request::NewestUnit(salt) => {
                    trace!(target: "AlephBFT-runway", "{:?} Newest unit request received {:?}.", self.index(), salt);
                    self.on_request_newest(node_id, salt).await
                }
            },
            RunwayNotificationIn::Response(res) => match res {
                Response::Coord(u) => {
                    trace!(target: "AlephBFT-runway", "{:?} Fetch response received {:?}.", self.index(), &u);
                    self.on_unit_received(u, false)
                }
                Response::Parents(u_hash, parents) => {
                    trace!(target: "AlephBFT-runway", "{:?} Response parents received {:?}.", self.index(), u_hash);
                    self.on_parents_response(u_hash, parents)
                }
                Response::NewestUnit(response) => {
                    trace!(target: "AlephBFT-runway", "{:?} Response newest unit received from {:?}.", self.index(), response.index());
                    if self
                        .responses_for_collection
                        .unbounded_send(response)
                        .is_err()
                    {
                        debug!(target: "AlephBFT-runway", "{:?} Could not send response to collection.", self.index())
                    }
                }
            },
        }
    }

    fn on_unit_received(&mut self, uu: UncheckedSignedUnit<H, D, MK::Signature>, alert: bool) {
        match self.validator.validate_unit(uu) {
            Ok(su) => {
                self.resolve_missing_coord(&su.as_signable().coord());
                if alert {
                    // Units from alerts explicitly come from forkers, and we want them anyway.
                    self.store.add_unit(su, true);
                } else {
                    self.add_unit_to_store_unless_fork(su);
                }
            }
            Err(e) => warn!(target: "AlephBFT-member", "Received unit failing validation: {}", e),
        }
    }

    fn resolve_missing_coord(&mut self, coord: &UnitCoord) {
        if self.missing_coords.remove(coord) {
            self.send_resolved_request_notification(Request::Coord(*coord));
        }
    }

    fn add_unit_to_store_unless_fork(&mut self, su: SignedUnit<'a, H, D, MK>) {
        let full_unit = su.as_signable();
        trace!(target: "AlephBFT-member", "{:?} Adding member unit to store {:?}", self.index(), full_unit);
        if self.store.is_forker(full_unit.creator()) {
            trace!(target: "AlephBFT-member", "{:?} Ignoring forker's unit {:?}", self.index(), full_unit);
            return;
        }
        if let Some(sv) = self.store.is_new_fork(full_unit) {
            let creator = full_unit.creator();
            if !self.store.is_forker(creator) {
                // We need to mark the forker if it is not known yet.
                let proof = (su.into(), sv.into());
                self.on_new_forker_detected(creator, proof);
            }
            // We ignore this unit. If it is legit, it will arrive in some alert and we need to wait anyway.
            // There is no point in keeping this unit in any kind of buffer.
            return;
        }
        self.store.add_unit(su, false);
    }

    fn on_new_forker_detected(&mut self, forker: NodeIndex, proof: ForkProof<H, D, MK::Signature>) {
        let alerted_units = self.store.mark_forker(forker);
        let alert = self.form_alert(proof, alerted_units);
        if self.alerts_for_alerter.unbounded_send(alert).is_err() {
            warn!(target: "AlephBFT-runway", "{:?} Channel to alerter should be open", self.index());
            self.exiting = true;
        }
    }

    fn form_alert(
        &self,
        proof: ForkProof<H, D, MK::Signature>,
        units: Vec<SignedUnit<H, D, MK>>,
    ) -> Alert<H, D, MK::Signature> {
        Alert::new(
            self.index(),
            proof,
            units.into_iter().map(|signed| signed.into()).collect(),
        )
    }

    fn on_request_coord(&mut self, node_id: NodeIndex, coord: UnitCoord) {
        debug!(target: "AlephBFT-runway", "{:?} Received fetch request for coord {:?} from {:?}.", self.index(), coord, node_id);
        let maybe_su = (self.store.unit_by_coord(coord)).cloned();

        if let Some(su) = maybe_su {
            trace!(target: "AlephBFT-runway", "{:?} Answering fetch request for coord {:?} from {:?}.", self.index(), coord, node_id);
            self.send_message_for_network(RunwayNotificationOut::Response(
                Response::Coord(su.into()),
                node_id,
            ));
        } else {
            trace!(target: "AlephBFT-runway", "{:?} Not answering fetch request for coord {:?}. Unit not in store.", self.index(), coord);
        }
    }

    fn on_request_parents(&mut self, node_id: NodeIndex, u_hash: H::Hash) {
        debug!(target: "AlephBFT-runway", "{:?} Received parents request for hash {:?} from {:?}.", self.index(), u_hash, node_id);

        if let Some(p_hashes) = self.store.get_parents(u_hash) {
            let p_hashes = p_hashes.clone();
            trace!(target: "AlephBFT-runway", "{:?} Answering parents request for hash {:?} from {:?}.", self.index(), u_hash, node_id);
            let mut full_units = Vec::new();
            for hash in p_hashes.iter() {
                if let Some(fu) = self.store.unit_by_hash(hash) {
                    full_units.push(fu.clone().into());
                } else {
                    debug!(target: "AlephBFT-runway", "{:?} Not answering parents request, one of the parents missing from store.", self.index());
                    //This can happen if we got a parents response from someone, but one of the units was a fork and we dropped it.
                    //Either this parent is legit and we will soon get it in alert or the parent is not legit in which case
                    //the unit u, whose parents are beeing seeked here is not legit either.
                    //In any case, if a node added u to its Dag, then it should never reach this place in code when answering
                    //a parents request (as all the parents must be legit an thus must be in store).
                    return;
                }
            }
            self.send_message_for_network(RunwayNotificationOut::Response(
                Response::Parents(u_hash, full_units),
                node_id,
            ));
        } else {
            trace!(target: "AlephBFT-runway", "{:?} Not answering parents request for hash {:?}. Unit not in DAG yet.", self.index(), u_hash);
        }
    }

    async fn on_request_newest(&mut self, requester: NodeIndex, salt: u64) {
        let unit = self.store.newest_unit(requester);
        let response = NewestUnitResponse::new(requester, self.index(), unit, salt);

        let signed_response = Signed::sign(response, self.keychain).await.into_unchecked();

        if let Err(e) =
            self.unit_messages_for_network
                .unbounded_send(RunwayNotificationOut::Response(
                    Response::NewestUnit(signed_response),
                    requester,
                ))
        {
            error!(target: "AlephBFT-runway", "Unable to send response to network: {}", e);
        }
    }

    fn on_parents_response(
        &mut self,
        u_hash: H::Hash,
        parents: Vec<UncheckedSignedUnit<H, D, MK::Signature>>,
    ) {
        if self.store.get_parents(u_hash).is_some() {
            trace!(target: "AlephBFT-runway", "{:?} We got parents response but already know the parents.", self.index());
            return;
        }
        let (u_round, u_control_hash, parent_ids) = match self.store.unit_by_hash(&u_hash) {
            Some(su) => {
                let full_unit = su.as_signable();
                let parent_ids: Vec<_> = full_unit.control_hash().parents().collect();
                (
                    full_unit.round(),
                    full_unit.control_hash().combined_hash,
                    parent_ids,
                )
            }
            None => {
                trace!(target: "AlephBFT-runway", "{:?} We got parents but don't even know the unit. Ignoring.", self.index());
                return;
            }
        };

        if parent_ids.len() != parents.len() {
            warn!(target: "AlephBFT-runway", "{:?} In received parent response expected {} parents got {} for unit {:?}.", self.index(), parents.len(), parent_ids.len(), u_hash);
            return;
        }

        let mut p_hashes_node_map = NodeMap::with_size(self.node_count());
        for (i, uu) in parents.into_iter().enumerate() {
            let su = match self.validator.validate_unit(uu) {
                Ok(su) => su,
                Err(e) => {
                    warn!(target: "AlephBFT-runway", "{:?} In received parent response received a unit that does not pass validation: {}", self.index(), e);
                    return;
                }
            };
            let full_unit = su.as_signable();
            if full_unit.round() + 1 != u_round {
                warn!(target: "AlephBFT-runway", "{:?} In received parent response received a unit with wrong round.", self.index());
                return;
            }
            if full_unit.creator() != parent_ids[i] {
                warn!(target: "AlephBFT-runway", "{:?} In received parent response received a unit with wrong creator.", self.index());
                return;
            }
            let p_hash = full_unit.hash();
            let ix = full_unit.creator();
            p_hashes_node_map.insert(ix, p_hash);
            // There might be some optimization possible here to not validate twice, but overall
            // this piece of code should be executed extremely rarely.
            self.resolve_missing_coord(&su.as_signable().coord());
            self.add_unit_to_store_unless_fork(su);
        }

        if ControlHash::<H>::combine_hashes(&p_hashes_node_map) != u_control_hash {
            warn!(target: "AlephBFT-runway", "{:?} In received parent response the control hash is incorrect {:?}.", self.index(), p_hashes_node_map);
            return;
        }
        let p_hashes: Vec<_> = p_hashes_node_map.into_values().collect();
        self.store.add_parents(u_hash, p_hashes.clone());
        trace!(target: "AlephBFT-runway", "{:?} Succesful parents response for {:?}.", self.index(), u_hash);
        self.send_consensus_notification(NotificationIn::UnitParents(u_hash, p_hashes));
    }

    fn resolve_missing_parents(&mut self, u_hash: &H::Hash) {
        if self.missing_parents.remove(u_hash) {
            self.send_resolved_request_notification(Request::Parents(*u_hash));
        }
    }

    fn save_unit(&mut self, uu: UncheckedSignedUnit<H, D, MK::Signature>) {
        let h = uu.as_signable().hash();
        trace!(target: "AlephBFT-runway", "{:?} Saving a created unit {:?}.", self.index(), h);
        if let Err(err) = self.unit_saver.save(uu) {
            error!(target: "AlephBFT-runway", "{:?} Failed to save unit {:?}. {}", self.index(), h, err);
        } else {
            trace!(target: "AlephBFT-runway", "{:?} Saved a created unit {:?}.", self.index(), h);
        }
    }

    async fn on_create(&mut self, u: PreUnit<H>) {
        debug!(target: "AlephBFT-runway", "{:?} On create notification.", self.index());
        let data = self.data_provider.get_data().await;
        let full_unit = FullUnit::new(u, data, self.session_id);
        let signed_unit = Signed::sign(full_unit, self.keychain).await;
        self.save_unit(signed_unit.clone().into());
        self.store.add_unit(signed_unit, false);
    }

    fn on_alert_notification(&mut self, notification: ForkingNotification<H, D, MK::Signature>) {
        use ForkingNotification::*;
        match notification {
            Forker(proof) => {
                let forker = proof.0.index();
                if !self.store.is_forker(forker) {
                    self.on_new_forker_detected(forker, proof);
                }
            }
            Units(units) => {
                for uu in units {
                    self.on_unit_received(uu, true);
                }
            }
        }
    }

    async fn on_consensus_notification(&mut self, notification: NotificationOut<H>) {
        match notification {
            NotificationOut::CreatedPreUnit(pu, _) => {
                self.on_create(pu).await;
            }
            NotificationOut::MissingUnits(coords) => {
                self.on_missing_coords(coords);
            }
            NotificationOut::WrongControlHash(h) => {
                self.on_wrong_control_hash(h);
            }
            NotificationOut::AddedToDag(h, p_hashes) => {
                self.store.add_parents(h, p_hashes);
                self.resolve_missing_parents(&h);
                if let Some(su) = self.store.unit_by_hash(&h).cloned() {
                    if su.as_signable().creator() == self.index() {
                        trace!(target: "AlephBFT-runway", "{:?} Sending a unit {:?}.", self.index(), h);
                        self.send_message_for_network(RunwayNotificationOut::NewUnit(su.into()));
                    }
                } else {
                    error!(target: "AlephBFT-runway", "{:?} A unit already added to DAG is not in our store: {:?}.", self.index(), h);
                }
            }
        }
    }

    fn on_missing_coords(&mut self, mut coords: Vec<UnitCoord>) {
        trace!(target: "AlephBFT-runway", "{:?} Dealing with missing coords notification {:?}.", self.index(), coords);
        coords.retain(|coord| !self.store.contains_coord(coord));
        for coord in coords {
            if self.missing_coords.insert(coord) {
                self.send_message_for_network(RunwayNotificationOut::Request(
                    Request::Coord(coord),
                    Recipient::Node(coord.creator()),
                ));
            }
        }
    }

    fn on_wrong_control_hash(&mut self, u_hash: H::Hash) {
        trace!(target: "AlephBFT-runway", "{:?} Dealing with wrong control hash notification {:?}.", self.index(), u_hash);
        if let Some(p_hashes) = self.store.get_parents(u_hash) {
            // We have the parents by some strange reason (someone sent us parents
            // without us requesting them).
            let p_hashes = p_hashes.clone();
            trace!(target: "AlephBFT-runway", "{:?} We have the parents for {:?} even though we did not request them.", self.index(), u_hash);
            let notification = NotificationIn::UnitParents(u_hash, p_hashes);
            self.send_consensus_notification(notification);
        } else {
            let node_id = self
                .store
                .unit_by_hash(&u_hash)
                .map(|u| u.as_signable().creator());
            let recipient = if let Some(node_id) = node_id {
                Recipient::Node(node_id)
            } else {
                Recipient::Everyone
            };
            if self.missing_parents.insert(u_hash) {
                self.send_message_for_network(RunwayNotificationOut::Request(
                    Request::Parents(u_hash),
                    recipient,
                ));
            }
        }
    }

    async fn on_ordered_batch(&mut self, batch: Vec<H::Hash>) {
        let data_iter: Vec<_> = batch
            .iter()
            .map(|h| {
                self.store
                    .unit_by_hash(h)
                    .expect("Ordered units must be in store")
                    .as_signable()
                    .data()
                    .clone()
            })
            .collect();

        for d in data_iter {
            self.finalization_handler.data_finalized(d).await;
        }
    }

    fn send_message_for_network(
        &mut self,
        notification: RunwayNotificationOut<H, D, MK::Signature>,
    ) {
        if self
            .unit_messages_for_network
            .unbounded_send(notification)
            .is_err()
        {
            warn!(target: "AlephBFT-runway", "{:?} unit_messages_for_network channel should be open", self.index());
            self.exiting = true;
        }
    }

    fn send_resolved_request_notification(&mut self, notification: Request<H>) {
        if self.resolved_requests.unbounded_send(notification).is_err() {
            warn!(target: "AlephBFT-runway", "{:?} resolved_requests channel should be open", self.index());
            self.exiting = true;
        }
    }

    fn send_consensus_notification(&mut self, notification: NotificationIn<H>) {
        if self.tx_consensus.unbounded_send(notification).is_err() {
            warn!(target: "AlephBFT-runway", "{:?} Channel to consensus should be open", self.index());
            self.exiting = true;
        }
    }

    fn move_units_to_consensus(&mut self) {
        let units_to_move = self
            .store
            .yield_buffer_units()
            .into_iter()
            .map(|su| su.as_signable().unit())
            .collect();
        self.send_consensus_notification(NotificationIn::NewUnits(units_to_move))
    }

    fn status_report(&self) {
        let runway_status: RunwayStatus<H> = RunwayStatus::new(
            self.store.get_status(),
            &self.missing_coords,
            &self.missing_parents,
        );
        info!(target: "AlephBFT-runway", "{}", runway_status);
    }

    async fn run(
        mut self,
        units_from_backup: oneshot::Receiver<Vec<UncheckedSignedUnit<H, D, MK::Signature>>>,
        mut terminator: Terminator,
    ) {
        let index = self.index();
        let units_from_backup = units_from_backup.fuse();
        pin_mut!(units_from_backup);

        let status_ticker_delay = Duration::from_secs(10);
        let mut status_ticker = Delay::new(status_ticker_delay).fuse();

        info!(target: "AlephBFT-runway", "{:?} Runway started.", index);
        loop {
            futures::select! {
                notification = self.rx_consensus.next() => match notification {
                        Some(notification) => self.on_consensus_notification(notification).await,
                        None => {
                            error!(target: "AlephBFT-runway", "{:?} Consensus notification stream closed.", index);
                            break;
                        }
                },

                notification = self.notifications_from_alerter.next() => match notification {
                    Some(notification) => {
                        self.on_alert_notification(notification)
                    },
                    None => {
                        error!(target: "AlephBFT-runway", "{:?} Alert notification stream closed.", index);
                        break;
                    }
                },

                event = self.unit_messages_from_network.next() => match event {
                    Some(event) => self.on_unit_message(event).await,
                    None => {
                        error!(target: "AlephBFT-runway", "{:?} Unit message stream closed.", index);
                        break;
                    }
                },

                message = units_from_backup => match message {
                    Ok(units) => {
                        for u in units {
                            self.on_unit_received(u, false);
                        }
                    },
                    Err(e) => {
                        error!(target: "AlephBFT-runway", "{:?} Units message from backup channel closed: {:?}", index, e);
                        break;
                    },
                },

                batch = self.ordered_batch_rx.next() => match batch {
                    Some(batch) => self.on_ordered_batch(batch).await,
                    None => {
                        error!(target: "AlephBFT-runway", "{:?} Ordered batch stream closed.", index);
                        break;
                    }
                },

                _ = &mut status_ticker => {
                    self.status_report();
                    status_ticker = Delay::new(status_ticker_delay).fuse();
                },

                _ = &mut terminator.get_exit() => {
                    info!(target: "AlephBFT-runway", "{:?} received exit signal", index);
                    self.exiting = true;
                }
            };
            self.move_units_to_consensus();

            if self.exiting {
                info!(target: "AlephBFT-runway", "{:?} Runway decided to exit.", index);
                terminator.terminate_sync().await;
                break;
            }
        }

        info!(target: "AlephBFT-runway", "{:?} Run ended.", index);
    }
}

pub(crate) struct NetworkIO<H: Hasher, D: Data, MK: MultiKeychain> {
    pub(crate) alert_messages_for_network: Sender<(
        AlertMessage<H, D, MK::Signature, MK::PartialMultisignature>,
        Recipient,
    )>,
    pub(crate) alert_messages_from_network:
        Receiver<AlertMessage<H, D, MK::Signature, MK::PartialMultisignature>>,
    pub(crate) unit_messages_for_network: Sender<RunwayNotificationOut<H, D, MK::Signature>>,
    pub(crate) unit_messages_from_network: Receiver<RunwayNotificationIn<H, D, MK::Signature>>,
    pub(crate) resolved_requests: Sender<Request<H>>,
}

#[cfg(feature = "initial_unit_collection")]
fn initial_unit_collection<'a, H: Hasher, D: Data, MK: MultiKeychain>(
    keychain: &'a MK,
    validator: &'a Validator<'a, MK>,
    threshold: NodeCount,
    unit_messages_for_network: &Sender<RunwayNotificationOut<H, D, MK::Signature>>,
    unit_collection_sender: oneshot::Sender<Round>,
    responses_from_runway: Receiver<
        UncheckedSigned<NewestUnitResponse<H, D, MK::Signature>, MK::Signature>,
    >,
    resolved_requests: Sender<Request<H>>,
) -> Result<impl Future<Output = ()> + 'a, ()> {
    let (collection, salt) = Collection::new(keychain, validator, threshold);
    let notification =
        RunwayNotificationOut::Request(Request::NewestUnit(salt), Recipient::Everyone);
    if let Err(e) = unit_messages_for_network.unbounded_send(notification) {
        error!(target: "AlephBFT-runway", "Unable to send the newest unit request: {}", e);
        return Err(());
    };
    let collection = CollectionIO::new(
        unit_collection_sender,
        responses_from_runway,
        resolved_requests,
        collection,
    );
    Ok(collection.run())
}

#[cfg(not(feature = "initial_unit_collection"))]
fn trivial_start(
    starting_round_sender: oneshot::Sender<Round>,
) -> Result<impl Future<Output = ()>, ()> {
    if let Err(e) = starting_round_sender.send(0) {
        error!(target: "AlephBFT-runway", "Unable to send the starting round: {}", e);
        return Err(());
    }
    Ok(async {})
}

pub struct RunwayIO<
    H: Hasher,
    D: Data,
    S: Signature,
    US: Write,
    UL: Read,
    DP: DataProvider<D>,
    FH: FinalizationHandler<D>,
> {
    pub data_provider: DP,
    pub finalization_handler: FH,
    pub unit_saver: UnitSaver<US, H, D, S>,
    pub unit_loader: UnitLoader<UL, H, D, S>,
    _phantom: PhantomData<(H, D, S)>,
}

impl<
        H: Hasher,
        D: Data,
        S: Signature,
        US: Write,
        UL: Read,
        DP: DataProvider<D>,
        FH: FinalizationHandler<D>,
    > RunwayIO<H, D, S, US, UL, DP, FH>
{
    pub fn new(
        data_provider: DP,
        finalization_handler: FH,
        unit_saver: US,
        unit_loader: UL,
    ) -> Self {
        RunwayIO {
            data_provider,
            finalization_handler,
            unit_saver: UnitSaver::new(unit_saver),
            unit_loader: UnitLoader::new(unit_loader),
            _phantom: PhantomData,
        }
    }
}

pub(crate) async fn run<H, D, US, UL, MK, DP, FH, SH>(
    config: Config,
    runway_io: RunwayIO<H, D, MK::Signature, US, UL, DP, FH>,
    keychain: MK,
    spawn_handle: SH,
    network_io: NetworkIO<H, D, MK>,
    mut terminator: Terminator,
) where
    H: Hasher,
    D: Data,
    US: Write,
    UL: Read,
    DP: DataProvider<D>,
    FH: FinalizationHandler<D>,
    MK: MultiKeychain,
    SH: SpawnHandle,
{
    let (tx_consensus, consensus_stream) = mpsc::unbounded();
    let (consensus_sink, rx_consensus) = mpsc::unbounded();
    let (ordered_batch_tx, ordered_batch_rx) = mpsc::unbounded();

    let (alert_notifications_for_units, notifications_from_alerter) = mpsc::unbounded();
    let (alerts_for_alerter, alerts_from_units) = mpsc::unbounded();
    let alert_config = AlertConfig {
        session_id: config.session_id,
        n_members: config.n_members,
    };
    let alerter_terminator = terminator.add_offspring_connection("AlephBFT-alerter");
    let alerter_keychain = keychain.clone();
    let alert_messages_for_network = network_io.alert_messages_for_network;
    let alert_messages_from_network = network_io.alert_messages_from_network;
    let alerter_handle = spawn_handle.spawn_essential("runway/alerter", async move {
        alerts::run(
            alerter_keychain,
            alert_messages_for_network,
            alert_messages_from_network,
            alert_notifications_for_units,
            alerts_from_units,
            alert_config,
            alerter_terminator,
        )
        .await;
    });
    let mut alerter_handle = alerter_handle.fuse();

    let consensus_terminator = terminator.add_offspring_connection("consensus");
    let consensus_config = config.clone();
    let consensus_spawner = spawn_handle.clone();
    let (starting_round_sender, starting_round) = oneshot::channel();

    let consensus_handle = spawn_handle.spawn_essential("runway/consensus", async move {
        consensus::run(
            consensus_config,
            consensus_stream,
            consensus_sink,
            ordered_batch_tx,
            consensus_spawner,
            starting_round,
            consensus_terminator,
        )
        .await
    });
    let mut consensus_handle = consensus_handle.fuse();

    let index = keychain.index();
    let threshold = (keychain.node_count() * 2) / 3 + NodeCount(1);
    let validator = Validator::new(config.session_id, &keychain, config.max_round, threshold);
    let (responses_for_collection, responses_from_runway) = mpsc::unbounded();
    let (unit_collections_sender, unit_collection_result) = oneshot::channel();
    let (loaded_units_tx, loaded_units_rx) = oneshot::channel();

    let backup_loading_handle = backup::run_loading_mechanism(
        runway_io.unit_loader,
        index,
        config.session_id,
        loaded_units_tx,
        starting_round_sender,
        unit_collection_result,
    )
    .fuse();
    pin_mut!(backup_loading_handle);

    #[cfg(feature = "initial_unit_collection")]
    let starting_round_handle = match initial_unit_collection(
        &keychain,
        &validator,
        threshold,
        &network_io.unit_messages_for_network,
        unit_collections_sender,
        responses_from_runway,
        network_io.resolved_requests.clone(),
    ) {
        Ok(handle) => handle.fuse(),
        Err(_) => return,
    };
    #[cfg(not(feature = "initial_unit_collection"))]
    let starting_round_handle = match trivial_start(unit_collections_sender) {
        Ok(handle) => handle.fuse(),
        Err(_) => return,
    };
    pin_mut!(starting_round_handle);

    let RunwayIO {
        data_provider,
        finalization_handler,
        unit_saver,
        ..
    } = runway_io;

    let runway_config = RunwayConfig {
        data_provider,
        finalization_handler,
        unit_saver,
        alerts_for_alerter,
        notifications_from_alerter,
        tx_consensus,
        rx_consensus,
        unit_messages_from_network: network_io.unit_messages_from_network,
        unit_messages_for_network: network_io.unit_messages_for_network,
        ordered_batch_rx,
        responses_for_collection,
        resolved_requests: network_io.resolved_requests,
        session_id: config.session_id,
        max_round: config.max_round,
    };
    let runway_terminator = terminator.add_offspring_connection("runway");
    let runway = Runway::new(runway_config, &keychain, &validator);
    let runway_handle = runway
        .run(loaded_units_rx, runway_terminator)
        .fuse();
    pin_mut!(runway_handle);

    loop {
        futures::select! {
            _ = runway_handle => {
                debug!(target: "AlephBFT-runway", "{:?} Runway task terminated early.", index);
                break;
            },
            _ = alerter_handle => {
                debug!(target: "AlephBFT-runway", "{:?} Alerter task terminated early.", index);
                break;
            },
            _ = consensus_handle => {
                debug!(target: "AlephBFT-runway", "{:?} Consensus task terminated early.", index);
                break;
            },
            _ = starting_round_handle => {
                debug!(target: "AlephBFT-runway", "{:?} Starting round task terminated.", index);
            },
            _ = backup_loading_handle => {
                debug!(target: "AlephBFT-runway", "{:?} Backup loading task terminated.", index);
            },
            _ = &mut terminator.get_exit() => {
                break;
            }
        }
    }

    info!(target: "AlephBFT-runway", "{:?} Ending run.", index);
    let terminator_handle = terminator.terminate_sync().fuse();
    pin_mut!(terminator_handle);

    loop {
        futures::select! {
            _ = runway_handle => {
                debug!(target: "AlephBFT-runway", "{:?} Runway stopped.", index);
            },

            _ = terminator_handle => {},

            complete => break,
        }
    }

    if !consensus_handle.is_terminated() {
        if let Err(()) = consensus_handle.await {
            warn!(target: "AlephBFT-runway", "{:?} Consensus finished with an error", index);
        }
        debug!(target: "AlephBFT-runway", "{:?} Consensus stopped.", index);
    }

    if !alerter_handle.is_terminated() {
        if let Err(()) = alerter_handle.await {
            warn!(target: "AlephBFT-runway", "{:?} Alerter finished with an error", index);
        }
        debug!(target: "AlephBFT-runway", "{:?} Alerter stopped.", index);
    }

    info!(target: "AlephBFT-runway", "{:?} Runway ended.", index);
}

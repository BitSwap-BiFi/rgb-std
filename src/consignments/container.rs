// RGB Standard Library: high-level API to RGB smart contracts.
// Written in 2019-2022 by
//     Dr. Maxim Orlovsky <orlovsky@lnp-bp.org>
//
// To the extent possible under law, the author(s) have dedicated all copyright
// and related and neighboring rights to this software to the public domain
// worldwide. This software is distributed without any warranty.
//
// You should have received a copy of the MIT License along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::{io, slice};

use bitcoin::Txid;
use commit_verify::{commit_encode, lnpbp4, ConsensusCommit};
use rgb_core::{
    schema, AttachmentId, BundleId, Consignment, ConsignmentEndpoint, ConsistencyError, ContractId,
    Extension, Genesis, GraphApi, Node, NodeId, Schema, SchemaId, SealEndpoint, Transition,
    TransitionBundle,
};
use strict_encoding::{LargeVec, StrictDecode};

use super::{AnchoredBundles, ConsignmentEndseals, ConsignmentType, ExtensionList};
use crate::state::StateReducer;
use crate::{Anchor, ConsignmentId, ContractState};

pub const RGB_INMEM_CONSIGNMENT_VERSION: u8 = 0;

// TODO: Refactor internal data store; separate state transitions from bundles
/// Consignment represents contract-specific data, always starting with genesis,
/// which must be valid under client-side-validation rules (i.e. internally
/// consistent and properly committed into the commitment layer, like bitcoin
/// blockchain or current state of the lightning channel).
///
/// All consignments-related procedures, including validation or merging
/// consignments data into stash or schema-specific data storage, must start with
/// `endpoints` and process up to the genesis. If any of the nodes within the
/// consignments are not part of the paths connecting endpoints with the genesis,
/// consignments validation will return
/// [`crate::validation::Warning::ExcessiveNode`] warning
#[cfg_attr(
    all(feature = "cli", feature = "serde"),
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate")
)]
#[derive(Clone, PartialEq, Eq, Debug, StrictEncode)]
pub struct InmemConsignment<T>
where T: ConsignmentType
{
    /// Version, used internally
    version: u8,

    pub(crate) schema: Schema,

    pub(crate) root_schema: Option<Schema>,

    /// Genesis data
    pub(crate) genesis: Genesis,

    /// Set of seals for the state transfer beneficiaries.
    pub(crate) endseals: ConsignmentEndseals,

    /// Data on all anchored state transitions contained in the consignments
    pub(crate) anchored_bundles: AnchoredBundles,

    /// Data on all state extensions contained in the consignments
    pub(crate) state_extensions: ExtensionList,

    /// Data containers coming with this consignment. For the purposes of
    /// in-memory consignments we are restricting the size of the containers to
    /// 24 bit value (RGB allows containers up to 32-bit values in size).
    pub(crate) data_containers: BTreeMap<AttachmentId, LargeVec<u8>>,

    #[strict_encoding(skip)]
    _phantom: PhantomData<T>,
}

// TODO: Switch to "UsingConceal" strategy
impl<T> commit_encode::Strategy for InmemConsignment<T>
where T: ConsignmentType
{
    type Strategy = commit_encode::strategies::UsingStrict;
}

impl<T> ConsensusCommit for InmemConsignment<T>
where T: ConsignmentType
{
    type Commitment = ConsignmentId;
}

impl<T> StrictDecode for InmemConsignment<T>
where T: ConsignmentType
{
    fn strict_decode<D: io::Read>(mut d: D) -> Result<Self, strict_encoding::Error> {
        let consignment = Self {
            version: StrictDecode::strict_decode(&mut d)?,
            schema: StrictDecode::strict_decode(&mut d)?,
            root_schema: StrictDecode::strict_decode(&mut d)?,
            genesis: StrictDecode::strict_decode(&mut d)?,
            endseals: StrictDecode::strict_decode(&mut d)?,
            anchored_bundles: StrictDecode::strict_decode(&mut d)?,
            state_extensions: StrictDecode::strict_decode(&mut d)?,
            data_containers: StrictDecode::strict_decode(&mut d)?,
            _phantom: none!(),
        };
        if consignment.version != RGB_INMEM_CONSIGNMENT_VERSION {
            return Err(strict_encoding::Error::UnsupportedDataStructure(
                "State transfer versions above 0 are not supported",
            ));
        }
        Ok(consignment)
    }
}

impl<'consignment, T> Consignment<'consignment> for InmemConsignment<T>
where
    Self: 'consignment,
    T: ConsignmentType,
{
    type EndpointIter = slice::Iter<'consignment, ConsignmentEndpoint>;
    type BundleIter = slice::Iter<'consignment, (Anchor<lnpbp4::MerkleProof>, TransitionBundle)>;
    type ExtensionsIter = slice::Iter<'consignment, Extension>;

    fn schema(&'consignment self) -> &'consignment Schema { &self.schema }

    fn root_schema(&'consignment self) -> Option<&'consignment Schema> { self.root_schema.as_ref() }

    fn genesis(&'consignment self) -> &'consignment Genesis { &self.genesis }

    fn node_ids(&'consignment self) -> BTreeSet<NodeId> {
        // TODO: Implement node id cache with making all fields private
        let mut set = bset![self.genesis.node_id()];
        set.extend(
            self.anchored_bundles
                .iter()
                .flat_map(|(_, bundle)| bundle.known_node_ids()),
        );
        set.extend(self.state_extensions.iter().map(Extension::node_id));
        set
    }

    fn endpoints(&'consignment self) -> Self::EndpointIter { self.endseals.iter() }

    fn anchored_bundles(&'consignment self) -> Self::BundleIter { self.anchored_bundles.iter() }

    fn state_extensions(&'consignment self) -> Self::ExtensionsIter { self.state_extensions.iter() }
}

impl<T> InmemConsignment<T>
where T: ConsignmentType
{
    #[inline]
    pub fn with(
        schema: Schema,
        root_schema: Option<Schema>,
        genesis: Genesis,
        endseals: ConsignmentEndseals,
        anchored_bundles: AnchoredBundles,
        state_extensions: ExtensionList,
    ) -> Self {
        Self {
            version: RGB_INMEM_CONSIGNMENT_VERSION,
            schema,
            root_schema,
            genesis,
            endseals,
            state_extensions,
            anchored_bundles,
            data_containers: none!(),
            _phantom: none!(),
        }
    }

    #[inline]
    pub fn id(&self) -> ConsignmentId { self.clone().consensus_commit() }

    #[inline]
    pub fn schema_id(&self) -> SchemaId { self.schema.schema_id() }

    #[inline]
    pub fn root_schema_id(&self) -> Option<SchemaId> {
        self.root_schema.as_ref().map(Schema::schema_id)
    }

    #[inline]
    pub fn contract_id(&self) -> ContractId { self.genesis.contract_id() }

    #[inline]
    pub fn version(&self) -> u8 { self.version }

    pub fn txids(&self) -> BTreeSet<Txid> {
        self.anchored_bundles
            .iter()
            .map(|(anchor, _)| anchor.txid)
            .collect()
    }

    pub fn endpoint_bundle_ids(&self) -> BTreeSet<BundleId> {
        self.endseals
            .iter()
            .map(|(bundle_id, _)| bundle_id)
            .copied()
            .collect()
    }

    pub fn endpoint_bundles(&self) -> Vec<&TransitionBundle> {
        self.endpoint_bundle_ids()
            .into_iter()
            .filter_map(|bundle_id| self.bundle_by_id(bundle_id).ok())
            .collect()
    }

    pub fn endpoint_transition_by_id(
        &self,
        node_id: NodeId,
    ) -> Result<&Transition, ConsistencyError> {
        if self
            .endseals
            .iter()
            .filter_map(|(id, _)| self.bundle_by_id(*id).ok())
            .flat_map(|bundle| bundle.known_node_ids())
            .any(|id| id == node_id)
        {
            return Err(ConsistencyError::NotEndpoint(node_id));
        }

        self.transition_by_id(node_id)
    }

    pub fn endpoint_transitions_by_type(
        &self,
        transition_type: schema::TransitionType,
    ) -> Vec<&Transition> {
        self.endpoint_transitions_by_types(&[transition_type])
    }

    pub fn endpoint_transitions_by_types(
        &self,
        types: &[schema::TransitionType],
    ) -> Vec<&Transition> {
        self.endpoint_bundle_ids()
            .into_iter()
            .filter_map(|bundle_id| self.known_transitions_by_bundle_id(bundle_id).ok())
            .flat_map(Vec::into_iter)
            .filter(|node| types.contains(&node.transition_type()))
            .collect()
    }

    pub fn push_anchored_bundle(
        &mut self,
        anchor: Anchor<lnpbp4::MerkleProof>,
        bundle: TransitionBundle,
    ) -> Result<usize, strict_encoding::Error> {
        self.anchored_bundles.push((anchor, bundle))
    }

    pub fn push_seal_endpoint(&mut self, bundle_id: BundleId, endseal: SealEndpoint) {
        self.endseals.push((bundle_id, endseal));
    }
}

impl ContractState {
    pub fn with<T>(consignment: &InmemConsignment<T>, reducer: StateReducer) -> Self
    where T: ConsignmentType {
        let genesis = consignment.genesis();
        let contract_id = consignment.contract_id();

        let mut state = ContractState::new(
            consignment.schema_id(),
            consignment.root_schema_id(),
            reducer,
            contract_id,
            genesis,
        );
        for (anchor, bundle) in consignment.anchored_bundles() {
            let witness_txid = anchor.txid;
            for (transition, _) in bundle.revealed_iter() {
                state.add_transition(witness_txid, transition);
            }
        }
        for extension in consignment.state_extensions() {
            state.add_extension(extension);
        }

        state
    }
}

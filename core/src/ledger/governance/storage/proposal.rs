use std::collections::{BTreeMap, HashSet};
use std::fmt::Display;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ibc::core::host::types::identifiers::{ChannelId, PortId};
use crate::ledger::governance::cli::onchain::{
    PgfAction, PgfContinuous, PgfRetro, PgfSteward, StewardsUpdate,
};
use crate::ledger::governance::utils::{ProposalStatus, TallyType};
use crate::ledger::storage_api::token::Amount;
use crate::types::address::Address;
use crate::types::hash::Hash;
use crate::types::storage::Epoch;

#[allow(missing_docs)]
#[derive(Debug, Error)]
pub enum ProposalTypeError {
    #[error("Invalid proposal type.")]
    InvalidProposalType,
}

/// Storage structure for pgf funding
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct StoragePgfFunding {
    /// The data about the pgf funding
    pub detail: PGFTarget,
    /// The id of the proposal that added this funding
    pub id: u64,
}

impl StoragePgfFunding {
    /// Init a new pgf funding struct
    pub fn new(detail: PGFTarget, id: u64) -> Self {
        Self { detail, id }
    }
}

/// An add or remove action for PGF
#[derive(
    Debug,
    Clone,
    Hash,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub enum AddRemove<T> {
    /// Add
    Add(T),
    /// Remove
    Remove(T),
}

/// The target of a PGF payment
#[derive(
    Debug,
    Clone,
    PartialEq,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Ord,
    Eq,
    PartialOrd,
)]
pub enum PGFTarget {
    /// Funding target on this chain
    Internal(PGFInternalTarget),
    /// Funding target on another chain
    Ibc(PGFIbcTarget),
}

impl PGFTarget {
    /// Returns the funding target as String
    pub fn target(&self) -> String {
        match self {
            PGFTarget::Internal(t) => t.target.to_string(),
            PGFTarget::Ibc(t) => t.target.clone(),
        }
    }

    /// Returns the funding amount
    pub fn amount(&self) -> Amount {
        match self {
            PGFTarget::Internal(t) => t.amount,
            PGFTarget::Ibc(t) => t.amount,
        }
    }
}

/// The target of a PGF payment
#[derive(
    Debug,
    Clone,
    PartialEq,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Ord,
    Eq,
    PartialOrd,
)]
pub struct PGFInternalTarget {
    /// The target address
    pub target: Address,
    /// The amount of token to fund the target address
    pub amount: Amount,
}

/// The target of a PGF payment
#[derive(
    Debug, Clone, PartialEq, Serialize, Deserialize, Ord, Eq, PartialOrd,
)]
pub struct PGFIbcTarget {
    /// The target address on the target chain
    pub target: String,
    /// The amount of token to fund the target address
    pub amount: Amount,
    /// Port ID to fund
    pub port_id: PortId,
    /// Channel ID to fund
    pub channel_id: ChannelId,
}

impl BorshSerialize for PGFIbcTarget {
    fn serialize<W: std::io::Write>(
        &self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        BorshSerialize::serialize(&self.target, writer)?;
        BorshSerialize::serialize(&self.amount, writer)?;
        BorshSerialize::serialize(&self.port_id.to_string(), writer)?;
        BorshSerialize::serialize(&self.channel_id.to_string(), writer)
    }
}

impl borsh::BorshDeserialize for PGFIbcTarget {
    fn deserialize_reader<R: std::io::Read>(
        reader: &mut R,
    ) -> std::io::Result<Self> {
        use std::io::{Error, ErrorKind};
        let target: String = BorshDeserialize::deserialize_reader(reader)?;
        let amount: Amount = BorshDeserialize::deserialize_reader(reader)?;
        let port_id: String = BorshDeserialize::deserialize_reader(reader)?;
        let port_id: PortId = port_id.parse().map_err(|err| {
            Error::new(
                ErrorKind::InvalidData,
                format!("Error decoding port ID: {}", err),
            )
        })?;
        let channel_id: String = BorshDeserialize::deserialize_reader(reader)?;
        let channel_id: ChannelId = channel_id.parse().map_err(|err| {
            Error::new(
                ErrorKind::InvalidData,
                format!("Error decoding channel ID: {}", err),
            )
        })?;
        Ok(Self {
            target,
            amount,
            port_id,
            channel_id,
        })
    }
}

impl borsh::BorshSchema for PGFIbcTarget {
    fn add_definitions_recursively(
        definitions: &mut BTreeMap<
            borsh::schema::Declaration,
            borsh::schema::Definition,
        >,
    ) {
        let fields = borsh::schema::Fields::NamedFields(vec![
            ("target".into(), String::declaration()),
            ("amount".into(), Amount::declaration()),
            ("port_id".into(), String::declaration()),
            ("channel_id".into(), String::declaration()),
        ]);
        let definition = borsh::schema::Definition::Struct { fields };
        definitions.insert(Self::declaration(), definition);
    }

    fn declaration() -> borsh::schema::Declaration {
        std::any::type_name::<Self>().into()
    }
}

/// The actions that a PGF Steward can propose to execute
#[derive(
    Debug,
    Clone,
    PartialEq,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub enum PGFAction {
    /// A continuous payment
    Continuous(AddRemove<PGFTarget>),
    /// A retro payment
    Retro(PGFTarget),
}

/// The type of a Proposal
#[derive(
    Debug,
    Clone,
    PartialEq,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub enum ProposalType {
    /// Default governance proposal with the optional wasm code
    Default(Option<Hash>),
    /// PGF stewards proposal
    PGFSteward(HashSet<AddRemove<Address>>),
    /// PGF funding proposal
    PGFPayment(Vec<PGFAction>),
}

impl ProposalType {
    /// Check if the proposal type is default
    pub fn is_default(&self) -> bool {
        matches!(self, ProposalType::Default(_))
    }
}

impl Display for ProposalType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposalType::Default(_) => write!(f, "Default"),
            ProposalType::PGFSteward(_) => write!(f, "Pgf steward"),
            ProposalType::PGFPayment(_) => write!(f, "Pgf funding"),
        }
    }
}

impl TryFrom<StewardsUpdate> for HashSet<AddRemove<Address>> {
    type Error = ProposalTypeError;

    fn try_from(value: StewardsUpdate) -> Result<Self, Self::Error> {
        let mut data = HashSet::default();

        if value.add.is_some() {
            data.insert(AddRemove::Add(value.add.unwrap()));
        }
        for steward in value.remove {
            data.insert(AddRemove::Remove(steward));
        }
        Ok(data)
    }
}

impl TryFrom<PgfSteward> for AddRemove<Address> {
    type Error = ProposalTypeError;

    fn try_from(value: PgfSteward) -> Result<Self, Self::Error> {
        match value.action {
            PgfAction::Add => Ok(AddRemove::Add(value.address)),
            PgfAction::Remove => Ok(AddRemove::Remove(value.address)),
        }
    }
}

impl From<PgfContinuous> for PGFAction {
    fn from(value: PgfContinuous) -> Self {
        match value.action {
            PgfAction::Add => {
                PGFAction::Continuous(AddRemove::Add(value.target))
            }
            PgfAction::Remove => {
                PGFAction::Continuous(AddRemove::Remove(value.target))
            }
        }
    }
}

impl From<PgfRetro> for PGFAction {
    fn from(value: PgfRetro) -> Self {
        PGFAction::Retro(value.target)
    }
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
/// Proposal representation when fetched from the storage
pub struct StorageProposal {
    /// The proposal id
    pub id: u64,
    /// The proposal content
    pub content: BTreeMap<String, String>,
    /// The proposal author address
    pub author: Address,
    /// The proposal type
    pub r#type: ProposalType,
    /// The epoch from which voting is allowed
    pub voting_start_epoch: Epoch,
    /// The epoch from which voting is stopped
    pub voting_end_epoch: Epoch,
    /// The epoch from which this changes are executed
    pub grace_epoch: Epoch,
}

impl StorageProposal {
    /// Check if the proposal can be voted
    pub fn can_be_voted(
        &self,
        current_epoch: Epoch,
        is_validator: bool,
    ) -> bool {
        if is_validator {
            self.voting_start_epoch <= current_epoch
                && current_epoch * 3
                    <= self.voting_start_epoch + self.voting_end_epoch * 2
        } else {
            let valid_start_epoch = current_epoch >= self.voting_start_epoch;
            let valid_end_epoch = current_epoch <= self.voting_end_epoch;
            valid_start_epoch && valid_end_epoch
        }
    }

    /// Return the type of tally for the proposal
    pub fn get_tally_type(&self, is_steward: bool) -> TallyType {
        TallyType::from(self.r#type.clone(), is_steward)
    }

    /// Return the status of a proposal
    pub fn get_status(&self, current_epoch: Epoch) -> ProposalStatus {
        if self.voting_start_epoch > current_epoch {
            ProposalStatus::Pending
        } else if self.voting_start_epoch <= current_epoch
            && current_epoch <= self.voting_end_epoch
        {
            ProposalStatus::OnGoing
        } else {
            ProposalStatus::Ended
        }
    }

    /// Serialize a proposal to string
    pub fn to_string_with_status(&self, current_epoch: Epoch) -> String {
        format!(
            "Proposal Id: {}
        {:2}Type: {}
        {:2}Author: {}
        {:2}Content: {:?}
        {:2}Start Epoch: {}
        {:2}End Epoch: {}
        {:2}Grace Epoch: {}
        {:2}Status: {}
        ",
            self.id,
            "",
            self.r#type,
            "",
            self.author,
            "",
            self.content,
            "",
            self.voting_start_epoch,
            "",
            self.voting_end_epoch,
            "",
            self.grace_epoch,
            "",
            self.get_status(current_epoch)
        )
    }
}

impl Display for StorageProposal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Proposal Id: {}
            {:2}Type: {}
            {:2}Author: {}
            {:2}Start Epoch: {}
            {:2}End Epoch: {}
            {:2}Grace Epoch: {}
            ",
            self.id,
            "",
            self.r#type,
            "",
            self.author,
            "",
            self.voting_start_epoch,
            "",
            self.voting_end_epoch,
            "",
            self.grace_epoch
        )
    }
}

#[cfg(any(test, feature = "testing"))]
/// Testing helpers and and strategies for governance proposals
pub mod testing {
    use proptest::prelude::Strategy;
    use proptest::{collection, option, prop_compose};

    use super::*;
    use crate::ledger::governance::storage::proposal::{
        PGFInternalTarget, PGFTarget,
    };
    use crate::types::address::testing::arb_non_internal_address;
    use crate::types::hash::testing::arb_hash;
    use crate::types::token::testing::arb_amount;

    /// Generate an arbitrary add or removal of what's generated by the supplied
    /// strategy
    pub fn arb_add_remove<X: Strategy>(
        strategy: X,
    ) -> impl Strategy<Value = AddRemove<<X as Strategy>::Value>> {
        (0..2, strategy).prop_map(|(discriminant, val)| match discriminant {
            0 => AddRemove::Add(val),
            1 => AddRemove::Remove(val),
            _ => unreachable!(),
        })
    }

    prop_compose! {
        /// Generate an arbitrary PGF target
        pub fn arb_pgf_target()(
            target in arb_non_internal_address(),
            amount in arb_amount(),
        ) -> PGFTarget {
            PGFTarget::Internal(PGFInternalTarget {
                target,
                amount,
            })
        }
    }

    /// Generate an arbitrary PGF action
    pub fn arb_pgf_action() -> impl Strategy<Value = PGFAction> {
        arb_add_remove(arb_pgf_target())
            .prop_map(PGFAction::Continuous)
            .boxed()
            .prop_union(arb_pgf_target().prop_map(PGFAction::Retro).boxed())
    }

    /// Generate an arbitrary proposal type
    pub fn arb_proposal_type() -> impl Strategy<Value = ProposalType> {
        option::of(arb_hash())
            .prop_map(ProposalType::Default)
            .boxed()
            .prop_union(
                collection::hash_set(
                    arb_add_remove(arb_non_internal_address()),
                    0..10,
                )
                .prop_map(ProposalType::PGFSteward)
                .boxed(),
            )
            .or(collection::vec(arb_pgf_action(), 0..10)
                .prop_map(ProposalType::PGFPayment)
                .boxed())
    }
}

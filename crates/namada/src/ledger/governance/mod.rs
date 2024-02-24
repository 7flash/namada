//! Governance VP

pub mod utils;

use std::collections::BTreeSet;

use borsh::BorshDeserialize;
use namada_governance::storage::proposal::{
    AddRemove, PGFAction, ProposalType,
};
use namada_governance::storage::{is_proposal_accepted, keys as gov_storage};
use namada_governance::utils::is_valid_validator_voting_period;
use namada_governance::ProposalVote;
use namada_proof_of_stake::is_validator;
use namada_proof_of_stake::queries::find_delegations;
use namada_state::{StateRead, StorageRead};
use namada_tx::Tx;
use namada_vp_env::VpEnv;
use thiserror::Error;

use self::utils::ReadType;
use crate::address::{Address, InternalAddress};
use crate::ledger::native_vp::{Ctx, NativeVp};
use crate::ledger::{native_vp, pos};
use crate::storage::{Epoch, Key};
use crate::token;
use crate::vm::WasmCacheAccess;

/// for handling Governance NativeVP errors
pub type Result<T> = std::result::Result<T, Error>;

/// The governance internal address
pub const ADDRESS: Address = Address::Internal(InternalAddress::Governance);

/// The maximum number of item in a pgf proposal
pub const MAX_PGF_ACTIONS: usize = 20;

#[allow(missing_docs)]
#[derive(Error, Debug)]
pub enum Error {
    #[error("Native VP error: {0}")]
    NativeVpError(#[from] native_vp::Error),
    #[error("Proposal field should not be empty: {0}")]
    EmptyProposalField(String),
    #[error("Vote key is not valid: {0}")]
    InvalidVoteKey(String),
}

/// Governance VP
pub struct GovernanceVp<'a, S, CA>
where
    S: StateRead,
    CA: WasmCacheAccess,
{
    /// Context to interact with the host structures.
    pub ctx: Ctx<'a, S, CA>,
}

impl<'a, S, CA> NativeVp for GovernanceVp<'a, S, CA>
where
    S: StateRead,
    CA: 'static + WasmCacheAccess,
{
    type Error = Error;

    fn validate_tx(
        &self,
        tx_data: &Tx,
        keys_changed: &BTreeSet<Key>,
        verifiers: &BTreeSet<Address>,
    ) -> Result<bool> {
        let (is_valid_keys_set, set_count) =
            self.is_valid_init_proposal_key_set(keys_changed)?;
        if !is_valid_keys_set {
            tracing::info!("Invalid changed governance key set");
            return Ok(false);
        };

        let native_token = self.ctx.pre().get_native_token()?;

        Ok(keys_changed.iter().all(|key| {
            let proposal_id = gov_storage::get_proposal_id(key);
            let key_type = KeyType::from_key(key, &native_token);

            let result = match (key_type, proposal_id) {
                (KeyType::VOTE, Some(proposal_id)) => {
                    self.is_valid_vote_key(proposal_id, key, verifiers)
                }
                (KeyType::CONTENT, Some(proposal_id)) => {
                    self.is_valid_content_key(proposal_id)
                }
                (KeyType::TYPE, Some(proposal_id)) => {
                    self.is_valid_proposal_type(proposal_id)
                }
                (KeyType::PROPOSAL_CODE, Some(proposal_id)) => {
                    self.is_valid_proposal_code(proposal_id)
                }
                (KeyType::ACTIVATION_EPOCH, Some(proposal_id)) => {
                    self.is_valid_activation_epoch(proposal_id)
                }
                (KeyType::START_EPOCH, Some(proposal_id)) => {
                    self.is_valid_start_epoch(proposal_id)
                }
                (KeyType::END_EPOCH, Some(proposal_id)) => {
                    self.is_valid_end_epoch(proposal_id)
                }
                (KeyType::FUNDS, Some(proposal_id)) => {
                    self.is_valid_funds(proposal_id, &native_token)
                }
                (KeyType::AUTHOR, Some(proposal_id)) => {
                    self.is_valid_author(proposal_id, verifiers)
                }
                (KeyType::COUNTER, _) => self.is_valid_counter(set_count),
                (KeyType::PROPOSAL_COMMIT, _) => {
                    self.is_valid_proposal_commit()
                }
                (KeyType::PARAMETER, _) => self.is_valid_parameter(tx_data),
                (KeyType::BALANCE, _) => self.is_valid_balance(&native_token),
                (KeyType::UNKNOWN_GOVERNANCE, _) => Ok(false),
                (KeyType::UNKNOWN, _) => Ok(true),
                _ => Ok(false),
            };
            match &result {
                Err(err) => tracing::info!(
                    "Key {key_type:?} rejected with error: {err:#?}."
                ),
                Ok(false) => tracing::info!("Key {key_type:?} rejected"),
                Ok(true) => {}
            }
            result.unwrap_or(false)
        }))
    }
}

impl<'a, S, CA> GovernanceVp<'a, S, CA>
where
    S: StateRead,
    CA: 'static + WasmCacheAccess,
{
    fn is_valid_init_proposal_key_set(
        &self,
        keys: &BTreeSet<Key>,
    ) -> Result<(bool, u64)> {
        let counter_key = gov_storage::get_counter_key();
        let pre_counter: u64 = self.force_read(&counter_key, ReadType::Pre)?;
        let post_counter: u64 =
            self.force_read(&counter_key, ReadType::Post)?;

        if post_counter < pre_counter {
            return Ok((false, 0));
        }

        for counter in pre_counter..post_counter {
            // Construct the set of expected keys
            // NOTE: we don't check the existence of committing_epoch because
            // it's going to be checked later into the VP
            let mandatory_keys = BTreeSet::from([
                counter_key.clone(),
                gov_storage::get_content_key(counter),
                gov_storage::get_author_key(counter),
                gov_storage::get_proposal_type_key(counter),
                gov_storage::get_funds_key(counter),
                gov_storage::get_voting_start_epoch_key(counter),
                gov_storage::get_voting_end_epoch_key(counter),
                gov_storage::get_activation_epoch_key(counter),
            ]);

            // Check that expected set is a subset of the actual one
            if !keys.is_superset(&mandatory_keys) {
                return Ok((false, 0));
            }
        }

        Ok((true, post_counter - pre_counter))
    }

    fn is_valid_vote_key(
        &self,
        proposal_id: u64,
        key: &Key,
        verifiers: &BTreeSet<Address>,
    ) -> Result<bool> {
        let counter_key = gov_storage::get_counter_key();
        let voting_start_epoch_key =
            gov_storage::get_voting_start_epoch_key(proposal_id);
        let voting_end_epoch_key =
            gov_storage::get_voting_end_epoch_key(proposal_id);

        let current_epoch = self.ctx.get_block_epoch()?;

        let pre_counter: u64 = self.force_read(&counter_key, ReadType::Pre)?;
        let pre_voting_start_epoch: Epoch =
            self.force_read(&voting_start_epoch_key, ReadType::Pre)?;
        let pre_voting_end_epoch: Epoch =
            self.force_read(&voting_end_epoch_key, ReadType::Pre)?;

        let voter = gov_storage::get_voter_address(key);
        let delegation_address = gov_storage::get_vote_delegation_address(key);

        let (voter_address, delegation_address) =
            match (voter, delegation_address) {
                (Some(voter_address), Some(delegator_address)) => {
                    (voter_address, delegator_address)
                }
                _ => return Err(Error::InvalidVoteKey(key.to_string())),
            };

        // Invalid proposal id
        if pre_counter <= proposal_id {
            tracing::info!(
                "Invalid proposal ID. Expected {pre_counter} or lower, got \
                 {proposal_id}."
            );
            return Ok(false);
        }

        let vote_key = gov_storage::get_vote_proposal_key(
            proposal_id,
            voter_address.clone(),
            delegation_address.clone(),
        );

        if self
            .force_read::<ProposalVote>(&vote_key, ReadType::Post)
            .is_err()
        {
            return Err(Error::InvalidVoteKey(key.to_string()));
        }

        // TODO: We should refactor this by modifying the vote proposal tx
        let all_delegations_are_valid = if let Ok(delegations) =
            find_delegations(&self.ctx.pre(), voter_address, &current_epoch)
        {
            if delegations.is_empty() {
                return Ok(false);
            } else {
                delegations.iter().all(|(address, _)| {
                    let vote_key = gov_storage::get_vote_proposal_key(
                        proposal_id,
                        voter_address.clone(),
                        address.clone(),
                    );
                    self.ctx.post().has_key(&vote_key).unwrap_or(false)
                })
            }
        } else {
            return Ok(false);
        };
        if !all_delegations_are_valid {
            return Ok(false);
        }

        // Voted outside of voting window. We dont check for validator because
        // if the proposal type is validator, we need to let
        // them vote for the entire voting window.
        if !self.is_valid_voting_window(
            current_epoch,
            pre_voting_start_epoch,
            pre_voting_end_epoch,
            false,
        ) {
            tracing::info!(
                "Voted outside voting window. Current epoch: {current_epoch}, \
                 start: {pre_voting_start_epoch}, end: {pre_voting_end_epoch}."
            );
            return Ok(false);
        }

        // first check if validator, then check if delegator
        let is_validator = self
            .is_validator(
                pre_voting_start_epoch,
                verifiers,
                voter_address,
                delegation_address,
            )
            .unwrap_or(false);

        if is_validator {
            let valid_voting_period = is_valid_validator_voting_period(
                current_epoch,
                pre_voting_start_epoch,
                pre_voting_end_epoch,
            );
            return Ok(valid_voting_period);
        }

        let is_delegator = self
            .is_delegator(
                pre_voting_start_epoch,
                verifiers,
                voter_address,
                delegation_address,
            )
            .unwrap_or(false);
        Ok(is_delegator)
    }

    /// Validate a content key
    pub fn is_valid_content_key(&self, proposal_id: u64) -> Result<bool> {
        let content_key: Key = gov_storage::get_content_key(proposal_id);
        let max_content_length_parameter_key =
            gov_storage::get_max_proposal_content_key();

        let has_pre_content: bool = self.ctx.has_key_pre(&content_key)?;
        if has_pre_content {
            return Ok(false);
        }

        let max_content_length: usize =
            self.force_read(&max_content_length_parameter_key, ReadType::Pre)?;
        let post_content =
            self.ctx.read_bytes_post(&content_key)?.unwrap_or_default();

        let is_valid = post_content.len() <= max_content_length;
        if !is_valid {
            tracing::info!(
                "Max content length {max_content_length}, got {}.",
                post_content.len()
            );
        }
        Ok(is_valid)
    }

    /// Validate the proposal type
    pub fn is_valid_proposal_type(&self, proposal_id: u64) -> Result<bool> {
        let proposal_type_key = gov_storage::get_proposal_type_key(proposal_id);
        let proposal_type: ProposalType =
            self.force_read(&proposal_type_key, ReadType::Post)?;

        match proposal_type {
            ProposalType::PGFSteward(stewards) => {
                let stewards_added = stewards
                    .iter()
                    .filter_map(|pgf_action| match pgf_action {
                        AddRemove::Add(address) => Some(address.clone()),
                        _ => None,
                    })
                    .collect::<Vec<Address>>();
                let total_stewards_added = stewards_added.len() as u64;

                let all_pgf_action_addresses = stewards
                    .iter()
                    .map(|steward| match steward {
                        AddRemove::Add(address) => address,
                        AddRemove::Remove(address) => address,
                    })
                    .collect::<BTreeSet<&Address>>()
                    .len();

                // we allow only a single steward to be added
                if total_stewards_added > 1 {
                    Ok(false)
                } else if total_stewards_added == 0 {
                    let is_valid_total_pgf_actions =
                        stewards.len() < MAX_PGF_ACTIONS;
                    return Ok(is_valid_total_pgf_actions);
                } else if let Some(address) = stewards_added.first() {
                    let author_key = gov_storage::get_author_key(proposal_id);
                    let author = self
                        .force_read::<Address>(&author_key, ReadType::Post)?;
                    let is_valid_author = address.eq(&author);

                    let stewards_addresses_are_unique =
                        stewards.len() == all_pgf_action_addresses;
                    let is_valid_total_pgf_actions =
                        all_pgf_action_addresses < MAX_PGF_ACTIONS;

                    return Ok(is_valid_author
                        && stewards_addresses_are_unique
                        && is_valid_total_pgf_actions);
                } else {
                    return Ok(false);
                }
            }
            ProposalType::PGFPayment(fundings) => {
                // collect all the funding target that we have to add and are
                // unique
                let are_continuous_add_targets_unique = fundings
                    .iter()
                    .filter_map(|funding| match funding {
                        PGFAction::Continuous(AddRemove::Add(target)) => {
                            Some(target.target().to_lowercase())
                        }
                        _ => None,
                    })
                    .collect::<BTreeSet<String>>();

                // collect all the funding target that we have to remove and are
                // unique
                let are_continuous_remove_targets_unique = fundings
                    .iter()
                    .filter_map(|funding| match funding {
                        PGFAction::Continuous(AddRemove::Remove(target)) => {
                            Some(target.target().to_lowercase())
                        }
                        _ => None,
                    })
                    .collect::<BTreeSet<String>>();

                let total_retro_targerts = fundings
                    .iter()
                    .filter(|funding| matches!(funding, PGFAction::Retro(_)))
                    .count();

                let is_total_fundings_valid = fundings.len() < MAX_PGF_ACTIONS;

                // check that they are unique by checking that the set of add
                // plus the set of remove plus the set of retro is equal to the
                // total fundings
                let are_continuous_fundings_unique =
                    are_continuous_add_targets_unique.len()
                        + are_continuous_remove_targets_unique.len()
                        + total_retro_targerts
                        == fundings.len();

                // can't remove and add the same target in the same proposal
                let are_targets_unique = are_continuous_add_targets_unique
                    .intersection(&are_continuous_remove_targets_unique)
                    .count() as u64
                    == 0;

                Ok(is_total_fundings_valid
                    && are_continuous_fundings_unique
                    && are_targets_unique)
            }
            _ => Ok(true), // default proposal
        }
    }

    /// Validate a proposal code
    pub fn is_valid_proposal_code(&self, proposal_id: u64) -> Result<bool> {
        let proposal_type_key = gov_storage::get_proposal_type_key(proposal_id);
        let proposal_type: ProposalType =
            self.force_read(&proposal_type_key, ReadType::Post)?;

        if !proposal_type.is_default() {
            return Ok(false);
        }

        let code_key = gov_storage::get_proposal_code_key(proposal_id);
        let max_code_size_parameter_key =
            gov_storage::get_max_proposal_code_size_key();

        let has_pre_code: bool = self.ctx.has_key_pre(&code_key)?;
        if has_pre_code {
            return Ok(false);
        }

        let max_proposal_length: usize =
            self.force_read(&max_code_size_parameter_key, ReadType::Pre)?;
        let post_code: Vec<u8> =
            self.ctx.read_bytes_post(&code_key)?.unwrap_or_default();

        Ok(post_code.len() <= max_proposal_length)
    }

    /// Validate an activation_epoch key
    pub fn is_valid_activation_epoch(&self, proposal_id: u64) -> Result<bool> {
        let start_epoch_key =
            gov_storage::get_voting_start_epoch_key(proposal_id);
        let end_epoch_key = gov_storage::get_voting_end_epoch_key(proposal_id);
        let activation_epoch_key =
            gov_storage::get_activation_epoch_key(proposal_id);
        let max_proposal_period = gov_storage::get_max_proposal_period_key();
        let min_grace_epochs_key =
            gov_storage::get_min_proposal_grace_epochs_key();

        let has_pre_activation_epoch =
            self.ctx.has_key_pre(&activation_epoch_key)?;
        if has_pre_activation_epoch {
            return Ok(false);
        }

        let start_epoch: Epoch =
            self.force_read(&start_epoch_key, ReadType::Post)?;
        let end_epoch: Epoch =
            self.force_read(&end_epoch_key, ReadType::Post)?;
        let activation_epoch: Epoch =
            self.force_read(&activation_epoch_key, ReadType::Post)?;
        let min_grace_epochs: u64 =
            self.force_read(&min_grace_epochs_key, ReadType::Pre)?;
        let max_proposal_period: u64 =
            self.force_read(&max_proposal_period, ReadType::Pre)?;

        let committing_epoch_key = gov_storage::get_committing_proposals_key(
            proposal_id,
            activation_epoch.into(),
        );
        let has_post_committing_epoch =
            self.ctx.has_key_post(&committing_epoch_key)?;
        if !has_post_committing_epoch {
            tracing::info!("Committing proposal key is missing present");
        }

        let is_valid_activation_epoch = end_epoch < activation_epoch
            && (activation_epoch - end_epoch).0 >= min_grace_epochs;
        if !is_valid_activation_epoch {
            tracing::info!(
                "Expected min duration between the end and activation epoch \
                 {min_grace_epochs}, but got activation = {}, end = {}",
                activation_epoch,
                end_epoch
            );
        }
        let is_valid_max_proposal_period = start_epoch < activation_epoch
            && activation_epoch.0 - start_epoch.0 <= max_proposal_period;
        if !is_valid_max_proposal_period {
            tracing::info!(
                "Expected max duration between the start and activation epoch \
                 {max_proposal_period}, but got activation = {}, start = {}",
                activation_epoch,
                start_epoch
            );
        }

        Ok(has_post_committing_epoch
            && is_valid_activation_epoch
            && is_valid_max_proposal_period)
    }

    /// Validate a start_epoch key
    pub fn is_valid_start_epoch(&self, proposal_id: u64) -> Result<bool> {
        let start_epoch_key =
            gov_storage::get_voting_start_epoch_key(proposal_id);
        let end_epoch_key = gov_storage::get_voting_end_epoch_key(proposal_id);
        let min_period_parameter_key =
            gov_storage::get_min_proposal_voting_period_key();

        let current_epoch = self.ctx.get_block_epoch()?;

        let has_pre_start_epoch = self.ctx.has_key_pre(&start_epoch_key)?;
        let has_pre_end_epoch = self.ctx.has_key_pre(&end_epoch_key)?;

        if has_pre_start_epoch || has_pre_end_epoch {
            return Ok(false);
        }

        let start_epoch: Epoch =
            self.force_read(&start_epoch_key, ReadType::Post)?;
        let end_epoch: Epoch =
            self.force_read(&end_epoch_key, ReadType::Post)?;
        let min_period: u64 =
            self.force_read(&min_period_parameter_key, ReadType::Pre)?;

        if end_epoch <= start_epoch || start_epoch <= current_epoch {
            return Ok(false);
        }

        Ok((end_epoch - start_epoch) % min_period == 0
            && (end_epoch - start_epoch).0 >= min_period)
    }

    /// Validate a end_epoch key
    fn is_valid_end_epoch(&self, proposal_id: u64) -> Result<bool> {
        let start_epoch_key =
            gov_storage::get_voting_start_epoch_key(proposal_id);
        let end_epoch_key = gov_storage::get_voting_end_epoch_key(proposal_id);
        let min_period_parameter_key =
            gov_storage::get_min_proposal_voting_period_key();
        let max_period_parameter_key =
            gov_storage::get_max_proposal_period_key();

        let current_epoch = self.ctx.get_block_epoch()?;

        let has_pre_start_epoch = self.ctx.has_key_pre(&start_epoch_key)?;
        let has_pre_end_epoch = self.ctx.has_key_pre(&end_epoch_key)?;

        if has_pre_start_epoch || has_pre_end_epoch {
            return Ok(false);
        }

        let start_epoch: Epoch =
            self.force_read(&start_epoch_key, ReadType::Post)?;
        let end_epoch: Epoch =
            self.force_read(&end_epoch_key, ReadType::Post)?;
        let min_period: u64 =
            self.force_read(&min_period_parameter_key, ReadType::Pre)?;
        let max_period: u64 =
            self.force_read(&max_period_parameter_key, ReadType::Pre)?;

        if end_epoch <= start_epoch || start_epoch <= current_epoch {
            tracing::info!(
                "Proposal end epoch ({end_epoch}) must be after the start \
                 epoch ({start_epoch}), and the start epoch must be after the \
                 current epoch ({current_epoch})."
            );
            return Ok(false);
        }
        Ok((end_epoch - start_epoch) % min_period == 0
            && (end_epoch - start_epoch).0 >= min_period
            && (end_epoch - start_epoch).0 <= max_period)
    }

    /// Validate a funds key
    pub fn is_valid_funds(
        &self,
        proposal_id: u64,
        native_token_address: &Address,
    ) -> Result<bool> {
        let funds_key = gov_storage::get_funds_key(proposal_id);
        let balance_key = token::storage_key::balance_key(
            native_token_address,
            self.ctx.address,
        );
        let min_funds_parameter_key = gov_storage::get_min_proposal_fund_key();

        let min_funds_parameter: token::Amount =
            self.force_read(&min_funds_parameter_key, ReadType::Pre)?;
        let pre_balance: Option<token::Amount> =
            self.ctx.pre().read(&balance_key)?;
        let post_balance: token::Amount =
            self.force_read(&balance_key, ReadType::Post)?;
        let post_funds: token::Amount =
            self.force_read(&funds_key, ReadType::Post)?;

        if let Some(pre_balance) = pre_balance {
            let is_post_funds_greater_than_minimum =
                post_funds >= min_funds_parameter;
            let is_valid_funds = post_balance >= pre_balance
                && post_balance - pre_balance == post_funds;
            Ok(is_post_funds_greater_than_minimum && is_valid_funds)
        } else {
            Ok(post_funds >= min_funds_parameter && post_balance == post_funds)
        }
    }

    /// Validate a balance key
    fn is_valid_balance(&self, native_token_address: &Address) -> Result<bool> {
        let balance_key = token::storage_key::balance_key(
            native_token_address,
            self.ctx.address,
        );
        let min_funds_parameter_key = gov_storage::get_min_proposal_fund_key();

        let pre_balance: Option<token::Amount> =
            self.ctx.pre().read(&balance_key)?;

        let min_funds_parameter: token::Amount =
            self.force_read(&min_funds_parameter_key, ReadType::Pre)?;
        let post_balance: token::Amount =
            self.force_read(&balance_key, ReadType::Post)?;

        if let Some(pre_balance) = pre_balance {
            Ok(post_balance > pre_balance
                && post_balance - pre_balance >= min_funds_parameter)
        } else {
            Ok(post_balance >= min_funds_parameter)
        }
    }

    /// Validate a author key
    pub fn is_valid_author(
        &self,
        proposal_id: u64,
        verifiers: &BTreeSet<Address>,
    ) -> Result<bool> {
        let author_key = gov_storage::get_author_key(proposal_id);

        let has_pre_author = self.ctx.has_key_pre(&author_key)?;
        if has_pre_author {
            return Ok(false);
        }

        let author = self.force_read(&author_key, ReadType::Post)?;
        let author_exists =
            namada_account::exists(&self.ctx.pre(), &author).unwrap_or(false);

        Ok(author_exists && verifiers.contains(&author))
    }

    /// Validate a counter key
    pub fn is_valid_counter(&self, set_count: u64) -> Result<bool> {
        let counter_key = gov_storage::get_counter_key();
        let pre_counter: u64 = self.force_read(&counter_key, ReadType::Pre)?;
        let post_counter: u64 =
            self.force_read(&counter_key, ReadType::Post)?;

        Ok(pre_counter + set_count == post_counter)
    }

    /// Validate a commit key
    pub fn is_valid_proposal_commit(&self) -> Result<bool> {
        let counter_key = gov_storage::get_counter_key();
        let pre_counter: u64 = self.force_read(&counter_key, ReadType::Pre)?;
        let post_counter: u64 =
            self.force_read(&counter_key, ReadType::Post)?;

        // NOTE: can't do pre_counter + set_count == post_counter here
        // because someone may update an empty proposal that just
        // register a committing key causing a bug
        Ok(pre_counter < post_counter)
    }

    /// Validate a governance parameter
    pub fn is_valid_parameter(&self, tx: &Tx) -> Result<bool> {
        match tx.data() {
            Some(data) => is_proposal_accepted(&self.ctx.pre(), data.as_ref())
                .map_err(Error::NativeVpError),
            None => Ok(false),
        }
    }

    /// Check if a vote is from a validator
    pub fn is_validator(
        &self,
        _epoch: Epoch,
        verifiers: &BTreeSet<Address>,
        address: &Address,
        delegation_address: &Address,
    ) -> Result<bool>
    where
        S: StateRead,
        CA: 'static + WasmCacheAccess,
    {
        if !address.eq(delegation_address) {
            return Ok(false);
        }

        let is_validator = is_validator(&self.ctx.pre(), address)?;

        Ok(is_validator && verifiers.contains(address))
    }

    /// Private method to read from storage data that are 100% in storage.
    fn force_read<T>(&self, key: &Key, read_type: ReadType) -> Result<T>
    where
        T: BorshDeserialize,
    {
        let res = match read_type {
            ReadType::Pre => self.ctx.pre().read::<T>(key),
            ReadType::Post => self.ctx.post().read::<T>(key),
        }?;

        if let Some(data) = res {
            Ok(data)
        } else {
            Err(Error::EmptyProposalField(key.to_string()))
        }
    }

    fn is_valid_voting_window(
        &self,
        current_epoch: Epoch,
        start_epoch: Epoch,
        end_epoch: Epoch,
        is_validator: bool,
    ) -> bool {
        if is_validator {
            is_valid_validator_voting_period(
                current_epoch,
                start_epoch,
                end_epoch,
            )
        } else {
            current_epoch >= start_epoch && current_epoch <= end_epoch
        }
    }

    /// Check if a vote is from a delegator
    pub fn is_delegator(
        &self,
        epoch: Epoch,
        verifiers: &BTreeSet<Address>,
        address: &Address,
        delegation_address: &Address,
    ) -> Result<bool> {
        Ok(address != delegation_address
            && verifiers.contains(address)
            && pos::namada_proof_of_stake::is_delegator(
                &self.ctx.pre(),
                address,
                Some(epoch),
            )?)
    }
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Debug)]
enum KeyType {
    #[allow(non_camel_case_types)]
    COUNTER,
    #[allow(non_camel_case_types)]
    VOTE,
    #[allow(non_camel_case_types)]
    CONTENT,
    #[allow(non_camel_case_types)]
    PROPOSAL_CODE,
    #[allow(non_camel_case_types)]
    TYPE,
    #[allow(non_camel_case_types)]
    PROPOSAL_COMMIT,
    #[allow(non_camel_case_types)]
    ACTIVATION_EPOCH,
    #[allow(non_camel_case_types)]
    START_EPOCH,
    #[allow(non_camel_case_types)]
    END_EPOCH,
    #[allow(non_camel_case_types)]
    FUNDS,
    #[allow(non_camel_case_types)]
    BALANCE,
    #[allow(non_camel_case_types)]
    AUTHOR,
    #[allow(non_camel_case_types)]
    PARAMETER,
    #[allow(non_camel_case_types)]
    UNKNOWN_GOVERNANCE,
    #[allow(non_camel_case_types)]
    UNKNOWN,
}

impl KeyType {
    fn from_key(key: &Key, native_token: &Address) -> Self {
        if gov_storage::is_vote_key(key) {
            Self::VOTE
        } else if gov_storage::is_content_key(key) {
            KeyType::CONTENT
        } else if gov_storage::is_proposal_type_key(key) {
            Self::TYPE
        } else if gov_storage::is_proposal_code_key(key) {
            Self::PROPOSAL_CODE
        } else if gov_storage::is_activation_epoch_key(key) {
            KeyType::ACTIVATION_EPOCH
        } else if gov_storage::is_start_epoch_key(key) {
            KeyType::START_EPOCH
        } else if gov_storage::is_commit_proposal_key(key) {
            KeyType::PROPOSAL_COMMIT
        } else if gov_storage::is_end_epoch_key(key) {
            KeyType::END_EPOCH
        } else if gov_storage::is_balance_key(key) {
            KeyType::FUNDS
        } else if gov_storage::is_author_key(key) {
            KeyType::AUTHOR
        } else if gov_storage::is_counter_key(key) {
            KeyType::COUNTER
        } else if gov_storage::is_parameter_key(key) {
            KeyType::PARAMETER
        } else if token::storage_key::is_balance_key(native_token, key)
            .is_some()
        {
            KeyType::BALANCE
        } else if gov_storage::is_governance_key(key) {
            KeyType::UNKNOWN_GOVERNANCE
        } else {
            KeyType::UNKNOWN
        }
    }
}

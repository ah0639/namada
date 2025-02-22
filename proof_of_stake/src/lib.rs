//! Proof of Stake system.

#![doc(html_favicon_url = "https://dev.namada.net/master/favicon.png")]
#![doc(html_logo_url = "https://dev.namada.net/master/rustdoc-logo.png")]
#![warn(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::private_intra_doc_links)]

pub mod epoched;
pub mod parameters;
pub mod pos_queries;
pub mod queries;
pub mod rewards;
pub mod slashing;
pub mod storage;
pub mod storage_key;
pub mod types;
pub mod validator_set_update;
// pub mod validation;

mod error;
#[cfg(test)]
mod tests;

use core::fmt::Debug;
use std::cmp::{self};
use std::collections::{BTreeMap, BTreeSet, HashSet};

pub use error::*;
use namada_core::ledger::storage_api::collections::lazy_map::{
    Collectable, LazyMap, NestedSubKey, SubKey,
};
use namada_core::ledger::storage_api::{
    self, token, StorageRead, StorageWrite,
};
use namada_core::types::address::{Address, InternalAddress};
use namada_core::types::dec::Dec;
use namada_core::types::key::common;
use namada_core::types::storage::BlockHeight;
pub use namada_core::types::storage::{Epoch, Key, KeySeg};
pub use parameters::{OwnedPosParams, PosParams};

use crate::queries::{find_bonds, has_bonds};
use crate::rewards::{
    add_rewards_to_counter, compute_current_rewards_from_bonds,
    read_rewards_counter, take_rewards_from_counter,
};
use crate::slashing::{
    apply_list_slashes, compute_amount_after_slashing_unbond,
    compute_amount_after_slashing_withdraw, find_validator_slashes,
};
use crate::storage::{
    below_capacity_validator_set_handle, bond_handle,
    consensus_validator_set_handle, delegator_redelegated_bonds_handle,
    delegator_redelegated_unbonds_handle, get_last_reward_claim_epoch,
    liveness_missed_votes_handle, liveness_sum_missed_votes_handle,
    read_consensus_validator_set_addresses, read_non_pos_owned_params,
    read_pos_params, read_validator_last_slash_epoch,
    read_validator_max_commission_rate_change, read_validator_stake,
    total_bonded_handle, total_consensus_stake_handle, total_unbonded_handle,
    try_insert_consensus_key, unbond_handle, update_total_deltas,
    update_validator_deltas, validator_addresses_handle,
    validator_commission_rate_handle, validator_consensus_key_handle,
    validator_deltas_handle, validator_eth_cold_key_handle,
    validator_eth_hot_key_handle, validator_incoming_redelegations_handle,
    validator_outgoing_redelegations_handle, validator_protocol_key_handle,
    validator_rewards_products_handle, validator_set_positions_handle,
    validator_slashes_handle, validator_state_handle,
    validator_total_redelegated_bonded_handle,
    validator_total_redelegated_unbonded_handle, write_last_reward_claim_epoch,
    write_pos_params, write_validator_address_raw_hash, write_validator_avatar,
    write_validator_description, write_validator_discord_handle,
    write_validator_email, write_validator_max_commission_rate_change,
    write_validator_metadata, write_validator_website,
};
use crate::storage_key::{bonds_for_source_prefix, is_bond_key};
use crate::types::{
    BondId, ConsensusValidator, ConsensusValidatorSet,
    EagerRedelegatedBondsMap, RedelegatedBondsOrUnbonds, RedelegatedTokens,
    ResultSlashing, Slash, Unbonds, ValidatorMetaData, ValidatorSetUpdate,
    ValidatorState, VoteInfo,
};
use crate::validator_set_update::{
    copy_validator_sets_and_positions, insert_validator_into_validator_set,
    promote_next_below_capacity_validator_to_consensus,
    remove_below_capacity_validator, remove_consensus_validator,
    update_validator_set,
};

/// Address of the PoS account implemented as a native VP
pub const ADDRESS: Address = Address::Internal(InternalAddress::PoS);

/// Address of the PoS slash pool account
pub const SLASH_POOL_ADDRESS: Address =
    Address::Internal(InternalAddress::PosSlashPool);

/// Address of the staking token (i.e. the native token)
pub fn staking_token_address(storage: &impl StorageRead) -> Address {
    storage
        .get_native_token()
        .expect("Must be able to read native token address")
}

/// Init genesis. Requires that the governance parameters are initialized.
pub fn init_genesis<S>(
    storage: &mut S,
    params: &OwnedPosParams,
    current_epoch: Epoch,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    tracing::debug!("Initializing PoS genesis");
    write_pos_params(storage, params)?;

    consensus_validator_set_handle().init(storage, current_epoch)?;
    below_capacity_validator_set_handle().init(storage, current_epoch)?;
    validator_set_positions_handle().init(storage, current_epoch)?;
    validator_addresses_handle().init(storage, current_epoch)?;
    tracing::debug!("Finished genesis");
    Ok(())
}

/// Copies the validator sets into all epochs up through the pipeline epoch at
/// genesis.
pub fn copy_genesis_validator_sets<S>(
    storage: &mut S,
    params: &OwnedPosParams,
    current_epoch: Epoch,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    let params = read_non_pos_owned_params(storage, params.clone())?;

    // Copy the genesis validator sets up to the pipeline epoch
    for epoch in (current_epoch.next()).iter_range(params.pipeline_len) {
        copy_validator_sets_and_positions(
            storage,
            &params,
            current_epoch,
            epoch,
        )?;
    }
    Ok(())
}

/// Check if the provided address is a validator address
pub fn is_validator<S>(
    storage: &S,
    address: &Address,
) -> storage_api::Result<bool>
where
    S: StorageRead,
{
    // TODO: should this check be made different? I suppose it does work but
    // feels weird...
    let rate = read_validator_max_commission_rate_change(storage, address)?;
    Ok(rate.is_some())
}

/// Check if the provided address is a delegator address, optionally at a
/// particular epoch
pub fn is_delegator<S>(
    storage: &S,
    address: &Address,
    epoch: Option<namada_core::types::storage::Epoch>,
) -> storage_api::Result<bool>
where
    S: StorageRead,
{
    let prefix = bonds_for_source_prefix(address);
    match epoch {
        Some(epoch) => {
            let iter = storage_api::iter_prefix_bytes(storage, &prefix)?;
            for res in iter {
                let (key, _) = res?;
                if let Some((bond_id, bond_epoch)) = is_bond_key(&key) {
                    if bond_id.source != bond_id.validator
                        && bond_epoch <= epoch
                    {
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        }
        None => {
            let iter = storage_api::iter_prefix_bytes(storage, &prefix)?;
            for res in iter {
                let (key, _) = res?;
                if let Some((bond_id, _epoch)) = is_bond_key(&key) {
                    if bond_id.source != bond_id.validator {
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        }
    }
}

/// Self-bond tokens to a validator when `source` is `None` or equal to
/// the `validator` address, or delegate tokens from the `source` to the
/// `validator`.
pub fn bond_tokens<S>(
    storage: &mut S,
    source: Option<&Address>,
    validator: &Address,
    amount: token::Amount,
    current_epoch: Epoch,
    offset_opt: Option<u64>,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    tracing::debug!(
        "Bonding token amount {} at epoch {current_epoch}",
        amount.to_string_native()
    );
    if amount.is_zero() {
        return Ok(());
    }

    // Transfer the bonded tokens from the source to PoS
    if let Some(source) = source {
        if source != validator && is_validator(storage, source)? {
            return Err(
                BondError::SourceMustNotBeAValidator(source.clone()).into()
            );
        }
    }
    let source = source.unwrap_or(validator);
    tracing::debug!("Source {source} --> Validator {validator}");

    let staking_token = staking_token_address(storage);
    token::transfer(storage, &staking_token, source, &ADDRESS, amount)?;

    let params = read_pos_params(storage)?;
    let offset = offset_opt.unwrap_or(params.pipeline_len);
    let offset_epoch = current_epoch + offset;

    // Check that the validator is actually a validator
    let validator_state_handle = validator_state_handle(validator);
    let state = validator_state_handle.get(storage, offset_epoch, &params)?;
    if state.is_none() {
        return Err(BondError::NotAValidator(validator.clone()).into());
    }

    let bond_handle = bond_handle(source, validator);
    let total_bonded_handle = total_bonded_handle(validator);

    if tracing::level_enabled!(tracing::Level::DEBUG) {
        let bonds = find_bonds(storage, source, validator)?;
        tracing::debug!("\nBonds before incrementing: {bonds:#?}");
    }

    // Initialize or update the bond at the pipeline offset
    bond_handle.add(storage, amount, current_epoch, offset)?;
    total_bonded_handle.add(storage, amount, current_epoch, offset)?;

    if tracing::level_enabled!(tracing::Level::DEBUG) {
        let bonds = find_bonds(storage, source, validator)?;
        tracing::debug!("\nBonds after incrementing: {bonds:#?}");
    }

    // Update the validator set
    // Allow bonding even if the validator is jailed. However, if jailed, there
    // must be no changes to the validator set. Check at the pipeline epoch.
    let is_jailed_or_inactive_at_pipeline = matches!(
        validator_state_handle.get(storage, offset_epoch, &params)?,
        Some(ValidatorState::Jailed) | Some(ValidatorState::Inactive)
    );
    if !is_jailed_or_inactive_at_pipeline {
        update_validator_set(
            storage,
            &params,
            validator,
            amount.change(),
            current_epoch,
            offset_opt,
        )?;
    }

    // Update the validator and total deltas
    update_validator_deltas(
        storage,
        &params,
        validator,
        amount.change(),
        current_epoch,
        offset_opt,
    )?;

    update_total_deltas(
        storage,
        &params,
        amount.change(),
        current_epoch,
        offset_opt,
    )?;

    Ok(())
}

/// Compute total validator stake for the current epoch
fn compute_total_consensus_stake<S>(
    storage: &S,
    epoch: Epoch,
) -> storage_api::Result<token::Amount>
where
    S: StorageRead,
{
    consensus_validator_set_handle()
        .at(&epoch)
        .iter(storage)?
        .fold(Ok(token::Amount::zero()), |acc, entry| {
            let acc = acc?;
            let (
                NestedSubKey::Data {
                    key: amount,
                    nested_sub_key: _,
                },
                _validator,
            ) = entry?;
            Ok(acc.checked_add(amount).expect(
                "Total consensus stake computation should not overflow.",
            ))
        })
}

/// Compute and then store the total consensus stake
pub fn compute_and_store_total_consensus_stake<S>(
    storage: &mut S,
    epoch: Epoch,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    let total = compute_total_consensus_stake(storage, epoch)?;
    tracing::debug!(
        "Total consensus stake for epoch {}: {}",
        epoch,
        total.to_string_native()
    );
    total_consensus_stake_handle().set(storage, total, epoch, 0)
}

/// Used below in `fn unbond_tokens` to update the bond and unbond amounts
#[derive(Eq, Hash, PartialEq)]
struct BondAndUnbondUpdates {
    bond_start: Epoch,
    new_bond_value: token::Change,
    unbond_value: token::Change,
}

/// Unbond tokens that are bonded between a validator and a source (self or
/// delegator).
///
/// This fn is also called during redelegation for a source validator, in
/// which case the `is_redelegation` param must be true.
pub fn unbond_tokens<S>(
    storage: &mut S,
    source: Option<&Address>,
    validator: &Address,
    amount: token::Amount,
    current_epoch: Epoch,
    is_redelegation: bool,
) -> storage_api::Result<ResultSlashing>
where
    S: StorageRead + StorageWrite,
{
    if amount.is_zero() {
        return Ok(ResultSlashing::default());
    }

    let params = read_pos_params(storage)?;
    let pipeline_epoch = current_epoch + params.pipeline_len;
    let withdrawable_epoch = current_epoch + params.withdrawable_epoch_offset();
    tracing::debug!(
        "Unbonding token amount {} at epoch {}, withdrawable at epoch {}",
        amount.to_string_native(),
        current_epoch,
        withdrawable_epoch
    );

    // Make sure source is not some other validator
    if let Some(source) = source {
        if source != validator && is_validator(storage, source)? {
            return Err(
                BondError::SourceMustNotBeAValidator(source.clone()).into()
            );
        }
    }
    // Make sure the target is actually a validator
    if !is_validator(storage, validator)? {
        return Err(BondError::NotAValidator(validator.clone()).into());
    }
    // Make sure the validator is not currently frozen
    if is_validator_frozen(storage, validator, current_epoch, &params)? {
        return Err(UnbondError::ValidatorIsFrozen(validator.clone()).into());
    }

    let source = source.unwrap_or(validator);
    let bonds_handle = bond_handle(source, validator);

    // Make sure there are enough tokens left in the bond at the pipeline offset
    let remaining_at_pipeline = bonds_handle
        .get_sum(storage, pipeline_epoch, &params)?
        .unwrap_or_default();
    if amount > remaining_at_pipeline {
        return Err(UnbondError::UnbondAmountGreaterThanBond(
            amount.to_string_native(),
            remaining_at_pipeline.to_string_native(),
        )
        .into());
    }

    if tracing::level_enabled!(tracing::Level::DEBUG) {
        let bonds = find_bonds(storage, source, validator)?;
        tracing::debug!("\nBonds before decrementing: {bonds:#?}");
    }

    let unbonds = unbond_handle(source, validator);

    let redelegated_bonds =
        delegator_redelegated_bonds_handle(source).at(validator);

    #[cfg(debug_assertions)]
    let redel_bonds_pre = redelegated_bonds.collect_map(storage)?;

    // `resultUnbonding`
    // Find the bonds to fully unbond (remove) and one to partially unbond, if
    // necessary
    let bonds_to_unbond = find_bonds_to_remove(
        storage,
        &bonds_handle.get_data_handler(),
        amount,
    )?;

    // `modifiedRedelegation`
    // A bond may have both redelegated and non-redelegated tokens in it. If
    // this is the case, compute the modified state of the redelegation.
    let modified_redelegation = match bonds_to_unbond.new_entry {
        Some((bond_epoch, new_bond_amount)) => {
            if redelegated_bonds.contains(storage, &bond_epoch)? {
                let cur_bond_amount = bonds_handle
                    .get_delta_val(storage, bond_epoch)?
                    .unwrap_or_default();
                compute_modified_redelegation(
                    storage,
                    &redelegated_bonds.at(&bond_epoch),
                    bond_epoch,
                    cur_bond_amount - new_bond_amount,
                )?
            } else {
                ModifiedRedelegation::default()
            }
        }
        None => ModifiedRedelegation::default(),
    };

    // Compute the new unbonds eagerly
    // `keysUnbonds`
    // Get a set of epochs from which we're unbonding (fully and partially).
    let bond_epochs_to_unbond =
        if let Some((start_epoch, _)) = bonds_to_unbond.new_entry {
            let mut to_remove = bonds_to_unbond.epochs.clone();
            to_remove.insert(start_epoch);
            to_remove
        } else {
            bonds_to_unbond.epochs.clone()
        };

    // `newUnbonds`
    // For each epoch we're unbonding, find the amount that's being unbonded.
    // For full unbonds, this is the current bond value. For partial unbonds
    // it is a difference between the current and new bond amount.
    let new_unbonds_map = bond_epochs_to_unbond
        .into_iter()
        .map(|epoch| {
            let cur_bond_value = bonds_handle
                .get_delta_val(storage, epoch)
                .unwrap()
                .unwrap_or_default();
            let value = if let Some((start_epoch, new_bond_amount)) =
                bonds_to_unbond.new_entry
            {
                if start_epoch == epoch {
                    cur_bond_value - new_bond_amount
                } else {
                    cur_bond_value
                }
            } else {
                cur_bond_value
            };
            (epoch, value)
        })
        .collect::<BTreeMap<Epoch, token::Amount>>();

    // `updatedBonded`
    // Remove bonds for all the full unbonds.
    for epoch in &bonds_to_unbond.epochs {
        bonds_handle.get_data_handler().remove(storage, epoch)?;
    }
    // Replace bond amount for partial unbond, if any.
    if let Some((bond_epoch, new_bond_amount)) = bonds_to_unbond.new_entry {
        bonds_handle.set(storage, new_bond_amount, bond_epoch, 0)?;
    }

    // `updatedUnbonded`
    // Update the unbonds in storage using the eager map computed above
    if !is_redelegation {
        for (start_epoch, &unbond_amount) in new_unbonds_map.iter() {
            unbonds.at(start_epoch).update(
                storage,
                withdrawable_epoch,
                |cur_val| cur_val.unwrap_or_default() + unbond_amount,
            )?;
        }
    }

    // `newRedelegatedUnbonds`
    // This is what the delegator's redelegated unbonds would look like if this
    // was the only unbond in the PoS system. We need to add these redelegated
    // unbonds to the existing redelegated unbonds
    let new_redelegated_unbonds = compute_new_redelegated_unbonds(
        storage,
        &redelegated_bonds,
        &bonds_to_unbond.epochs,
        &modified_redelegation,
    )?;

    // `updatedRedelegatedBonded`
    // NOTE: for now put this here after redelegated unbonds calc bc that one
    // uses the pre-modified redelegated bonds from storage!
    // First remove redelegation entries in epochs with full unbonds.
    for epoch_to_remove in &bonds_to_unbond.epochs {
        redelegated_bonds.remove_all(storage, epoch_to_remove)?;
    }
    if let Some(epoch) = modified_redelegation.epoch {
        tracing::debug!("\nIs modified redelegation");
        if modified_redelegation.validators_to_remove.is_empty() {
            redelegated_bonds.remove_all(storage, &epoch)?;
        } else {
            // Then update the redelegated bonds at this epoch
            let rbonds = redelegated_bonds.at(&epoch);
            update_redelegated_bonds(storage, &rbonds, &modified_redelegation)?;
        }
    }

    if !is_redelegation {
        // `val updatedRedelegatedUnbonded` with updates applied below
        // Delegator's redelegated unbonds to this validator.
        let delegator_redelegated_unbonded =
            delegator_redelegated_unbonds_handle(source).at(validator);

        // Quint `def updateRedelegatedUnbonded` with `val
        // updatedRedelegatedUnbonded` together with last statement
        // in `updatedDelegator.with("redelegatedUnbonded", ...` updated
        // directly in storage
        for (start, unbonds) in &new_redelegated_unbonds {
            let this_redelegated_unbonded = delegator_redelegated_unbonded
                .at(start)
                .at(&withdrawable_epoch);

            // Update the delegator's redelegated unbonds with the change
            for (src_validator, redelegated_unbonds) in unbonds {
                let redelegated_unbonded =
                    this_redelegated_unbonded.at(src_validator);
                for (&redelegation_epoch, &change) in redelegated_unbonds {
                    redelegated_unbonded.update(
                        storage,
                        redelegation_epoch,
                        |current| current.unwrap_or_default() + change,
                    )?;
                }
            }
        }
    }
    // all `val updatedDelegator` changes are applied at this point

    // `val updatedTotalBonded` and `val updatedTotalUnbonded` with updates
    // Update the validator's total bonded and unbonded amounts
    let total_bonded = total_bonded_handle(validator).get_data_handler();
    let total_unbonded = total_unbonded_handle(validator).at(&pipeline_epoch);
    for (&start_epoch, &amount) in &new_unbonds_map {
        total_bonded.update(storage, start_epoch, |current| {
            current.unwrap_or_default() - amount
        })?;
        total_unbonded.update(storage, start_epoch, |current| {
            current.unwrap_or_default() + amount
        })?;
    }

    let total_redelegated_bonded =
        validator_total_redelegated_bonded_handle(validator);
    let total_redelegated_unbonded =
        validator_total_redelegated_unbonded_handle(validator);
    for (redelegation_start_epoch, unbonds) in &new_redelegated_unbonds {
        for (src_validator, changes) in unbonds {
            for (bond_start_epoch, change) in changes {
                // total redelegated bonded
                let bonded_sub_map = total_redelegated_bonded
                    .at(redelegation_start_epoch)
                    .at(src_validator);
                bonded_sub_map.update(
                    storage,
                    *bond_start_epoch,
                    |current| current.unwrap_or_default() - *change,
                )?;

                // total redelegated unbonded
                let unbonded_sub_map = total_redelegated_unbonded
                    .at(&pipeline_epoch)
                    .at(redelegation_start_epoch)
                    .at(src_validator);
                unbonded_sub_map.update(
                    storage,
                    *bond_start_epoch,
                    |current| current.unwrap_or_default() + *change,
                )?;
            }
        }
    }

    let slashes = find_validator_slashes(storage, validator)?;
    // `val resultSlashing`
    let result_slashing = compute_amount_after_slashing_unbond(
        storage,
        &params,
        &new_unbonds_map,
        &new_redelegated_unbonds,
        slashes,
    )?;
    #[cfg(debug_assertions)]
    let redel_bonds_post = redelegated_bonds.collect_map(storage)?;
    debug_assert!(
        result_slashing.sum <= amount,
        "Amount after slashing ({}) must be <= requested amount to unbond \
         ({}).",
        result_slashing.sum.to_string_native(),
        amount.to_string_native(),
    );

    let change_after_slashing = -result_slashing.sum.change();
    // Update the validator set at the pipeline offset. Since unbonding from a
    // jailed validator who is no longer frozen is allowed, only update the
    // validator set if the validator is not jailed
    let is_jailed_or_inactive_at_pipeline = matches!(
        validator_state_handle(validator).get(
            storage,
            pipeline_epoch,
            &params
        )?,
        Some(ValidatorState::Jailed) | Some(ValidatorState::Inactive)
    );
    if !is_jailed_or_inactive_at_pipeline {
        update_validator_set(
            storage,
            &params,
            validator,
            change_after_slashing,
            current_epoch,
            None,
        )?;
    }

    // Update the validator and total deltas at the pipeline offset
    update_validator_deltas(
        storage,
        &params,
        validator,
        change_after_slashing,
        current_epoch,
        None,
    )?;
    update_total_deltas(
        storage,
        &params,
        change_after_slashing,
        current_epoch,
        None,
    )?;

    if tracing::level_enabled!(tracing::Level::DEBUG) {
        let bonds = find_bonds(storage, source, validator)?;
        tracing::debug!("\nBonds after decrementing: {bonds:#?}");
    }

    // Invariant: in the affected epochs, the delta of bonds must be >= delta of
    // redelegated bonds deltas sum
    #[cfg(debug_assertions)]
    {
        let mut epochs = bonds_to_unbond.epochs.clone();
        if let Some((epoch, _)) = bonds_to_unbond.new_entry {
            epochs.insert(epoch);
        }
        for epoch in epochs {
            let cur_bond = bonds_handle
                .get_delta_val(storage, epoch)?
                .unwrap_or_default();
            let redelegated_deltas = redelegated_bonds
                .at(&epoch)
                // Sum of redelegations from any src validator
                .collect_map(storage)?
                .into_values()
                .map(|redeleg| redeleg.into_values().sum())
                .sum();
            debug_assert!(
                cur_bond >= redelegated_deltas,
                "After unbonding, in epoch {epoch} the bond amount {} must be \
                 >= redelegated deltas at pipeline {}.\n\nredelegated_bonds \
                 pre: {redel_bonds_pre:#?}\nredelegated_bonds post: \
                 {redel_bonds_post:#?},\nmodified_redelegation: \
                 {modified_redelegation:#?},\nbonds_to_unbond: \
                 {bonds_to_unbond:#?}",
                cur_bond.to_string_native(),
                redelegated_deltas.to_string_native()
            );
        }
    }

    // Tally rewards (only call if this is not the first epoch)
    if current_epoch > Epoch::default() {
        let mut rewards = token::Amount::zero();

        let last_claim_epoch =
            get_last_reward_claim_epoch(storage, source, validator)?
                .unwrap_or_default();
        let rewards_products = validator_rewards_products_handle(validator);

        for (start_epoch, slashed_amount) in &result_slashing.epoch_map {
            // Stop collecting rewards at the moment the unbond is initiated
            // (right now)
            for ep in
                Epoch::iter_bounds_inclusive(*start_epoch, current_epoch.prev())
            {
                // Consider the last epoch when rewards were claimed
                if ep < last_claim_epoch {
                    continue;
                }
                let rp =
                    rewards_products.get(storage, &ep)?.unwrap_or_default();
                rewards += rp * (*slashed_amount);
            }
        }

        // Update the rewards from the current unbonds first
        add_rewards_to_counter(storage, source, validator, rewards)?;
    }

    Ok(result_slashing)
}

#[derive(Debug, Default, Eq, PartialEq)]
struct FoldRedelegatedBondsResult {
    total_redelegated: token::Amount,
    total_after_slashing: token::Amount,
}

/// Iterates over a `redelegated_unbonds` and computes the both the sum of all
/// redelegated tokens and how much is left after applying all relevant slashes.
// `def foldAndSlashRedelegatedBondsMap`
fn fold_and_slash_redelegated_bonds<S>(
    storage: &S,
    params: &OwnedPosParams,
    redelegated_unbonds: &EagerRedelegatedBondsMap,
    start_epoch: Epoch,
    list_slashes: &[Slash],
    slash_epoch_filter: impl Fn(Epoch) -> bool,
) -> FoldRedelegatedBondsResult
where
    S: StorageRead,
{
    let mut result = FoldRedelegatedBondsResult::default();
    for (src_validator, bonds_map) in redelegated_unbonds {
        for (bond_start, &change) in bonds_map {
            // Merge the two lists of slashes
            let mut merged: Vec<Slash> =
            // Look-up slashes for this validator ...
                validator_slashes_handle(src_validator)
                    .iter(storage)
                    .unwrap()
                    .map(Result::unwrap)
                    .filter(|slash| {
                        params.in_redelegation_slashing_window(
                            slash.epoch,
                            params.redelegation_start_epoch_from_end(
                                start_epoch,
                            ),
                            start_epoch,
                        ) && *bond_start <= slash.epoch
                            && slash_epoch_filter(slash.epoch)
                    })
                    // ... and add `list_slashes`
                    .chain(list_slashes.iter().cloned())
                    .collect();

            // Sort slashes by epoch
            merged.sort_by(|s1, s2| s1.epoch.partial_cmp(&s2.epoch).unwrap());

            result.total_redelegated += change;
            result.total_after_slashing +=
                apply_list_slashes(params, &merged, change);
        }
    }
    result
}

/// Epochs for full and partial unbonds.
#[derive(Debug, Default)]
struct BondsForRemovalRes {
    /// Full unbond epochs
    pub epochs: BTreeSet<Epoch>,
    /// Partial unbond epoch associated with the new bond amount
    pub new_entry: Option<(Epoch, token::Amount)>,
}

/// In decreasing epoch order, decrement the non-zero bond amount entries until
/// the full `amount` has been removed. Returns a `BondsForRemovalRes` object
/// that contains the epochs for which the full bond amount is removed and
/// additionally information for the one epoch whose bond amount is partially
/// removed, if any.
fn find_bonds_to_remove<S>(
    storage: &S,
    bonds_handle: &LazyMap<Epoch, token::Amount>,
    amount: token::Amount,
) -> storage_api::Result<BondsForRemovalRes>
where
    S: StorageRead,
{
    #[allow(clippy::needless_collect)]
    let bonds: Vec<Result<_, _>> = bonds_handle.iter(storage)?.collect();

    let mut bonds_for_removal = BondsForRemovalRes::default();
    let mut remaining = amount;

    for bond in bonds.into_iter().rev() {
        let (bond_epoch, bond_amount) = bond?;
        let to_unbond = cmp::min(bond_amount, remaining);
        if to_unbond == bond_amount {
            bonds_for_removal.epochs.insert(bond_epoch);
        } else {
            bonds_for_removal.new_entry =
                Some((bond_epoch, bond_amount - to_unbond));
        }
        remaining -= to_unbond;
        if remaining.is_zero() {
            break;
        }
    }
    Ok(bonds_for_removal)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ModifiedRedelegation {
    epoch: Option<Epoch>,
    validators_to_remove: BTreeSet<Address>,
    validator_to_modify: Option<Address>,
    epochs_to_remove: BTreeSet<Epoch>,
    epoch_to_modify: Option<Epoch>,
    new_amount: Option<token::Amount>,
}

/// Used in `fn unbond_tokens` to compute the modified state of a redelegation
/// if redelegated tokens are being unbonded.
fn compute_modified_redelegation<S>(
    storage: &S,
    redelegated_bonds: &RedelegatedTokens,
    start_epoch: Epoch,
    amount_to_unbond: token::Amount,
) -> storage_api::Result<ModifiedRedelegation>
where
    S: StorageRead,
{
    let mut modified_redelegation = ModifiedRedelegation::default();

    let mut src_validators = BTreeSet::<Address>::new();
    let mut total_redelegated = token::Amount::zero();
    for rb in redelegated_bonds.iter(storage)? {
        let (
            NestedSubKey::Data {
                key: src_validator,
                nested_sub_key: _,
            },
            amount,
        ) = rb?;
        total_redelegated += amount;
        src_validators.insert(src_validator);
    }

    modified_redelegation.epoch = Some(start_epoch);

    // If the total amount of redelegated bonds is less than the target amount,
    // then all redelegated bonds must be unbonded.
    if total_redelegated <= amount_to_unbond {
        return Ok(modified_redelegation);
    }

    let mut remaining = amount_to_unbond;
    for src_validator in src_validators.into_iter() {
        if remaining.is_zero() {
            break;
        }
        let rbonds = redelegated_bonds.at(&src_validator);
        let total_src_val_amount = rbonds
            .iter(storage)?
            .map(|res| {
                let (_, amount) = res?;
                Ok(amount)
            })
            .sum::<storage_api::Result<token::Amount>>()?;

        // TODO: move this into the `if total_redelegated <= remaining` branch
        // below, then we don't have to remove it in `fn
        // update_redelegated_bonds` when `validator_to_modify` is Some (and
        // avoid `modified_redelegation.validators_to_remove.clone()`).
        // It affects assumption 2. in `fn compute_new_redelegated_unbonds`, but
        // that looks trivial to change.
        // NOTE: not sure if this TODO is still relevant...
        modified_redelegation
            .validators_to_remove
            .insert(src_validator.clone());
        if total_src_val_amount <= remaining {
            remaining -= total_src_val_amount;
        } else {
            let bonds_to_remove =
                find_bonds_to_remove(storage, &rbonds, remaining)?;

            remaining = token::Amount::zero();

            // NOTE: When there are multiple `src_validators` from which we're
            // unbonding, `validator_to_modify` cannot get overridden, because
            // only one of them can be a partial unbond (`new_entry`
            // is partial unbond)
            if let Some((bond_epoch, new_bond_amount)) =
                bonds_to_remove.new_entry
            {
                modified_redelegation.validator_to_modify = Some(src_validator);
                modified_redelegation.epochs_to_remove = {
                    let mut epochs = bonds_to_remove.epochs;
                    // TODO: remove this insertion then we don't have to remove
                    // it again in `fn update_redelegated_bonds`
                    // when `epoch_to_modify` is Some (and avoid
                    // `modified_redelegation.epochs_to_remove.clone`)
                    // It affects assumption 3. in `fn
                    // compute_new_redelegated_unbonds`, but that also looks
                    // trivial to change.
                    epochs.insert(bond_epoch);
                    epochs
                };
                modified_redelegation.epoch_to_modify = Some(bond_epoch);
                modified_redelegation.new_amount = Some(new_bond_amount);
            } else {
                modified_redelegation.validator_to_modify = Some(src_validator);
                modified_redelegation.epochs_to_remove = bonds_to_remove.epochs;
            }
        }
    }
    Ok(modified_redelegation)
}

fn update_redelegated_bonds<S>(
    storage: &mut S,
    redelegated_bonds: &RedelegatedTokens,
    modified_redelegation: &ModifiedRedelegation,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    if let Some(val_to_modify) = &modified_redelegation.validator_to_modify {
        let mut updated_vals_to_remove =
            modified_redelegation.validators_to_remove.clone();
        updated_vals_to_remove.remove(val_to_modify);

        // Remove the updated_vals_to_remove keys from the
        // redelegated_bonds map
        for val in &updated_vals_to_remove {
            redelegated_bonds.remove_all(storage, val)?;
        }

        if let Some(epoch_to_modify) = modified_redelegation.epoch_to_modify {
            let mut updated_epochs_to_remove =
                modified_redelegation.epochs_to_remove.clone();
            updated_epochs_to_remove.remove(&epoch_to_modify);
            let val_bonds_to_modify = redelegated_bonds.at(val_to_modify);
            for epoch in updated_epochs_to_remove {
                val_bonds_to_modify.remove(storage, &epoch)?;
            }
            val_bonds_to_modify.insert(
                storage,
                epoch_to_modify,
                modified_redelegation.new_amount.unwrap(),
            )?;
        } else {
            // Then remove to epochs_to_remove from the redelegated bonds of the
            // val_to_modify
            let val_bonds_to_modify = redelegated_bonds.at(val_to_modify);
            for epoch in &modified_redelegation.epochs_to_remove {
                val_bonds_to_modify.remove(storage, epoch)?;
            }
        }
    } else {
        // Remove all validators in modified_redelegation.validators_to_remove
        // from redelegated_bonds
        for val in &modified_redelegation.validators_to_remove {
            redelegated_bonds.remove_all(storage, val)?;
        }
    }
    Ok(())
}

/// Temp helper type to match quint model.
/// Result of `compute_new_redelegated_unbonds` that contains a map of
/// redelegated unbonds.
/// The map keys from outside in are:
///
/// - redelegation end epoch where redeleg stops contributing to src validator
/// - src validator address
/// - src bond start epoch where it started contributing to src validator
type EagerRedelegatedUnbonds = BTreeMap<Epoch, EagerRedelegatedBondsMap>;

/// Computes a map of redelegated unbonds from a set of redelegated bonds.
///
/// - `redelegated_bonds` - a map of redelegated bonds from epoch to
///   `RedelegatedTokens`.
/// - `epochs_to_remove` - a set of epochs that indicate the set of epochs
///   unbonded.
/// - `modified` record that represents a redelegated bond that it is only
///   partially unbonded.
///
/// The function assumes that:
///
/// 1. `modified.epoch` is not in the `epochs_to_remove` set.
/// 2. `modified.validator_to_modify` is in `modified.vals_to_remove`.
/// 3. `modified.epoch_to_modify` is in in `modified.epochs_to_remove`.
// `def computeNewRedelegatedUnbonds` from Quint
fn compute_new_redelegated_unbonds<S>(
    storage: &S,
    redelegated_bonds: &RedelegatedBondsOrUnbonds,
    epochs_to_remove: &BTreeSet<Epoch>,
    modified: &ModifiedRedelegation,
) -> storage_api::Result<EagerRedelegatedUnbonds>
where
    S: StorageRead + StorageWrite,
{
    let unbonded_epochs = if let Some(epoch) = modified.epoch {
        debug_assert!(
            !epochs_to_remove.contains(&epoch),
            "1. assumption in `fn compute_new_redelegated_unbonds` doesn't \
             hold"
        );
        let mut epochs = epochs_to_remove.clone();
        epochs.insert(epoch);
        epochs
            .iter()
            .cloned()
            .filter(|e| redelegated_bonds.contains(storage, e).unwrap())
            .collect::<BTreeSet<Epoch>>()
    } else {
        epochs_to_remove
            .iter()
            .cloned()
            .filter(|e| redelegated_bonds.contains(storage, e).unwrap())
            .collect::<BTreeSet<Epoch>>()
    };
    debug_assert!(
        modified
            .validator_to_modify
            .as_ref()
            .map(|validator| modified.validators_to_remove.contains(validator))
            .unwrap_or(true),
        "2. assumption in `fn compute_new_redelegated_unbonds` doesn't hold"
    );
    debug_assert!(
        modified
            .epoch_to_modify
            .as_ref()
            .map(|epoch| modified.epochs_to_remove.contains(epoch))
            .unwrap_or(true),
        "3. assumption in `fn compute_new_redelegated_unbonds` doesn't hold"
    );

    // quint `newRedelegatedUnbonds` returned from
    // `computeNewRedelegatedUnbonds`
    let new_redelegated_unbonds: EagerRedelegatedUnbonds = unbonded_epochs
        .into_iter()
        .map(|start| {
            let mut rbonds = EagerRedelegatedBondsMap::default();
            if modified
                .epoch
                .map(|redelegation_epoch| start != redelegation_epoch)
                .unwrap_or(true)
                || modified.validators_to_remove.is_empty()
            {
                for res in redelegated_bonds.at(&start).iter(storage).unwrap() {
                    let (
                        NestedSubKey::Data {
                            key: validator,
                            nested_sub_key: SubKey::Data(epoch),
                        },
                        amount,
                    ) = res.unwrap();
                    rbonds
                        .entry(validator.clone())
                        .or_default()
                        .insert(epoch, amount);
                }
                (start, rbonds)
            } else {
                for src_validator in &modified.validators_to_remove {
                    if modified
                        .validator_to_modify
                        .as_ref()
                        .map(|validator| src_validator != validator)
                        .unwrap_or(true)
                    {
                        let raw_bonds =
                            redelegated_bonds.at(&start).at(src_validator);
                        for res in raw_bonds.iter(storage).unwrap() {
                            let (bond_epoch, bond_amount) = res.unwrap();
                            rbonds
                                .entry(src_validator.clone())
                                .or_default()
                                .insert(bond_epoch, bond_amount);
                        }
                    } else {
                        for bond_start in &modified.epochs_to_remove {
                            let cur_redel_bond_amount = redelegated_bonds
                                .at(&start)
                                .at(src_validator)
                                .get(storage, bond_start)
                                .unwrap()
                                .unwrap_or_default();
                            let raw_bonds = rbonds
                                .entry(src_validator.clone())
                                .or_default();
                            if modified
                                .epoch_to_modify
                                .as_ref()
                                .map(|epoch| bond_start != epoch)
                                .unwrap_or(true)
                            {
                                raw_bonds
                                    .insert(*bond_start, cur_redel_bond_amount);
                            } else {
                                raw_bonds.insert(
                                    *bond_start,
                                    cur_redel_bond_amount
                                        - modified
                                            .new_amount
                                            // Safe unwrap - it shouldn't
                                            // get to
                                            // this if it's None
                                            .unwrap(),
                                );
                            }
                        }
                    }
                }
                (start, rbonds)
            }
        })
        .collect();

    Ok(new_redelegated_unbonds)
}

/// Arguments to [`become_validator`].
pub struct BecomeValidator<'a> {
    /// Proof-of-stake parameters.
    pub params: &'a PosParams,
    /// The validator's address.
    pub address: &'a Address,
    /// The validator's consensus key, used by Tendermint.
    pub consensus_key: &'a common::PublicKey,
    /// The validator's protocol key.
    pub protocol_key: &'a common::PublicKey,
    /// The validator's Ethereum bridge cold key.
    pub eth_cold_key: &'a common::PublicKey,
    /// The validator's Ethereum bridge hot key.
    pub eth_hot_key: &'a common::PublicKey,
    /// The numeric value of the current epoch.
    pub current_epoch: Epoch,
    /// Commission rate.
    pub commission_rate: Dec,
    /// Max commission rate change.
    pub max_commission_rate_change: Dec,
    /// Validator metadata
    pub metadata: ValidatorMetaData,
    /// Optional offset to use instead of pipeline offset
    pub offset_opt: Option<u64>,
}

/// Initialize data for a new validator.
pub fn become_validator<S>(
    storage: &mut S,
    args: BecomeValidator<'_>,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    let BecomeValidator {
        params,
        address,
        consensus_key,
        protocol_key,
        eth_cold_key,
        eth_hot_key,
        current_epoch,
        commission_rate,
        max_commission_rate_change,
        metadata,
        offset_opt,
    } = args;
    let offset = offset_opt.unwrap_or(params.pipeline_len);

    if !address.is_established() {
        return Err(storage_api::Error::new_const(
            "The given address {address} is not established. Only an \
             established address can become a validator.",
        ));
    }

    if is_validator(storage, address)? {
        return Err(storage_api::Error::new_const(
            "The given address is already a validator",
        ));
    }

    // If the address is not yet a validator, it cannot have self-bonds, but it
    // may have delegations.
    if has_bonds(storage, address)? {
        return Err(storage_api::Error::new_const(
            "The given address has delegations and therefore cannot become a \
             validator. Unbond first.",
        ));
    }

    // This will fail if the key is already being used
    try_insert_consensus_key(storage, consensus_key)?;

    let pipeline_epoch = current_epoch + offset;
    validator_addresses_handle()
        .at(&pipeline_epoch)
        .insert(storage, address.clone())?;

    // Non-epoched validator data
    write_validator_address_raw_hash(storage, address, consensus_key)?;
    write_validator_max_commission_rate_change(
        storage,
        address,
        max_commission_rate_change,
    )?;
    write_validator_metadata(storage, address, &metadata)?;

    // Epoched validator data
    validator_consensus_key_handle(address).set(
        storage,
        consensus_key.clone(),
        current_epoch,
        offset,
    )?;
    validator_protocol_key_handle(address).set(
        storage,
        protocol_key.clone(),
        current_epoch,
        offset,
    )?;
    validator_eth_hot_key_handle(address).set(
        storage,
        eth_hot_key.clone(),
        current_epoch,
        offset,
    )?;
    validator_eth_cold_key_handle(address).set(
        storage,
        eth_cold_key.clone(),
        current_epoch,
        offset,
    )?;
    validator_commission_rate_handle(address).set(
        storage,
        commission_rate,
        current_epoch,
        offset,
    )?;
    validator_deltas_handle(address).set(
        storage,
        token::Change::zero(),
        current_epoch,
        offset,
    )?;

    // The validator's stake at initialization is 0, so its state is immediately
    // below-threshold
    validator_state_handle(address).set(
        storage,
        ValidatorState::BelowThreshold,
        current_epoch,
        offset,
    )?;

    insert_validator_into_validator_set(
        storage,
        params,
        address,
        token::Amount::zero(),
        current_epoch,
        offset,
    )?;

    Ok(())
}

/// Consensus key change for a validator
pub fn change_consensus_key<S>(
    storage: &mut S,
    validator: &Address,
    consensus_key: &common::PublicKey,
    current_epoch: Epoch,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    tracing::debug!("Changing consensus key for validator {}", validator);

    // Require that the new consensus key is an Ed25519 key
    match consensus_key {
        common::PublicKey::Ed25519(_) => {}
        common::PublicKey::Secp256k1(_) => {
            return Err(ConsensusKeyChangeError::MustBeEd25519.into());
        }
    }

    // Check for uniqueness of the consensus key
    try_insert_consensus_key(storage, consensus_key)?;

    // Set the new consensus key at the pipeline epoch
    let params = read_pos_params(storage)?;
    validator_consensus_key_handle(validator).set(
        storage,
        consensus_key.clone(),
        current_epoch,
        params.pipeline_len,
    )?;

    // Write validator's new raw hash
    write_validator_address_raw_hash(storage, validator, consensus_key)?;

    Ok(())
}

/// Withdraw tokens from those that have been unbonded from proof-of-stake
pub fn withdraw_tokens<S>(
    storage: &mut S,
    source: Option<&Address>,
    validator: &Address,
    current_epoch: Epoch,
) -> storage_api::Result<token::Amount>
where
    S: StorageRead + StorageWrite,
{
    let params = read_pos_params(storage)?;
    let source = source.unwrap_or(validator);

    tracing::debug!("Withdrawing tokens in epoch {current_epoch}");
    tracing::debug!("Source {} --> Validator {}", source, validator);

    let unbond_handle: Unbonds = unbond_handle(source, validator);
    let redelegated_unbonds =
        delegator_redelegated_unbonds_handle(source).at(validator);

    // Check that there are unbonded tokens available for withdrawal
    if unbond_handle.is_empty(storage)? {
        return Err(WithdrawError::NoUnbondFound(BondId {
            source: source.clone(),
            validator: validator.clone(),
        })
        .into());
    }

    let mut unbonds_and_redelegated_unbonds: BTreeMap<
        (Epoch, Epoch),
        (token::Amount, EagerRedelegatedBondsMap),
    > = BTreeMap::new();

    for unbond in unbond_handle.iter(storage)? {
        let (
            NestedSubKey::Data {
                key: start_epoch,
                nested_sub_key: SubKey::Data(withdraw_epoch),
            },
            amount,
        ) = unbond?;

        // Logging
        tracing::debug!(
            "Unbond delta ({start_epoch}..{withdraw_epoch}), amount {}",
            amount.to_string_native()
        );
        // Consider only unbonds that are eligible to be withdrawn
        if withdraw_epoch > current_epoch {
            tracing::debug!(
                "Not yet withdrawable until epoch {withdraw_epoch}"
            );
            continue;
        }

        let mut eager_redelegated_unbonds = EagerRedelegatedBondsMap::default();
        let matching_redelegated_unbonds =
            redelegated_unbonds.at(&start_epoch).at(&withdraw_epoch);
        for ub in matching_redelegated_unbonds.iter(storage)? {
            let (
                NestedSubKey::Data {
                    key: address,
                    nested_sub_key: SubKey::Data(epoch),
                },
                amount,
            ) = ub?;
            eager_redelegated_unbonds
                .entry(address)
                .or_default()
                .entry(epoch)
                .or_insert(amount);
        }

        unbonds_and_redelegated_unbonds.insert(
            (start_epoch, withdraw_epoch),
            (amount, eager_redelegated_unbonds),
        );
    }

    let slashes = find_validator_slashes(storage, validator)?;

    // `val resultSlashing`
    let result_slashing = compute_amount_after_slashing_withdraw(
        storage,
        &params,
        &unbonds_and_redelegated_unbonds,
        slashes,
    )?;

    let withdrawable_amount = result_slashing.sum;
    tracing::debug!(
        "Withdrawing total {}",
        withdrawable_amount.to_string_native()
    );

    // `updateDelegator` with `unbonded` and `redelegeatedUnbonded`
    for ((start_epoch, withdraw_epoch), _unbond_and_redelegations) in
        unbonds_and_redelegated_unbonds
    {
        tracing::debug!("Remove ({start_epoch}..{withdraw_epoch}) from unbond");
        unbond_handle
            .at(&start_epoch)
            .remove(storage, &withdraw_epoch)?;
        redelegated_unbonds
            .at(&start_epoch)
            .remove_all(storage, &withdraw_epoch)?;

        if unbond_handle.at(&start_epoch).is_empty(storage)? {
            unbond_handle.remove_all(storage, &start_epoch)?;
        }
        if redelegated_unbonds.at(&start_epoch).is_empty(storage)? {
            redelegated_unbonds.remove_all(storage, &start_epoch)?;
        }
    }

    // Transfer the withdrawable tokens from the PoS address back to the source
    let staking_token = staking_token_address(storage);
    token::transfer(
        storage,
        &staking_token,
        &ADDRESS,
        source,
        withdrawable_amount,
    )?;

    // TODO: Transfer the slashed tokens from the PoS address to the Slash Pool
    // address
    // token::transfer(
    //     storage,
    //     &staking_token,
    //     &ADDRESS,
    //     &SLASH_POOL_ADDRESS,
    //     total_slashed,
    // )?;

    Ok(withdrawable_amount)
}

/// Change the commission rate of a validator
pub fn change_validator_commission_rate<S>(
    storage: &mut S,
    validator: &Address,
    new_rate: Dec,
    current_epoch: Epoch,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    if new_rate.is_negative() {
        return Err(CommissionRateChangeError::NegativeRate(
            new_rate,
            validator.clone(),
        )
        .into());
    }

    if new_rate > Dec::one() {
        return Err(CommissionRateChangeError::LargerThanOne(
            new_rate,
            validator.clone(),
        )
        .into());
    }

    let max_change =
        read_validator_max_commission_rate_change(storage, validator)?;
    if max_change.is_none() {
        return Err(CommissionRateChangeError::NoMaxSetInStorage(
            validator.clone(),
        )
        .into());
    }

    let params = read_pos_params(storage)?;
    let commission_handle = validator_commission_rate_handle(validator);
    let pipeline_epoch = current_epoch + params.pipeline_len;

    let rate_at_pipeline = commission_handle
        .get(storage, pipeline_epoch, &params)?
        .expect("Could not find a rate in given epoch");
    if new_rate == rate_at_pipeline {
        return Ok(());
    }
    let rate_before_pipeline = commission_handle
        .get(storage, pipeline_epoch.prev(), &params)?
        .expect("Could not find a rate in given epoch");

    let change_from_prev = new_rate.abs_diff(&rate_before_pipeline);
    if change_from_prev > max_change.unwrap() {
        return Err(CommissionRateChangeError::RateChangeTooLarge(
            change_from_prev,
            validator.clone(),
        )
        .into());
    }

    commission_handle.set(storage, new_rate, current_epoch, params.pipeline_len)
}

/// Get the total bond amount, including slashes, for a given bond ID and epoch.
/// Returns the bond amount after slashing. For future epochs the value is
/// subject to change.
pub fn bond_amount<S>(
    storage: &S,
    bond_id: &BondId,
    epoch: Epoch,
) -> storage_api::Result<token::Amount>
where
    S: StorageRead,
{
    let params = read_pos_params(storage)?;
    // Outer key is the start epoch used to calculate slashes.
    let mut amounts: BTreeMap<Epoch, token::Amount> = BTreeMap::default();

    // Bonds
    let bonds =
        bond_handle(&bond_id.source, &bond_id.validator).get_data_handler();
    for next in bonds.iter(storage)? {
        let (start, delta) = next?;
        if start <= epoch {
            let amount = amounts.entry(start).or_default();
            *amount += delta;
        }
    }

    // Add unbonds that are still contributing to stake
    let unbonds = unbond_handle(&bond_id.source, &bond_id.validator);
    for next in unbonds.iter(storage)? {
        let (
            NestedSubKey::Data {
                key: start,
                nested_sub_key: SubKey::Data(withdrawable_epoch),
            },
            delta,
        ) = next?;
        // This is the first epoch in which the unbond stops contributing to
        // voting power
        let end = withdrawable_epoch - params.withdrawable_epoch_offset()
            + params.pipeline_len;

        if start <= epoch && end > epoch {
            let amount = amounts.entry(start).or_default();
            *amount += delta;
        }
    }

    // Redelegations
    if bond_id.validator != bond_id.source {
        // Add outgoing redelegations that are still contributing to the source
        // validator's stake
        let redelegated_bonds =
            delegator_redelegated_bonds_handle(&bond_id.source);
        for res in redelegated_bonds.iter(storage)? {
            let (
                NestedSubKey::Data {
                    key: _dest_validator,
                    nested_sub_key:
                        NestedSubKey::Data {
                            key: end,
                            nested_sub_key:
                                NestedSubKey::Data {
                                    key: src_validator,
                                    nested_sub_key: SubKey::Data(start),
                                },
                        },
                },
                delta,
            ) = res?;
            if src_validator == bond_id.validator
                && start <= epoch
                && end > epoch
            {
                let amount = amounts.entry(start).or_default();
                *amount += delta;
            }
        }

        // Add outgoing redelegation unbonds that are still contributing to
        // the source validator's stake
        let redelegated_unbonds =
            delegator_redelegated_unbonds_handle(&bond_id.source);
        for res in redelegated_unbonds.iter(storage)? {
            let (
                NestedSubKey::Data {
                    key: _dest_validator,
                    nested_sub_key:
                        NestedSubKey::Data {
                            key: redelegation_epoch,
                            nested_sub_key:
                                NestedSubKey::Data {
                                    key: _withdraw_epoch,
                                    nested_sub_key:
                                        NestedSubKey::Data {
                                            key: src_validator,
                                            nested_sub_key: SubKey::Data(start),
                                        },
                                },
                        },
                },
                delta,
            ) = res?;
            if src_validator == bond_id.validator
                // If the unbonded bond was redelegated after this epoch ...
                && redelegation_epoch > epoch
                // ... the start was before or at this epoch
                && start <= epoch
            {
                let amount = amounts.entry(start).or_default();
                *amount += delta;
            }
        }
    }

    if !amounts.is_empty() {
        let slashes = find_validator_slashes(storage, &bond_id.validator)?;
        let redelegated_bonded =
            delegator_redelegated_bonds_handle(&bond_id.source)
                .at(&bond_id.validator);

        // Apply slashes
        for (&start, amount) in amounts.iter_mut() {
            let list_slashes = slashes
                .iter()
                .filter(|slash| {
                    let processing_epoch =
                        slash.epoch + params.slash_processing_epoch_offset();
                    // Only use slashes that were processed before or at the
                    // epoch associated with the bond amount. This assumes
                    // that slashes are applied before inflation.
                    processing_epoch <= epoch && start <= slash.epoch
                })
                .cloned()
                .collect::<Vec<_>>();

            let slash_epoch_filter =
                |e: Epoch| e + params.slash_processing_epoch_offset() <= epoch;

            let redelegated_bonds =
                redelegated_bonded.at(&start).collect_map(storage)?;

            let result_fold = fold_and_slash_redelegated_bonds(
                storage,
                &params,
                &redelegated_bonds,
                start,
                &list_slashes,
                slash_epoch_filter,
            );

            let total_not_redelegated = *amount - result_fold.total_redelegated;

            let after_not_redelegated = apply_list_slashes(
                &params,
                &list_slashes,
                total_not_redelegated,
            );

            *amount = after_not_redelegated + result_fold.total_after_slashing;
        }
    }

    Ok(amounts.values().cloned().sum())
}

/// Get bond amounts within the `claim_start..=claim_end` epoch range for
/// claiming rewards for a given bond ID. Returns a map of bond amounts
/// associated with every epoch within the given epoch range (accumulative) in
/// which an amount contributed to the validator's stake.
/// This function will only consider slashes that were processed before or at
/// the epoch in which we're calculating the bond amount to correspond to the
/// validator stake that was used to calculate reward products (slashes do *not*
/// retrospectively affect the rewards calculated before slash processing).
pub fn bond_amounts_for_rewards<S>(
    storage: &S,
    bond_id: &BondId,
    claim_start: Epoch,
    claim_end: Epoch,
) -> storage_api::Result<BTreeMap<Epoch, token::Amount>>
where
    S: StorageRead,
{
    let params = read_pos_params(storage)?;
    // Outer key is every epoch in which the a bond amount contributed to stake
    // and the inner key is the start epoch used to calculate slashes. The inner
    // keys are discarded after applying slashes.
    let mut amounts: BTreeMap<Epoch, BTreeMap<Epoch, token::Amount>> =
        BTreeMap::default();

    // Only need to do bonds since rewards are accumulated during
    // `unbond_tokens`
    let bonds =
        bond_handle(&bond_id.source, &bond_id.validator).get_data_handler();
    for next in bonds.iter(storage)? {
        let (start, delta) = next?;

        for ep in Epoch::iter_bounds_inclusive(claim_start, claim_end) {
            // A bond that wasn't unbonded is added to all epochs up to
            // `claim_end`
            if start <= ep {
                let amount =
                    amounts.entry(ep).or_default().entry(start).or_default();
                *amount += delta;
            }
        }
    }

    if !amounts.is_empty() {
        let slashes = find_validator_slashes(storage, &bond_id.validator)?;
        let redelegated_bonded =
            delegator_redelegated_bonds_handle(&bond_id.source)
                .at(&bond_id.validator);

        // Apply slashes
        for (&ep, amounts) in amounts.iter_mut() {
            for (&start, amount) in amounts.iter_mut() {
                let list_slashes = slashes
                    .iter()
                    .filter(|slash| {
                        let processing_epoch = slash.epoch
                            + params.slash_processing_epoch_offset();
                        // Only use slashes that were processed before or at the
                        // epoch associated with the bond amount. This assumes
                        // that slashes are applied before inflation.
                        processing_epoch <= ep && start <= slash.epoch
                    })
                    .cloned()
                    .collect::<Vec<_>>();

                let slash_epoch_filter =
                    |e: Epoch| e + params.slash_processing_epoch_offset() <= ep;

                let redelegated_bonds =
                    redelegated_bonded.at(&start).collect_map(storage)?;

                let result_fold = fold_and_slash_redelegated_bonds(
                    storage,
                    &params,
                    &redelegated_bonds,
                    start,
                    &list_slashes,
                    slash_epoch_filter,
                );

                let total_not_redelegated =
                    *amount - result_fold.total_redelegated;

                let after_not_redelegated = apply_list_slashes(
                    &params,
                    &list_slashes,
                    total_not_redelegated,
                );

                *amount =
                    after_not_redelegated + result_fold.total_after_slashing;
            }
        }
    }

    Ok(amounts
        .into_iter()
        // Flatten the inner maps to discard bond start epochs
        .map(|(ep, amounts)| (ep, amounts.values().cloned().sum()))
        .collect())
}

/// Get the genesis consensus validators stake and consensus key for Tendermint,
/// converted from [`ValidatorSetUpdate`]s using the given function.
pub fn genesis_validator_set_tendermint<S, T>(
    storage: &S,
    params: &PosParams,
    current_epoch: Epoch,
    mut f: impl FnMut(ValidatorSetUpdate) -> T,
) -> storage_api::Result<Vec<T>>
where
    S: StorageRead,
{
    let consensus_validator_handle =
        consensus_validator_set_handle().at(&current_epoch);
    let iter = consensus_validator_handle.iter(storage)?;

    iter.map(|validator| {
        let (
            NestedSubKey::Data {
                key: new_stake,
                nested_sub_key: _,
            },
            address,
        ) = validator?;
        let consensus_key = validator_consensus_key_handle(&address)
            .get(storage, current_epoch, params)?
            .unwrap();
        let converted = f(ValidatorSetUpdate::Consensus(ConsensusValidator {
            consensus_key,
            bonded_stake: new_stake,
        }));
        Ok(converted)
    })
    .collect()
}

/// Unjail a validator that is currently jailed.
pub fn unjail_validator<S>(
    storage: &mut S,
    validator: &Address,
    current_epoch: Epoch,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    let params = read_pos_params(storage)?;

    // Check that the validator is jailed up to the pipeline epoch
    for epoch in current_epoch.iter_range(params.pipeline_len + 1) {
        let state =
            validator_state_handle(validator).get(storage, epoch, &params)?;
        if let Some(state) = state {
            if state != ValidatorState::Jailed {
                return Err(UnjailValidatorError::NotJailed(
                    validator.clone(),
                    epoch,
                )
                .into());
            }
        } else {
            return Err(
                UnjailValidatorError::NotAValidator(validator.clone()).into()
            );
        }
    }

    // Check that the unjailing tx can be submitted given the current epoch
    // and the most recent infraction epoch
    let last_slash_epoch = read_validator_last_slash_epoch(storage, validator)?;
    if let Some(last_slash_epoch) = last_slash_epoch {
        let eligible_epoch =
            last_slash_epoch + params.slash_processing_epoch_offset();
        if current_epoch < eligible_epoch {
            return Err(UnjailValidatorError::NotEligible(
                validator.clone(),
                eligible_epoch,
                current_epoch,
            )
            .into());
        }
    }

    // Re-insert the validator into the validator set and update its state
    let pipeline_epoch = current_epoch + params.pipeline_len;
    let stake =
        read_validator_stake(storage, &params, validator, pipeline_epoch)?;

    insert_validator_into_validator_set(
        storage,
        &params,
        validator,
        stake,
        current_epoch,
        params.pipeline_len,
    )?;
    Ok(())
}

/// Check if a validator is frozen. A validator is frozen until after all of its
/// enqueued slashes have been processed, i.e. until `unbonding_len + 1 +
/// cubic_slashing_window_length` epochs after its most recent infraction epoch.
pub fn is_validator_frozen<S>(
    storage: &S,
    validator: &Address,
    current_epoch: Epoch,
    params: &PosParams,
) -> storage_api::Result<bool>
where
    S: StorageRead,
{
    let last_infraction_epoch =
        read_validator_last_slash_epoch(storage, validator)?;
    if let Some(last_epoch) = last_infraction_epoch {
        let is_frozen =
            current_epoch < last_epoch + params.slash_processing_epoch_offset();
        Ok(is_frozen)
    } else {
        Ok(false)
    }
}

/// Find the total amount of tokens staked at the given `epoch`,
/// belonging to the set of consensus validators.
pub fn get_total_consensus_stake<S>(
    storage: &S,
    epoch: Epoch,
    params: &PosParams,
) -> storage_api::Result<token::Amount>
where
    S: StorageRead,
{
    total_consensus_stake_handle()
        .get(storage, epoch, params)
        .map(|o| o.expect("Total consensus stake could not be retrieved."))
}

/// Redelegate bonded tokens from a source validator to a destination validator
pub fn redelegate_tokens<S>(
    storage: &mut S,
    delegator: &Address,
    src_validator: &Address,
    dest_validator: &Address,
    current_epoch: Epoch,
    amount: token::Amount,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    tracing::debug!(
        "Delegator {} redelegating {} tokens from {} to {}",
        delegator,
        amount.to_string_native(),
        src_validator,
        dest_validator
    );
    if amount.is_zero() {
        return Ok(());
    }

    // The src and dest validators must be different
    if src_validator == dest_validator {
        return Err(RedelegationError::RedelegationSrcEqDest.into());
    }

    // The delegator must not be a validator
    if is_validator(storage, delegator)? {
        return Err(RedelegationError::DelegatorIsValidator.into());
    }

    // The src and dest validators must actually be validators
    if !is_validator(storage, src_validator)? {
        return Err(
            RedelegationError::NotAValidator(src_validator.clone()).into()
        );
    }
    if !is_validator(storage, dest_validator)? {
        return Err(
            RedelegationError::NotAValidator(dest_validator.clone()).into()
        );
    }

    let params = read_pos_params(storage)?;
    let pipeline_epoch = current_epoch + params.pipeline_len;
    let src_redel_end_epoch =
        validator_incoming_redelegations_handle(src_validator)
            .get(storage, delegator)?;

    // Forbid chained redelegations. A redelegation is "chained" if:
    // 1. the source validator holds bonded tokens that themselves were
    // redelegated to the src validator
    // 2. given the latest epoch at which the most recently redelegated tokens
    // started contributing to the src validator's voting power, these tokens
    // cannot be slashed anymore
    let is_not_chained = if let Some(end_epoch) = src_redel_end_epoch {
        let last_contrib_epoch = end_epoch.prev();
        // If the source validator's slashes that would cause slash on
        // redelegation are now outdated (would have to be processed before or
        // on start of the current epoch), the redelegation can be redelegated
        // again
        last_contrib_epoch + params.slash_processing_epoch_offset()
            <= current_epoch
    } else {
        true
    };
    if !is_not_chained {
        return Err(RedelegationError::IsChainedRedelegation.into());
    }

    // Unbond the redelegated tokens from the src validator.
    // `resultUnbond` in quint
    let result_unbond = unbond_tokens(
        storage,
        Some(delegator),
        src_validator,
        amount,
        current_epoch,
        true,
    )?;

    // The unbonded amount after slashing is what is going to be redelegated.
    // `amountAfterSlashing`
    let amount_after_slashing = result_unbond.sum;
    tracing::debug!(
        "Redelegated amount after slashing: {}",
        amount_after_slashing.to_string_native()
    );

    // Add incoming redelegated bonds to the dest validator.
    // `updatedRedelegatedBonds` with updates to delegatorState
    // `redelegatedBonded`
    let redelegated_bonds = delegator_redelegated_bonds_handle(delegator)
        .at(dest_validator)
        .at(&pipeline_epoch)
        .at(src_validator);
    for (&epoch, &unbonded_amount) in result_unbond.epoch_map.iter() {
        redelegated_bonds.update(storage, epoch, |current| {
            current.unwrap_or_default() + unbonded_amount
        })?;
    }

    if tracing::level_enabled!(tracing::Level::DEBUG) {
        let bonds = find_bonds(storage, delegator, dest_validator)?;
        tracing::debug!("\nRedeleg dest bonds before incrementing: {bonds:#?}");
    }

    // Add a bond delta to the destination.
    if !amount_after_slashing.is_zero() {
        // `updatedDelegator` with updates to `bonded`
        let bond_handle = bond_handle(delegator, dest_validator);
        bond_handle.add(
            storage,
            amount_after_slashing,
            current_epoch,
            params.pipeline_len,
        )?;
        // `updatedDestValidator` --> `with("totalVBonded")`
        // Add the amount to the dest validator total bonded
        let dest_total_bonded = total_bonded_handle(dest_validator);
        dest_total_bonded.add(
            storage,
            amount_after_slashing,
            current_epoch,
            params.pipeline_len,
        )?;
    }

    if tracing::level_enabled!(tracing::Level::DEBUG) {
        let bonds = find_bonds(storage, delegator, dest_validator)?;
        tracing::debug!("\nRedeleg dest bonds after incrementing: {bonds:#?}");
    }

    // Add outgoing redelegation to the src validator.
    // `updateOutgoingRedelegations` with `updatedSrcValidator`
    let outgoing_redelegations =
        validator_outgoing_redelegations_handle(src_validator)
            .at(dest_validator);
    for (start, &unbonded_amount) in result_unbond.epoch_map.iter() {
        outgoing_redelegations.at(start).update(
            storage,
            current_epoch,
            |current| current.unwrap_or_default() + unbonded_amount,
        )?;
    }

    // Add the amount to the dest validator total redelegated bonds.
    let dest_total_redelegated_bonded =
        validator_total_redelegated_bonded_handle(dest_validator)
            .at(&pipeline_epoch)
            .at(src_validator);
    for (&epoch, &amount) in &result_unbond.epoch_map {
        dest_total_redelegated_bonded.update(storage, epoch, |current| {
            current.unwrap_or_default() + amount
        })?;
    }

    // Set the epoch of the validator incoming redelegation from this delegator
    let dest_incoming_redelegations =
        validator_incoming_redelegations_handle(dest_validator);
    dest_incoming_redelegations.insert(
        storage,
        delegator.clone(),
        pipeline_epoch,
    )?;

    // Update validator set for dest validator
    let is_jailed_or_inactive_at_pipeline = matches!(
        validator_state_handle(dest_validator).get(
            storage,
            pipeline_epoch,
            &params
        )?,
        Some(ValidatorState::Jailed) | Some(ValidatorState::Inactive)
    );
    if !is_jailed_or_inactive_at_pipeline {
        update_validator_set(
            storage,
            &params,
            dest_validator,
            amount_after_slashing.change(),
            current_epoch,
            None,
        )?;
    }

    // Update deltas
    update_validator_deltas(
        storage,
        &params,
        dest_validator,
        amount_after_slashing.change(),
        current_epoch,
        None,
    )?;
    update_total_deltas(
        storage,
        &params,
        amount_after_slashing.change(),
        current_epoch,
        None,
    )?;

    Ok(())
}

/// Deactivate a validator by removing it from any validator sets. A validator
/// can only be deactivated if it is not jailed or already inactive.
pub fn deactivate_validator<S>(
    storage: &mut S,
    validator: &Address,
    current_epoch: Epoch,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    let params = read_pos_params(storage)?;
    let pipeline_epoch = current_epoch + params.pipeline_len;

    let pipeline_state = match validator_state_handle(validator).get(
        storage,
        pipeline_epoch,
        &params,
    )? {
        Some(state) => state,
        None => {
            return Err(
                DeactivationError::NotAValidator(validator.clone()).into()
            );
        }
    };

    // Remove the validator from the validator set. If it is in the consensus
    // set, promote the next validator.
    match pipeline_state {
        ValidatorState::Consensus => {
            // Remove from the consensus set first
            remove_consensus_validator(
                storage,
                &params,
                pipeline_epoch,
                validator,
            )?;

            // Promote the next below-capacity validator to consensus
            promote_next_below_capacity_validator_to_consensus(
                storage,
                pipeline_epoch,
            )?;
        }

        ValidatorState::BelowCapacity => {
            remove_below_capacity_validator(
                storage,
                &params,
                pipeline_epoch,
                validator,
            )?;
        }
        ValidatorState::BelowThreshold => {}
        ValidatorState::Inactive => {
            return Err(DeactivationError::AlreadyInactive(
                validator.clone(),
                pipeline_epoch,
            )
            .into());
        }
        ValidatorState::Jailed => {
            return Err(DeactivationError::ValidatorIsJailed(
                validator.clone(),
                pipeline_epoch,
            )
            .into());
        }
    }

    // Set the state to inactive
    validator_state_handle(validator).set(
        storage,
        ValidatorState::Inactive,
        current_epoch,
        params.pipeline_len,
    )?;

    Ok(())
}

/// Re-activate an inactive validator
pub fn reactivate_validator<S>(
    storage: &mut S,
    validator: &Address,
    current_epoch: Epoch,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    let params = read_pos_params(storage)?;
    let pipeline_epoch = current_epoch + params.pipeline_len;

    // Make sure state is Inactive at every epoch up through the pipeline
    for epoch in Epoch::iter_bounds_inclusive(current_epoch, pipeline_epoch) {
        let state =
            validator_state_handle(validator).get(storage, epoch, &params)?;
        if let Some(state) = state {
            if state != ValidatorState::Inactive {
                return Err(ReactivationError::NotInactive(
                    validator.clone(),
                    epoch,
                )
                .into());
            }
        } else {
            return Err(ReactivationError::NoStateFound(
                validator.clone(),
                epoch,
            )
            .into());
        }
    }

    // Check to see if the validator should be jailed upon a reactivation. This
    // may occur if a validator is deactivated but then an infraction is
    // discovered later.
    let last_slash_epoch = read_validator_last_slash_epoch(storage, validator)?;
    if let Some(last_slash_epoch) = last_slash_epoch {
        let eligible_epoch =
            last_slash_epoch + params.slash_processing_epoch_offset();
        if current_epoch < eligible_epoch {
            // The validator should be set back to jailed
            validator_state_handle(validator).set(
                storage,
                ValidatorState::Jailed,
                pipeline_epoch,
                0,
            )?;
            return Ok(());
        }
    }

    // Determine which validator set the validator should be added to again
    let stake =
        read_validator_stake(storage, &params, validator, pipeline_epoch)?;

    insert_validator_into_validator_set(
        storage,
        &params,
        validator,
        stake,
        current_epoch,
        params.pipeline_len,
    )?;

    Ok(())
}

/// Remove liveness data from storage for all validators that are not in the
/// current consensus validator set.
pub fn prune_liveness_data<S>(
    storage: &mut S,
    current_epoch: Epoch,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    let consensus_validators =
        read_consensus_validator_set_addresses(storage, current_epoch)?;
    let liveness_missed_votes = liveness_missed_votes_handle();
    let liveness_sum_missed_votes = liveness_sum_missed_votes_handle();

    let validators_to_prune = liveness_sum_missed_votes
        .iter(storage)?
        .filter_map(|entry| {
            let (address, _) = entry.ok()?;

            if consensus_validators.contains(&address) {
                None
            } else {
                Some(address)
            }
        })
        .collect::<Vec<Address>>();

    for validator in &validators_to_prune {
        liveness_missed_votes.remove_all(storage, validator)?;
        liveness_sum_missed_votes.remove(storage, validator)?;
    }

    Ok(())
}

/// Record the liveness data of the consensus validators
pub fn record_liveness_data<S>(
    storage: &mut S,
    votes: &[VoteInfo],
    votes_epoch: Epoch,
    votes_height: BlockHeight,
    pos_params: &PosParams,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    let consensus_validators =
        read_consensus_validator_set_addresses(storage, votes_epoch)?;
    let liveness_missed_votes = liveness_missed_votes_handle();
    let liveness_sum_missed_votes = liveness_sum_missed_votes_handle();

    // Get the addresses of the validators who voted
    let vote_addresses = votes
        .iter()
        .map(|vote| (&vote.validator_address))
        .collect::<HashSet<&Address>>();

    let height_to_prune =
        votes_height.0.checked_sub(pos_params.liveness_window_check);

    for cons_validator in consensus_validators.into_iter() {
        // Prune old vote (only need to look for the block height that was just
        // pushed out of the sliding window)
        if let Some(prune_height) = height_to_prune {
            let pruned_missing_vote = liveness_missed_votes
                .at(&cons_validator)
                .remove(storage, &prune_height)?;

            if pruned_missing_vote {
                // Update liveness data
                liveness_sum_missed_votes.update(
                    storage,
                    cons_validator.clone(),
                    |missed_votes| missed_votes.unwrap() - 1,
                )?;
            }
        }

        // Evaluate new vote
        if !vote_addresses.contains(&cons_validator) {
            // Insert the height of the missing vote in storage
            liveness_missed_votes
                .at(&cons_validator)
                .insert(storage, votes_height.0)?;

            // Update liveness data
            liveness_sum_missed_votes.update(
                storage,
                cons_validator,
                |missed_votes| {
                    match missed_votes {
                        Some(missed_votes) => missed_votes + 1,
                        None => {
                            // Missing liveness data for the validator (newly
                            // added to the consensus
                            // set), initialize it
                            1
                        }
                    }
                },
            )?;
        } else {
            // Initialize any new consensus validator who has signed the first
            // block
            if !liveness_sum_missed_votes.contains(storage, &cons_validator)? {
                liveness_sum_missed_votes.insert(storage, cons_validator, 0)?;
            }
        }
    }

    Ok(())
}

/// Jail validators who failed to match the liveness threshold
pub fn jail_for_liveness<S>(
    storage: &mut S,
    params: &PosParams,
    current_epoch: Epoch,
    jail_epoch: Epoch,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    // Derive the actual missing votes limit from the percentage
    let missing_votes_threshold = ((Dec::one() - params.liveness_threshold)
        * params.liveness_window_check)
        .to_uint()
        .ok_or_else(|| {
            storage_api::Error::SimpleMessage(
                "Found negative liveness threshold",
            )
        })?
        .as_u64();

    // Jail inactive validators
    let validators_to_jail = liveness_sum_missed_votes_handle()
        .iter(storage)?
        .filter_map(|entry| {
            let (address, missed_votes) = entry.ok()?;

            // Check if validator failed to match the threshold and jail
            // them
            if missed_votes >= missing_votes_threshold {
                Some(address)
            } else {
                None
            }
        })
        .collect::<HashSet<_>>();

    for validator in &validators_to_jail {
        let state_jail_epoch = validator_state_handle(validator)
            .get(storage, jail_epoch, params)?
            .expect("Validator should have a state for the jail epoch");
        if state_jail_epoch == ValidatorState::Jailed {
            continue;
        }
        tracing::info!(
            "Jailing validator {} starting in epoch {} for missing too many \
             votes to ensure liveness",
            validator,
            jail_epoch,
        );
        jail_validator(storage, params, validator, current_epoch, jail_epoch)?;
    }

    Ok(())
}

#[cfg(any(test, feature = "testing"))]
/// PoS related utility functions to help set up tests.
pub mod test_utils {
    use namada_core::ledger::storage_api;
    use namada_core::ledger::storage_api::token::credit_tokens;
    use namada_core::ledger::storage_api::{StorageRead, StorageWrite};

    use super::*;
    use crate::parameters::PosParams;
    use crate::storage::read_non_pos_owned_params;
    use crate::types::GenesisValidator;

    /// Helper function to initialize storage with PoS data
    /// about validators for tests.
    pub fn init_genesis_helper<S>(
        storage: &mut S,
        params: &PosParams,
        validators: impl Iterator<Item = GenesisValidator>,
        current_epoch: namada_core::types::storage::Epoch,
    ) -> storage_api::Result<()>
    where
        S: StorageRead + StorageWrite,
    {
        init_genesis(storage, params, current_epoch)?;
        for GenesisValidator {
            address,
            consensus_key,
            protocol_key,
            eth_cold_key,
            eth_hot_key,
            commission_rate,
            max_commission_rate_change,
            tokens,
            metadata,
        } in validators
        {
            become_validator(
                storage,
                BecomeValidator {
                    params,
                    address: &address,
                    consensus_key: &consensus_key,
                    protocol_key: &protocol_key,
                    eth_cold_key: &eth_cold_key,
                    eth_hot_key: &eth_hot_key,
                    current_epoch,
                    commission_rate,
                    max_commission_rate_change,
                    metadata,
                    offset_opt: Some(0),
                },
            )?;
            // Credit token amount to be bonded to the validator address so it
            // can be bonded
            let staking_token = staking_token_address(storage);
            credit_tokens(storage, &staking_token, &address, tokens)?;

            bond_tokens(
                storage,
                None,
                &address,
                tokens,
                current_epoch,
                Some(0),
            )?;
        }
        // Store the total consensus validator stake to storage
        compute_and_store_total_consensus_stake(storage, current_epoch)?;

        // Copy validator sets and positions
        copy_genesis_validator_sets(storage, params, current_epoch)?;

        Ok(())
    }

    /// Init PoS genesis wrapper helper that also initializes gov params that
    /// are used in PoS with default values.
    pub fn test_init_genesis<S>(
        storage: &mut S,
        owned: OwnedPosParams,
        validators: impl Iterator<Item = GenesisValidator> + Clone,
        current_epoch: namada_core::types::storage::Epoch,
    ) -> storage_api::Result<PosParams>
    where
        S: StorageRead + StorageWrite,
    {
        let gov_params = namada_core::ledger::governance::parameters::GovernanceParameters::default();
        gov_params.init_storage(storage)?;
        let params = read_non_pos_owned_params(storage, owned)?;
        init_genesis_helper(storage, &params, validators, current_epoch)?;
        Ok(params)
    }
}

/// Change validator's metadata. In addition to changing any of the data from
/// [`ValidatorMetaData`], the validator's commission rate can be changed within
/// here as well.
#[allow(clippy::too_many_arguments)]
pub fn change_validator_metadata<S>(
    storage: &mut S,
    validator: &Address,
    email: Option<String>,
    description: Option<String>,
    website: Option<String>,
    discord_handle: Option<String>,
    avatar: Option<String>,
    commission_rate: Option<Dec>,
    current_epoch: Epoch,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    if let Some(email) = email {
        write_validator_email(storage, validator, &email)?;
    }
    if let Some(description) = description {
        write_validator_description(storage, validator, &description)?;
    }
    if let Some(website) = website {
        write_validator_website(storage, validator, &website)?;
    }
    if let Some(discord) = discord_handle {
        write_validator_discord_handle(storage, validator, &discord)?;
    }
    if let Some(avatar) = avatar {
        write_validator_avatar(storage, validator, &avatar)?;
    }
    if let Some(commission_rate) = commission_rate {
        change_validator_commission_rate(
            storage,
            validator,
            commission_rate,
            current_epoch,
        )?;
    }
    Ok(())
}

/// Claim available rewards, triggering an immediate transfer of tokens from the
/// PoS account to the source address.
pub fn claim_reward_tokens<S>(
    storage: &mut S,
    source: Option<&Address>,
    validator: &Address,
    current_epoch: Epoch,
) -> storage_api::Result<token::Amount>
where
    S: StorageRead + StorageWrite,
{
    tracing::debug!("Claiming rewards in epoch {current_epoch}");

    let source = source.cloned().unwrap_or_else(|| validator.clone());
    tracing::debug!("Source {} --> Validator {}", source, validator);

    let mut reward_tokens = compute_current_rewards_from_bonds(
        storage,
        &source,
        validator,
        current_epoch,
    )?;

    // Add reward tokens tallied during previous withdrawals
    reward_tokens += take_rewards_from_counter(storage, &source, validator)?;

    // Update the last claim epoch in storage
    write_last_reward_claim_epoch(storage, &source, validator, current_epoch)?;

    // Transfer the bonded tokens from PoS to the source
    let staking_token = staking_token_address(storage);
    token::transfer(storage, &staking_token, &ADDRESS, &source, reward_tokens)?;

    Ok(reward_tokens)
}

/// Query the amount of available reward tokens for a given bond.
pub fn query_reward_tokens<S>(
    storage: &S,
    source: Option<&Address>,
    validator: &Address,
    current_epoch: Epoch,
) -> storage_api::Result<token::Amount>
where
    S: StorageRead,
{
    let source = source.cloned().unwrap_or_else(|| validator.clone());
    let rewards_from_bonds = compute_current_rewards_from_bonds(
        storage,
        &source,
        validator,
        current_epoch,
    )?;

    let rewards_from_counter =
        read_rewards_counter(storage, &source, validator)?;

    Ok(rewards_from_bonds + rewards_from_counter)
}

/// Jail a validator by removing it from and updating the validator sets and
/// changing a its state to `Jailed`. Validators are jailed for liveness and for
/// misbehaving.
fn jail_validator<S>(
    storage: &mut S,
    params: &PosParams,
    validator: &Address,
    current_epoch: Epoch,
    validator_set_update_epoch: Epoch,
) -> storage_api::Result<()>
where
    S: StorageRead + StorageWrite,
{
    tracing::debug!(
        "Jailing validator {} beginning in epoch {}",
        validator,
        validator_set_update_epoch
    );

    // Remove the validator from the set starting at the update epoch and up
    // thru the pipeline epoch.
    let pipeline_epoch = current_epoch + params.pipeline_len;
    for epoch in
        Epoch::iter_bounds_inclusive(validator_set_update_epoch, pipeline_epoch)
    {
        let prev_state = validator_state_handle(validator)
            .get(storage, epoch, params)?
            .expect("Expected to find a valid validator.");
        match prev_state {
            ValidatorState::Consensus => {
                tracing::debug!(
                    "Removing validator from the consensus set in epoch {}",
                    epoch
                );
                remove_consensus_validator(storage, params, epoch, validator)?;

                // For the pipeline epoch only:
                // promote the next max inactive validator to the active
                // validator set at the pipeline offset
                if epoch == pipeline_epoch {
                    promote_next_below_capacity_validator_to_consensus(
                        storage, epoch,
                    )?;
                }
            }
            ValidatorState::BelowCapacity => {
                tracing::debug!(
                    "Removing validator from the below-capacity set in epoch \
                     {}",
                    epoch
                );
                remove_below_capacity_validator(
                    storage, params, epoch, validator,
                )?;
            }
            ValidatorState::BelowThreshold => {
                tracing::debug!(
                    "Setting below-threshold validator as jailed in epoch {}",
                    epoch
                );
            }
            ValidatorState::Inactive => {
                tracing::debug!(
                    "Setting inactive validator as jailed in epoch {}",
                    epoch
                );
            }
            ValidatorState::Jailed => {
                tracing::debug!(
                    "Found evidence for a validator who is already jailed"
                );
            }
        }
    }

    // Safe sub cause `validator_set_update_epoch > current_epoch`
    let start_offset = validator_set_update_epoch.0 - current_epoch.0;
    // Set the validator state as `Jailed` thru the pipeline epoch
    for offset in start_offset..=params.pipeline_len {
        validator_state_handle(validator).set(
            storage,
            ValidatorState::Jailed,
            current_epoch,
            offset,
        )?;
    }
    Ok(())
}

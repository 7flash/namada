//! MASP rewards conversions

use std::collections::BTreeMap;

use borsh::{BorshDeserialize, BorshSerialize};
use masp_primitives::asset_type::AssetType;
use masp_primitives::convert::AllowedConversion;
use masp_primitives::merkle_tree::FrozenCommitmentTree;
use masp_primitives::sapling::Node;

use crate::ledger::inflation::{mint_tokens, RewardsController, ValsToUpdate};
use crate::ledger::parameters;
use crate::ledger::storage_api::token::read_denom;
use crate::ledger::storage_api::{ResultExt, StorageRead, StorageWrite};
use crate::types::address::Address;
use crate::types::dec::Dec;
use crate::types::storage::{Epoch, Key};
use crate::types::token::MaspDenom;
use crate::types::uint::{Uint, I256};
use crate::types::{address, token};

/// A representation of the conversion state
#[derive(Debug, Default, BorshSerialize, BorshDeserialize)]
pub struct ConversionState {
    /// The last amount of the native token distributed
    pub normed_inflation: Option<I256>,
    /// The tree currently containing all the conversions
    pub tree: FrozenCommitmentTree<Node>,
    /// Map assets to their latest conversion and position in Merkle tree
    #[allow(clippy::type_complexity)]
    pub assets: BTreeMap<
        AssetType,
        (
            (Address, Option<Key>, MaspDenom),
            Epoch,
            AllowedConversion,
            usize,
        ),
    >,
}

#[cfg(feature = "wasm-runtime")]
fn calculate_masp_rewards<D, H>(
    wl_storage: &mut super::WlStorage<D, H>,
    addr: &Address,
    sub_prefix: Option<Key>,
) -> crate::ledger::storage_api::Result<(I256, I256)>
where
    D: 'static + super::DB + for<'iter> super::DBIter<'iter>,
    H: 'static + super::StorageHasher,
{
    let masp_addr = address::masp();
    // Query the storage for information

    //// information about the amount of tokens on the chain
    let total_tokens: token::Amount = wl_storage
        .read(&token::total_supply_key(addr))?
        .expect("the total supply key should be here");

    // total staked amount in the Shielded pool
    let total_token_in_masp: token::Amount = wl_storage
        .read(&token::balance_key(addr, &masp_addr))?
        .unwrap_or_default();

    let denomination = read_denom(wl_storage, addr, sub_prefix.as_ref())
        .unwrap()
        .unwrap();

    let denomination_base =
        read_denom(wl_storage, &wl_storage.get_native_token().unwrap(), None)
            .unwrap()
            .unwrap();

    let denomination_offset =
        10u64.pow((denomination.0 - denomination_base.0) as u32);
    let conversion = |amt| amt / denomination_offset;
    let total_tokens = conversion(total_tokens);
    let total_token_in_masp = conversion(total_token_in_masp);

    let epochs_per_year: u64 = wl_storage
        .read(&parameters::storage::get_epochs_per_year_key())?
        .expect("");

    //// Values from the last epoch
    let last_inflation: I256 = wl_storage
        .read(&token::last_inflation(addr))
        .expect("failure to read last inflation")
        .expect("");

    let last_locked_ratio: Dec = wl_storage
        .read(&token::last_locked_ratio(addr))
        .expect("failure to read last inflation")
        .expect("");

    //// Parameters for each token
    let max_reward_rate: Dec = wl_storage
        .read(&token::parameters::max_reward_rate(addr))
        .expect("max reward should properly decode")
        .expect("");

    let kp_gain_nom: Dec = wl_storage
        .read(&token::parameters::kp_sp_gain(addr))
        .expect("kp_gain_nom reward should properly decode")
        .expect("");

    let kd_gain_nom: Dec = wl_storage
        .read(&token::parameters::kd_sp_gain(addr))
        .expect("kd_gain_nom reward should properly decode")
        .expect("");

    let locked_target_ratio: Dec = wl_storage
        .read(&token::parameters::locked_token_ratio(addr))?
        .expect("");

    // Creating the PD controller for handing out tokens
    let controller = RewardsController::new(
        total_token_in_masp,
        total_tokens,
        locked_target_ratio,
        last_locked_ratio,
        max_reward_rate,
        token::Amount::from(last_inflation),
        kp_gain_nom,
        kd_gain_nom,
        epochs_per_year,
    );

    let ValsToUpdate {
        locked_ratio,
        inflation,
    } = RewardsController::run(controller);

    // inflation-per-token = inflation / locked tokens = n/100
    // ∴ n = (inflation * 100) / locked tokens
    // Since we must put the notes in a compatible format with the
    // note format, we must make the inflation amount discrete.
    let total_in = total_token_in_masp.change();
    let noterized_inflation = if total_in.is_zero() {
        I256::zero()
    } else {
        I256::from(100 * inflation) / (total_token_in_masp.change())
    };
    let clamped_inflation =
        I256::max(noterized_inflation, I256::from(i64::MAX));

    tracing::debug!(
        "Controller, call: total_in_masp {:?}, total_tokens {:?}, \
         locked_target_ratio {:?}, last_locked_ratio {:?}, max_reward_rate \
         {:?}, last_inflation {:?}, kp_gain_nom {:?}, kd_gain_nom {:?}, \
         epochs_per_year {:?}",
        total_token_in_masp,
        total_tokens,
        locked_target_ratio,
        last_locked_ratio,
        max_reward_rate,
        token::Amount::from(last_inflation),
        kp_gain_nom,
        kd_gain_nom,
        epochs_per_year,
    );

    // Is it fine to write the inflation rate, this is accurate,
    // but we should make sure the return value's ratio matches
    // this new inflation rate in 'update_allowed_conversions',
    // otherwise we will have an inaccurate view of inflation
    wl_storage
        .write(
            &token::last_inflation(addr),
            (clamped_inflation / I256::from(100))
                * total_token_in_masp.change() as I256,
        )
        .expect("unable to encode new inflation rate (Decimal)");

    wl_storage
        .write(&token::last_locked_ratio(addr), locked_ratio)
        .expect("unable to encode new locked ratio (Decimal)");

    // to make it conform with the expected output, we need to
    // move it to a ratio of x/100 to match the masp_rewards
    // function This may be unneeded, as we could describe it as a
    // ratio of x/1

    Ok((clamped_inflation, I256::from(100 * denomination_offset)))
}

// This is only enabled when "wasm-runtime" is on, because we're using rayon
#[cfg(feature = "wasm-runtime")]
/// Update the MASP's allowed conversions
pub fn update_allowed_conversions<D, H>(
    wl_storage: &mut super::WlStorage<D, H>,
) -> crate::ledger::storage_api::Result<()>
where
    D: 'static + super::DB + for<'iter> super::DBIter<'iter>,
    H: 'static + super::StorageHasher,
{
    use std::cmp::Ordering;

    use masp_primitives::ff::PrimeField;
    use masp_primitives::transaction::components::Amount as MaspAmount;
    use rayon::iter::{
        IndexedParallelIterator, IntoParallelIterator, ParallelIterator,
    };
    use rayon::prelude::ParallelSlice;

    use crate::types::storage::{self, KeySeg};

    // The derived conversions will be placed in MASP address space
    let masp_addr = address::masp();
    let key_prefix: storage::Key = masp_addr.to_db_key().into();

    let native_token = wl_storage.get_native_token().unwrap();
    let masp_rewards = address::masp_rewards();
    let mut masp_reward_keys: Vec<_> = masp_rewards.keys().collect();
    // Put the native rewards first because other inflation computations depend
    // on it
    masp_reward_keys.sort_unstable_by(|(x, _key), (y, _)| {
        if (*x == native_token) == (*y == native_token) {
            Ordering::Equal
        } else if *x == native_token {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    });
    // The total transparent value of the rewards being distributed
    let mut total_reward = token::Amount::native_whole(0);

    // Construct MASP asset type for rewards. Always timestamp reward tokens
    // with the zeroth epoch to minimize the number of convert notes clients
    // have to use. This trick works under the assumption that reward tokens
    // from different epochs are exactly equivalent.
    let reward_asset =
        encode_asset_type(native_token, &None, MaspDenom::Zero, Epoch(0));
    // Conversions from the previous to current asset for each address
    let mut current_convs =
        BTreeMap::<(Address, Option<Key>, MaspDenom), AllowedConversion>::new();
    // Reward all tokens according to above reward rates
    for (addr, sub_prefix) in masp_rewards.keys() {
        // TODO please intergate this into the logic
        let reward =
            calculate_masp_rewards(wl_storage, addr, sub_prefix.clone())?;

        // TODO Fix for multiple inflation
        // Native token inflation values are always with respect to this
        let ref_inflation = I256::from(1);
        // Get the last rewarded amount of the native token
        let normed_inflation = *wl_storage
            .storage
            .conversion_state
            .normed_inflation
            .get_or_insert(ref_inflation);

        // Dispense a transparent reward in parallel to the shielded rewards
        let addr_bal: token::Amount = match sub_prefix {
            None => wl_storage
                .read(&token::balance_key(addr, &masp_addr))?
                .unwrap_or_default(),
            Some(sub) => wl_storage
                .read(&token::multitoken_balance_key(
                    &token::multitoken_balance_prefix(addr, sub),
                    &masp_addr,
                ))?
                .unwrap_or_default(),
        };

        let mut new_normed_inflation = I256::zero();
        let mut real_reward = I256::zero();

        // TODO properly fix
        if *addr == address::nam() {
            // The amount that will be given of the new native token for
            // every amount of the native token given in the
            // previous epoch
            new_normed_inflation =
                normed_inflation + (normed_inflation * reward.0) / reward.1;

            println!("==============================================");
            println!(
                "reward before nam total_reward: {}",
                total_reward.to_string_native()
            );
            println!("==============================================");
            // The reward for each reward.1 units of the current asset is
            // reward.0 units of the reward token
            total_reward +=
                (addr_bal * (new_normed_inflation, normed_inflation)).0
                    - addr_bal;
            // Save the new normed inflation
            _ = wl_storage
                .storage
                .conversion_state
                .normed_inflation
                .insert(new_normed_inflation);
        } else {
            // Express the inflation reward in real terms, that is, with
            // respect to the native asset in the zeroth
            // epoch
            real_reward = (reward.0 * ref_inflation) / normed_inflation;

            println!("==============================================");
            println!(
                "reward before non nam total_reward: {}",
                total_reward.to_string_native()
            );
            println!("==============================================");
            // The reward for each reward.1 units of the current asset is
            // reward.0 units of the reward token
            total_reward += ((addr_bal * (real_reward, reward.1)).0
                * (normed_inflation, ref_inflation))
                .0;
        }

        for denom in token::MaspDenom::iter() {
            let total_reward_multiplier =
                Uint::pow(2.into(), (denom as u64 * 64).into());
            let total_reward = total_reward * total_reward_multiplier;
            // Provide an allowed conversion from previous timestamp. The
            // negative sign allows each instance of the old asset to be
            // cancelled out/replaced with the new asset
            let old_asset = encode_asset_type(
                addr.clone(),
                sub_prefix,
                denom,
                wl_storage.storage.last_epoch,
            );
            let new_asset = encode_asset_type(
                addr.clone(),
                sub_prefix,
                denom,
                wl_storage.storage.block.epoch,
            );

            println!("==============================================");
            println!(
                "final total_reward for denom {:?}: {:?}",
                denom, total_reward
            );
            println!("==============================================");

            if *addr == address::nam() {
                let new_normed_inflation =
                    new_normed_inflation % I256::from(u64::MAX);
                // The conversion is computed such that if consecutive
                // conversions are added together, the
                // intermediate native tokens cancel/
                // telescope out
                current_convs.insert(
                    (addr.clone(), sub_prefix.clone(), denom),
                    (MaspAmount::from_pair(old_asset, -(normed_inflation))
                        .unwrap()
                        + MaspAmount::from_pair(
                            new_asset,
                            new_normed_inflation,
                        )
                        .unwrap())
                    .into(),
                );
            } else {
                let real_reward = real_reward % I256::from(u64::MAX);
                // The conversion is computed such that if consecutive
                // conversions are added together, the
                // intermediate tokens cancel/ telescope out
                current_convs.insert(
                    (addr.clone(), sub_prefix.clone(), denom),
                    (MaspAmount::from_pair(old_asset, -(reward.1)).unwrap()
                        + MaspAmount::from_pair(new_asset, reward.1).unwrap()
                        + MaspAmount::from_pair(reward_asset, real_reward)
                            .unwrap())
                    .into(),
                );
            }

            // Add a conversion from the previous asset type
            println!("==============================================");
            println!("inserting conversions now");
            println!("old_asset: {}", old_asset);
            println!("denom: {:?}", denom);
            println!("addr, sub_prefix: {:?}", (addr, sub_prefix));
            println!("==============================================");
            wl_storage.storage.conversion_state.assets.insert(
                old_asset,
                (
                    (addr.clone(), sub_prefix.clone(), denom),
                    wl_storage.storage.last_epoch,
                    MaspAmount::zero().into(),
                    0,
                ),
            );
        }
    }

    // Try to distribute Merkle leaf updating as evenly as possible across
    // multiple cores
    let num_threads = rayon::current_num_threads();
    // Put assets into vector to enable computation batching
    let assets: Vec<_> = wl_storage
        .storage
        .conversion_state
        .assets
        .values_mut()
        .enumerate()
        .collect();
    // ceil(assets.len() / num_threads)
    let notes_per_thread_max = (assets.len() - 1) / num_threads + 1;
    // floor(assets.len() / num_threads)
    let notes_per_thread_min = assets.len() / num_threads;
    // Now on each core, add the latest conversion to each conversion
    let conv_notes: Vec<Node> = assets
        .into_par_iter()
        .with_min_len(notes_per_thread_min)
        .with_max_len(notes_per_thread_max)
        .map(|(idx, (asset, _epoch, conv, pos))| {
            // Use transitivity to update conversion
            *conv += current_convs[asset].clone();
            // Update conversion position to leaf we are about to create
            *pos = idx;
            // The merkle tree need only provide the conversion commitment,
            // the remaining information is provided through the storage API
            Node::new(conv.cmu().to_repr())
        })
        .collect();

    // Update the MASP's transparent reward token balance to ensure that it
    // is sufficiently backed to redeem rewards
    println!("==============================================");
    println!("current total_reward: {}", total_reward.to_string_native());
    println!("==============================================");
    mint_tokens(wl_storage, &masp_addr, &address::nam(), total_reward)?;

    // Try to distribute Merkle tree construction as evenly as possible
    // across multiple cores
    // Merkle trees must have exactly 2^n leaves to be mergeable
    let mut notes_per_thread_rounded = 1;
    while notes_per_thread_max > notes_per_thread_rounded * 4 {
        notes_per_thread_rounded *= 2;
    }
    // Make the sub-Merkle trees in parallel
    let tree_parts: Vec<_> = conv_notes
        .par_chunks(notes_per_thread_rounded)
        .map(FrozenCommitmentTree::new)
        .collect();

    // Convert conversion vector into tree so that Merkle paths can be
    // obtained
    wl_storage.storage.conversion_state.tree =
        FrozenCommitmentTree::merge(&tree_parts);

    // Add purely decoding entries to the assets map. These will be
    // overwritten before the creation of the next commitment tree
    for (addr, sub_prefix) in masp_rewards.keys() {
        for denom in token::MaspDenom::iter() {
            // Add the decoding entry for the new asset type. An uncommited
            // node position is used since this is not a conversion.
            let new_asset = encode_asset_type(
                addr.clone(),
                sub_prefix,
                denom,
                wl_storage.storage.block.epoch,
            );
            wl_storage.storage.conversion_state.assets.insert(
                new_asset,
                (
                    (addr.clone(), sub_prefix.clone(), denom),
                    wl_storage.storage.block.epoch,
                    MaspAmount::zero().into(),
                    wl_storage.storage.conversion_state.tree.size(),
                ),
            );
        }
    }

    // Save the current conversion state in order to avoid computing
    // conversion commitments from scratch in the next epoch
    let state_key = key_prefix
        .push(&(token::CONVERSION_KEY_PREFIX.to_owned()))
        .into_storage_result()?;
    // We cannot borrow `conversion_state` at the same time as when we call
    // `wl_storage.write`, so we encode it manually first
    let conv_bytes = wl_storage
        .storage
        .conversion_state
        .try_to_vec()
        .into_storage_result()?;
    wl_storage.write_bytes(&state_key, conv_bytes)?;
    Ok(())
}

/// Construct MASP asset type with given epoch for given token
pub fn encode_asset_type(
    addr: Address,
    sub_prefix: &Option<Key>,
    denom: MaspDenom,
    epoch: Epoch,
) -> AssetType {
    let new_asset_bytes = (
        addr,
        sub_prefix
            .as_ref()
            .map(|k| k.to_string())
            .unwrap_or_default(),
        denom,
        epoch.0,
    )
        .try_to_vec()
        .expect("unable to serialize address and epoch");
    AssetType::new(new_asset_bytes.as_ref())
        .expect("unable to derive asset identifier")
}

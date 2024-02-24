//! IBC integration

use namada_core::types::token::Amount;
use namada_ibc::storage::{
    channel_counter_key, client_counter_key, connection_counter_key,
    deposit_prefix, withdraw_prefix,
};
pub use namada_ibc::{parameters, storage};
use namada_state::{
    Key, StorageError, StorageHasher, StorageRead, StorageWrite, WlStorage,
};

/// Initialize storage in the genesis block.
pub fn init_genesis_storage<DB, H>(storage: &mut WlStorage<DB, H>)
where
    DB: namada_state::DB + for<'iter> namada_state::DBIter<'iter>,
    H: StorageHasher,
{
    // In ibc-go, u64 like a counter is encoded with big-endian:
    // https://github.com/cosmos/ibc-go/blob/89ffaafb5956a5ea606e1f1bf249c880bea802ed/modules/core/04-channel/keeper/keeper.go#L115

    let init_value = 0_u64;

    // the client counter
    let key = client_counter_key();
    storage
        .write(&key, init_value)
        .expect("Unable to write the initial client counter");

    // the connection counter
    let key = connection_counter_key();
    storage
        .write(&key, init_value)
        .expect("Unable to write the initial connection counter");

    // the channel counter
    let key = channel_counter_key();
    storage
        .write(&key, init_value)
        .expect("Unable to write the initial channel counter");
}

/// Clear the per-epoch throughputs (deposit and withdraw)
pub fn clear_throughputs<DB, H>(
    storage: &mut WlStorage<DB, H>,
) -> Result<(), StorageError>
where
    DB: namada_state::DB + for<'iter> namada_state::DBIter<'iter> + 'static,
    H: StorageHasher + 'static,
{
    for prefix in [deposit_prefix(), withdraw_prefix()] {
        let keys: Vec<Key> = storage
            .iter_prefix(&prefix)?
            .map(|(key, _, _)| {
                Key::parse(key).expect("The key should be parsable")
            })
            .collect();
        for key in keys {
            storage.write(&key, Amount::from(0))?;
        }
    }

    Ok(())
}

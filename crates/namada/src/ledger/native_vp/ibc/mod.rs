//! IBC integration as a native validity predicate

pub mod context;

use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use std::rc::Rc;
use std::time::Duration;

use context::{PseudoExecutionContext, VpValidationContext};
use namada_core::types::storage::Key;
use namada_gas::{IBC_ACTION_EXECUTE_GAS, IBC_ACTION_VALIDATE_GAS};
use namada_ibc::{
    Error as ActionError, IbcActions, NftTransferModule, TransferModule,
    ValidationParams,
};
use namada_proof_of_stake::storage::read_pos_params;
use namada_state::write_log::StorageModification;
use namada_state::StorageHasher;
use namada_tx::Tx;
use namada_vp_env::VpEnv;
use thiserror::Error;

use crate::ibc::core::host::types::identifiers::ChainId as IbcChainId;
use crate::ledger::ibc::storage::{
    calc_hash, deposit_key, get_limits, is_ibc_key, is_ibc_trace_key,
    mint_amount_key, withdraw_key,
};
use crate::ledger::native_vp::{self, Ctx, NativeVp};
use crate::ledger::parameters::read_epoch_duration_parameter;
use crate::token::storage_key::is_any_token_balance_key;
use crate::types::address::Address;
use crate::types::token::Amount;
use crate::vm::WasmCacheAccess;

#[allow(missing_docs)]
#[derive(Error, Debug)]
pub enum Error {
    #[error("Native VP error: {0}")]
    NativeVpError(native_vp::Error),
    #[error("Decoding error: {0}")]
    Decoding(std::io::Error),
    #[error("IBC message is required as transaction data")]
    NoTxData,
    #[error("IBC action error: {0}")]
    IbcAction(ActionError),
    #[error("State change error: {0}")]
    StateChange(String),
    #[error("IBC event error: {0}")]
    IbcEvent(String),
    #[error("IBC rate limit: {0}")]
    RateLimit(String),
}

/// IBC functions result
pub type VpResult<T> = std::result::Result<T, Error>;

/// IBC VP
pub struct Ibc<'a, DB, H, CA>
where
    DB: namada_state::DB + for<'iter> namada_state::DBIter<'iter>,
    H: StorageHasher,
    CA: 'static + WasmCacheAccess,
{
    /// Context to interact with the host structures.
    pub ctx: Ctx<'a, DB, H, CA>,
}

impl<'a, DB, H, CA> NativeVp for Ibc<'a, DB, H, CA>
where
    DB: 'static + namada_state::DB + for<'iter> namada_state::DBIter<'iter>,
    H: 'static + StorageHasher,
    CA: 'static + WasmCacheAccess,
{
    type Error = Error;

    fn validate_tx(
        &self,
        tx_data: &Tx,
        keys_changed: &BTreeSet<Key>,
        _verifiers: &BTreeSet<Address>,
    ) -> VpResult<bool> {
        let signed = tx_data;
        let tx_data = signed.data().ok_or(Error::NoTxData)?;

        // Pseudo execution and compare them
        self.validate_state(&tx_data, keys_changed)?;

        // Validate the state according to the given IBC message
        self.validate_with_msg(&tx_data)?;

        // Validate the denom store if a denom key has been changed
        self.validate_trace(keys_changed)?;

        // Check the limits
        self.check_limits(keys_changed)?;

        Ok(true)
    }
}

impl<'a, DB, H, CA> Ibc<'a, DB, H, CA>
where
    DB: 'static + namada_state::DB + for<'iter> namada_state::DBIter<'iter>,
    H: 'static + StorageHasher,
    CA: 'static + WasmCacheAccess,
{
    fn validate_state(
        &self,
        tx_data: &[u8],
        keys_changed: &BTreeSet<Key>,
    ) -> VpResult<()> {
        let exec_ctx = PseudoExecutionContext::new(self.ctx.pre());
        let ctx = Rc::new(RefCell::new(exec_ctx));

        let mut actions = IbcActions::new(ctx.clone());
        let module = TransferModule::new(ctx.clone());
        actions.add_transfer_module(module);
        let module = NftTransferModule::new(ctx.clone());
        actions.add_transfer_module(module);
        // Charge gas for the expensive execution
        self.ctx
            .charge_gas(IBC_ACTION_EXECUTE_GAS)
            .map_err(Error::NativeVpError)?;
        actions.execute(tx_data)?;

        let changed_ibc_keys: HashSet<&Key> =
            keys_changed.iter().filter(|k| is_ibc_key(k)).collect();
        if changed_ibc_keys.len() != ctx.borrow().get_changed_keys().len() {
            return Err(Error::StateChange(format!(
                "The changed keys mismatched: Actual {:?}, Expected {:?}",
                changed_ibc_keys,
                ctx.borrow().get_changed_keys(),
            )));
        }

        for key in changed_ibc_keys {
            let actual = self
                .ctx
                .read_bytes_post(key)
                .map_err(Error::NativeVpError)?;
            match_value(key, actual, ctx.borrow().get_changed_value(key))?;
        }

        // check the event
        let actual = self.ctx.write_log.get_ibc_events();
        if *actual != ctx.borrow().event {
            return Err(Error::IbcEvent(format!(
                "The IBC event is invalid: Actual {:?}, Expected {:?}",
                actual,
                ctx.borrow().event
            )));
        }

        Ok(())
    }

    fn validate_with_msg(&self, tx_data: &[u8]) -> VpResult<()> {
        let validation_ctx = VpValidationContext::new(self.ctx.pre());
        let ctx = Rc::new(RefCell::new(validation_ctx));

        let mut actions = IbcActions::new(ctx.clone());
        actions.set_validation_params(self.validation_params()?);

        let module = TransferModule::new(ctx.clone());
        actions.add_transfer_module(module);
        let module = NftTransferModule::new(ctx);
        actions.add_transfer_module(module);
        // Charge gas for the expensive validation
        self.ctx
            .charge_gas(IBC_ACTION_VALIDATE_GAS)
            .map_err(Error::NativeVpError)?;
        actions.validate(tx_data).map_err(Error::IbcAction)
    }

    /// Retrieve the validation params
    pub fn validation_params(&self) -> VpResult<ValidationParams> {
        use std::str::FromStr;
        let chain_id = self.ctx.get_chain_id().map_err(Error::NativeVpError)?;
        let proof_specs = namada_state::ics23_specs::ibc_proof_specs::<H>();
        let pos_params =
            read_pos_params(&self.ctx.post()).map_err(Error::NativeVpError)?;
        let pipeline_len = pos_params.pipeline_len;
        let epoch_duration = read_epoch_duration_parameter(&self.ctx.post())
            .map_err(Error::NativeVpError)?;
        let unbonding_period_secs =
            pipeline_len * epoch_duration.min_duration.0;
        Ok(ValidationParams {
            chain_id: IbcChainId::from_str(&chain_id)
                .map_err(ActionError::ChainId)?,
            proof_specs: proof_specs.into(),
            unbonding_period: Duration::from_secs(unbonding_period_secs),
            upgrade_path: Vec::new(),
        })
    }

    fn validate_trace(&self, keys_changed: &BTreeSet<Key>) -> VpResult<()> {
        for key in keys_changed {
            if let Some((_, hash)) = is_ibc_trace_key(key) {
                match self.ctx.read_post::<String>(key).map_err(|e| {
                    ActionError::Trace(format!(
                        "Getting the trace failed: Key {}, Error {}",
                        key, e
                    ))
                })? {
                    Some(trace) => {
                        if calc_hash(&trace) != hash {
                            return Err(ActionError::Trace(format!(
                                "The trace is invalid: Key {}, Trace {}",
                                key, trace
                            ))
                            .into());
                        }
                    }
                    None => {
                        return Err(ActionError::Trace(format!(
                            "The corresponding trace wasn't stored: Key {}",
                            key
                        ))
                        .into());
                    }
                }
            }
        }
        Ok(())
    }

    fn check_limits(&self, keys_changed: &BTreeSet<Key>) -> VpResult<bool> {
        let tokens: BTreeSet<&Address> = keys_changed
            .iter()
            .filter_map(|k| is_any_token_balance_key(k).map(|[key, _]| key))
            .collect();
        for token in tokens {
            let (mint_limit, throughput_limit) =
                get_limits(&self.ctx.pre(), token)
                    .map_err(Error::NativeVpError)?;

            // Check the supply
            let mint_amount_key = mint_amount_key(token);
            let minted: Amount = self
                .ctx
                .read_post(&mint_amount_key)
                .map_err(Error::NativeVpError)?
                .unwrap_or_default();
            if mint_limit < minted {
                return Err(Error::RateLimit(format!(
                    "Transfer exceeding the mint limit is not allowed: Mint \
                     limit {mint_limit}, minted amount {minted}"
                )));
            }

            // Check the rate limit
            let deposit_key = deposit_key(token);
            let deposit: Amount = self
                .ctx
                .read_post(&deposit_key)
                .map_err(Error::NativeVpError)?
                .unwrap_or_default();
            let withdraw_key = withdraw_key(token);
            let withdraw: Amount = self
                .ctx
                .read_post(&withdraw_key)
                .map_err(Error::NativeVpError)?
                .unwrap_or_default();
            let diff = if deposit < withdraw {
                withdraw
                    .checked_sub(deposit)
                    .expect("withdraw should be bigger than deposit")
            } else {
                deposit
                    .checked_sub(withdraw)
                    .expect("deposit should be bigger than withdraw")
            };
            if throughput_limit < diff {
                return Err(Error::RateLimit(format!(
                    "Transfer exceeding the per-epoch throughput limit is not \
                     allowed: Per-epoch throughput limit {throughput_limit}, \
                     actual throughput {diff}"
                )));
            }
        }
        Ok(true)
    }
}

fn match_value(
    key: &Key,
    actual: Option<Vec<u8>>,
    expected: Option<&StorageModification>,
) -> VpResult<()> {
    match (actual, expected) {
        (Some(v), Some(StorageModification::Write { value })) => {
            if v == *value {
                Ok(())
            } else {
                Err(Error::StateChange(format!(
                    "The value mismatched: Key {} actual {:?}, expected {:?}",
                    key, v, value
                )))
            }
        }
        (Some(_), _) => Err(Error::StateChange(format!(
            "The value was invalid: Key {}",
            key
        ))),
        (None, Some(StorageModification::Delete)) => Ok(()),
        (None, _) => Err(Error::StateChange(format!(
            "The key was deleted unexpectedly: Key {}",
            key
        ))),
    }
}

impl From<ActionError> for Error {
    fn from(err: ActionError) -> Self {
        Self::IbcAction(err)
    }
}

/// A dummy header used for testing
#[cfg(any(test, feature = "testing"))]
pub fn get_dummy_header() -> crate::types::storage::Header {
    use crate::tendermint::time::Time as TmTime;
    crate::types::storage::Header {
        hash: crate::types::hash::Hash([0; 32]),
        time: TmTime::now().try_into().unwrap(),
        next_validators_hash: crate::types::hash::Hash([0; 32]),
    }
}

/// A dummy validator used for testing
#[cfg(any(test, feature = "testing"))]
pub fn get_dummy_genesis_validator()
-> namada_proof_of_stake::types::GenesisValidator {
    use crate::core::types::address::testing::established_address_1;
    use crate::core::types::dec::Dec;
    use crate::core::types::key::testing::common_sk_from_simple_seed;
    use crate::types::key;

    let address = established_address_1();
    let tokens = Amount::native_whole(1);
    let consensus_sk = common_sk_from_simple_seed(0);
    let consensus_key = consensus_sk.to_public();

    let protocol_sk = common_sk_from_simple_seed(1);
    let protocol_key = protocol_sk.to_public();

    let commission_rate =
        Dec::new(1, 1).expect("expected 0.1 to be a valid decimal");
    let max_commission_rate_change =
        Dec::new(1, 1).expect("expected 0.1 to be a valid decimal");

    let eth_hot_sk =
        key::common::SecretKey::Secp256k1(key::testing::gen_keypair::<
            key::secp256k1::SigScheme,
        >());
    let eth_hot_key = eth_hot_sk.to_public();

    let eth_cold_sk =
        key::common::SecretKey::Secp256k1(key::testing::gen_keypair::<
            key::secp256k1::SigScheme,
        >());
    let eth_cold_key = eth_cold_sk.to_public();

    namada_proof_of_stake::types::GenesisValidator {
        address,
        tokens,
        consensus_key,
        protocol_key,
        eth_cold_key,
        eth_hot_key,
        commission_rate,
        max_commission_rate_change,
        metadata: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::convert::TryFrom;
    use std::str::FromStr;

    use borsh::BorshDeserialize;
    use borsh_ext::BorshSerializeExt;
    use ibc_testkit::testapp::ibc::clients::mock::client_state::{
        client_type, MockClientState, MOCK_CLIENT_TYPE,
    };
    use ibc_testkit::testapp::ibc::clients::mock::consensus_state::MockConsensusState;
    use ibc_testkit::testapp::ibc::clients::mock::header::MockHeader;
    use namada_gas::TxGasMeter;
    use namada_governance::parameters::GovernanceParameters;
    use namada_state::testing::TestWlStorage;
    use namada_state::StorageRead;
    use namada_token::NATIVE_MAX_DECIMAL_PLACES;
    use namada_tx::data::TxType;
    use namada_tx::{Code, Data, Section, Signature, Tx};
    use prost::Message;
    use sha2::Digest;

    use super::*;
    use crate::core::types::address::testing::{
        established_address_1, established_address_2,
    };
    use crate::core::types::address::{nam, InternalAddress};
    use crate::core::types::ibc::{MsgNftTransfer, MsgTransfer};
    use crate::core::types::storage::Epoch;
    use crate::ibc::apps::nft_transfer::types::events::{
        RecvEvent as NftRecvEvent, TokenTraceEvent,
        TransferEvent as NftTransferEvent,
    };
    use crate::ibc::apps::nft_transfer::types::msgs::transfer::MsgTransfer as IbcMsgNftTransfer;
    use crate::ibc::apps::nft_transfer::types::packet::PacketData as NftPacketData;
    use crate::ibc::apps::nft_transfer::types::{
        self as nft_types, PrefixedClassId, TokenId, TokenIds,
        VERSION as NFT_VERSION,
    };
    use crate::ibc::apps::transfer::types::events::{
        AckEvent, DenomTraceEvent, RecvEvent, TimeoutEvent, TransferEvent,
    };
    use crate::ibc::apps::transfer::types::msgs::transfer::MsgTransfer as IbcMsgTransfer;
    use crate::ibc::apps::transfer::types::packet::PacketData;
    use crate::ibc::apps::transfer::types::{
        ack_success_b64, PrefixedCoin, TracePrefix, VERSION,
    };
    use crate::ibc::core::channel::types::acknowledgement::{
        Acknowledgement, AcknowledgementStatus,
    };
    use crate::ibc::core::channel::types::channel::{
        ChannelEnd, Counterparty as ChanCounterparty, Order, State as ChanState,
    };
    use crate::ibc::core::channel::types::commitment::PacketCommitment;
    use crate::ibc::core::channel::types::events::{
        AcknowledgePacket, OpenAck as ChanOpenAck,
        OpenConfirm as ChanOpenConfirm, OpenInit as ChanOpenInit,
        OpenTry as ChanOpenTry, ReceivePacket, SendPacket, TimeoutPacket,
        WriteAcknowledgement,
    };
    use crate::ibc::core::channel::types::msgs::{
        MsgAcknowledgement, MsgChannelOpenAck, MsgChannelOpenConfirm,
        MsgChannelOpenInit, MsgChannelOpenTry, MsgRecvPacket, MsgTimeout,
        MsgTimeoutOnClose,
    };
    use crate::ibc::core::channel::types::packet::Packet;
    use crate::ibc::core::channel::types::timeout::TimeoutHeight;
    use crate::ibc::core::channel::types::Version as ChanVersion;
    use crate::ibc::core::client::types::events::{CreateClient, UpdateClient};
    use crate::ibc::core::client::types::msgs::{
        MsgCreateClient, MsgUpdateClient,
    };
    use crate::ibc::core::client::types::Height;
    use crate::ibc::core::commitment_types::commitment::{
        CommitmentPrefix, CommitmentProofBytes,
    };
    use crate::ibc::core::connection::types::events::{
        OpenAck as ConnOpenAck, OpenConfirm as ConnOpenConfirm,
        OpenInit as ConnOpenInit, OpenTry as ConnOpenTry,
    };
    use crate::ibc::core::connection::types::msgs::{
        MsgConnectionOpenAck, MsgConnectionOpenConfirm, MsgConnectionOpenInit,
        MsgConnectionOpenTry,
    };
    use crate::ibc::core::connection::types::version::Version as ConnVersion;
    use crate::ibc::core::connection::types::{
        ConnectionEnd, Counterparty as ConnCounterparty, State as ConnState,
    };
    use crate::ibc::core::handler::types::events::{
        IbcEvent as RawIbcEvent, MessageEvent,
    };
    use crate::ibc::core::host::types::identifiers::{
        ChannelId, ClientId, ConnectionId, PortId, Sequence,
    };
    use crate::ibc::core::router::types::event::ModuleEvent;
    use crate::ibc::parameters::IbcParameters;
    use crate::ibc::primitives::proto::{Any, Protobuf};
    use crate::ibc::primitives::{Timestamp, ToProto};
    use crate::ibc::storage::{
        ack_key, calc_hash, channel_counter_key, channel_key,
        client_connections_key, client_counter_key, client_state_key,
        client_update_height_key, client_update_timestamp_key, commitment_key,
        connection_counter_key, connection_key, consensus_state_key, ibc_token,
        ibc_trace_key, mint_amount_key, next_sequence_ack_key,
        next_sequence_recv_key, next_sequence_send_key, nft_class_key,
        nft_metadata_key, receipt_key,
    };
    use crate::ledger::gas::VpGasMeter;
    use crate::ledger::parameters::storage::{
        get_epoch_duration_storage_key, get_max_expected_time_per_block_key,
    };
    use crate::ledger::parameters::EpochDuration;
    use crate::ledger::{ibc, pos};
    use crate::tendermint::time::Time as TmTime;
    use crate::token::storage_key::balance_key;
    use crate::token::Amount;
    use crate::types::ibc::{NftClass, NftMetadata};
    use crate::types::key::testing::keypair_1;
    use crate::types::storage::{BlockHash, BlockHeight, TxIndex};
    use crate::types::time::DurationSecs;
    use crate::vm::wasm;

    const ADDRESS: Address = Address::Internal(InternalAddress::Ibc);
    const COMMITMENT_PREFIX: &[u8] = b"ibc";
    const TX_GAS_LIMIT: u64 = 1_000_000;

    fn get_client_id() -> ClientId {
        let id = format!("{}-0", MOCK_CLIENT_TYPE);
        ClientId::from_str(&id).expect("Creating a client ID failed")
    }

    fn init_storage() -> TestWlStorage {
        let mut wl_storage = TestWlStorage::default();

        // initialize the storage
        ibc::init_genesis_storage(&mut wl_storage);
        let gov_params = GovernanceParameters::default();
        gov_params.init_storage(&mut wl_storage).unwrap();
        let ibc_params = IbcParameters {
            default_mint_limit: Amount::native_whole(100),
            default_per_epoch_throughput_limit: Amount::native_whole(100),
        };
        ibc_params.init_storage(&mut wl_storage).unwrap();
        pos::test_utils::test_init_genesis(
            &mut wl_storage,
            namada_proof_of_stake::OwnedPosParams::default(),
            vec![get_dummy_genesis_validator()].into_iter(),
            Epoch(1),
        )
        .unwrap();
        // epoch duration
        let epoch_duration_key = get_epoch_duration_storage_key();
        let epoch_duration = EpochDuration {
            min_num_of_blocks: 10,
            min_duration: DurationSecs(100),
        };
        wl_storage
            .write_log
            .write(&epoch_duration_key, epoch_duration.serialize_to_vec())
            .expect("write failed");
        // max_expected_time_per_block
        let time = DurationSecs::from(Duration::new(60, 0));
        let time_key = get_max_expected_time_per_block_key();
        wl_storage
            .write_log
            .write(&time_key, namada_core::types::encode(&time))
            .expect("write failed");
        // set a dummy header
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(1))
            .unwrap();

        wl_storage
    }

    fn insert_init_client(wl_storage: &mut TestWlStorage) {
        // insert a mock client type
        let client_id = get_client_id();
        // insert a mock client state
        let client_state_key = client_state_key(&get_client_id());
        let height = Height::new(0, 1).unwrap();
        let header = MockHeader {
            height,
            timestamp: Timestamp::now(),
        };
        let client_state = MockClientState::new(header);
        let bytes = Protobuf::<Any>::encode_vec(client_state);
        wl_storage
            .write_log
            .write(&client_state_key, bytes)
            .expect("write failed");
        // insert a mock consensus state
        let consensus_key = consensus_state_key(&client_id, height);
        let consensus_state = MockConsensusState::new(header);
        let bytes = Protobuf::<Any>::encode_vec(consensus_state);
        wl_storage
            .write_log
            .write(&consensus_key, bytes)
            .expect("write failed");
        // insert update time and height
        let client_update_time_key = client_update_timestamp_key(&client_id);
        let time = wl_storage
            .storage
            .get_block_header(None)
            .unwrap()
            .0
            .unwrap()
            .time;
        let bytes = TmTime::try_from(time).unwrap().encode_vec();
        wl_storage
            .write_log
            .write(&client_update_time_key, bytes)
            .expect("write failed");
        let client_update_height_key = client_update_height_key(&client_id);
        let host_height = wl_storage.storage.get_block_height().0;
        let host_height =
            Height::new(0, host_height.0).expect("invalid height");
        wl_storage
            .write_log
            .write(&client_update_height_key, host_height.encode_vec())
            .expect("write failed");
        wl_storage.write_log.commit_tx();
    }

    fn get_connection_id() -> ConnectionId {
        ConnectionId::new(0)
    }

    fn get_port_id() -> PortId {
        PortId::transfer()
    }

    fn get_nft_port_id() -> PortId {
        PortId::from_str(crate::ibc::apps::nft_transfer::types::PORT_ID_STR)
            .unwrap()
    }

    fn get_channel_id() -> ChannelId {
        ChannelId::new(0)
    }

    fn get_connection(conn_state: ConnState) -> ConnectionEnd {
        ConnectionEnd::new(
            conn_state,
            get_client_id(),
            get_conn_counterparty(),
            vec![ConnVersion::default()],
            Duration::new(0, 0),
        )
        .unwrap()
    }

    fn get_conn_counterparty() -> ConnCounterparty {
        let counterpart_client_id =
            ClientId::new(&client_type().to_string(), 22).unwrap();
        let counterpart_conn_id = ConnectionId::new(32);
        let commitment_prefix =
            CommitmentPrefix::try_from(COMMITMENT_PREFIX.to_vec())
                .expect("the prefix should be parsable");
        ConnCounterparty::new(
            counterpart_client_id,
            Some(counterpart_conn_id),
            commitment_prefix,
        )
    }

    fn get_channel(channel_state: ChanState, order: Order) -> ChannelEnd {
        ChannelEnd::new(
            channel_state,
            order,
            get_channel_counterparty(),
            vec![get_connection_id()],
            ChanVersion::new(VERSION.to_string()),
        )
        .unwrap()
    }

    fn get_channel_counterparty() -> ChanCounterparty {
        let counterpart_port_id = PortId::transfer();
        let counterpart_channel_id = ChannelId::new(0);
        ChanCounterparty::new(counterpart_port_id, Some(counterpart_channel_id))
    }

    fn get_channel_for_nft(
        channel_state: ChanState,
        order: Order,
    ) -> ChannelEnd {
        ChannelEnd::new(
            channel_state,
            order,
            get_channel_counterparty_for_nft(),
            vec![get_connection_id()],
            ChanVersion::new(NFT_VERSION.to_string()),
        )
        .unwrap()
    }

    fn get_channel_counterparty_for_nft() -> ChanCounterparty {
        let counterpart_port_id = get_nft_port_id();
        let counterpart_channel_id = ChannelId::new(0);
        ChanCounterparty::new(counterpart_port_id, Some(counterpart_channel_id))
    }

    fn get_next_seq(wl_storage: &TestWlStorage, key: &Key) -> Sequence {
        let (val, _) = wl_storage.storage.read(key).expect("read failed");
        match val {
            Some(v) => {
                // IBC related data is encoded without borsh
                let index: [u8; 8] = v.try_into().expect("decoding failed");
                let index = u64::from_be_bytes(index);
                Sequence::from(index)
            }
            // The sequence has not been used yet
            None => Sequence::from(1),
        }
    }

    fn increment_sequence(wl_storage: &mut TestWlStorage, key: &Key) {
        let count = match wl_storage.read_bytes(key).expect("read failed") {
            Some(value) => {
                let count: [u8; 8] =
                    value.try_into().expect("decoding a count failed");
                u64::from_be_bytes(count)
            }
            None => 0,
        };
        wl_storage
            .write_log
            .write(key, (count + 1).to_be_bytes().to_vec())
            .expect("write failed");
    }

    fn increment_counter(wl_storage: &mut TestWlStorage, key: &Key) {
        let count = match wl_storage.read_bytes(key).expect("read failed") {
            Some(value) => {
                u64::try_from_slice(&value).expect("invalid counter value")
            }
            None => unreachable!("The counter should be initialized"),
        };
        wl_storage
            .write_log
            .write(key, (count + 1).serialize_to_vec())
            .expect("write failed");
    }

    fn dummy_proof() -> CommitmentProofBytes {
        CommitmentProofBytes::try_from(vec![0]).unwrap()
    }

    fn packet_from_message(
        msg: &IbcMsgTransfer,
        sequence: Sequence,
        counterparty: &ChanCounterparty,
    ) -> Packet {
        let data = serde_json::to_vec(&msg.packet_data)
            .expect("Encoding PacketData failed");

        Packet {
            seq_on_a: sequence,
            port_id_on_a: msg.port_id_on_a.clone(),
            chan_id_on_a: msg.chan_id_on_a.clone(),
            port_id_on_b: counterparty.port_id.clone(),
            chan_id_on_b: counterparty
                .channel_id()
                .expect("the counterparty channel should exist")
                .clone(),
            data,
            timeout_height_on_b: msg.timeout_height_on_b,
            timeout_timestamp_on_b: msg.timeout_timestamp_on_b,
        }
    }

    fn nft_packet_from_message(
        msg: &IbcMsgNftTransfer,
        sequence: Sequence,
        counterparty: &ChanCounterparty,
    ) -> Packet {
        // the packet data should be updated
        let mut packet_data = msg.packet_data.clone();
        packet_data.class_uri = Some(DUMMY_URI.parse().unwrap());
        packet_data.class_data = Some(DUMMY_DATA.parse().unwrap());
        packet_data.token_uris = Some(vec![DUMMY_URI.parse().unwrap()]);
        packet_data.token_data = Some(vec![DUMMY_DATA.parse().unwrap()]);
        let data = serde_json::to_vec(&packet_data)
            .expect("Encoding NftPacketData failed");

        Packet {
            seq_on_a: sequence,
            port_id_on_a: msg.port_id_on_a.clone(),
            chan_id_on_a: msg.chan_id_on_a.clone(),
            port_id_on_b: counterparty.port_id.clone(),
            chan_id_on_b: counterparty
                .channel_id()
                .expect("the counterparty channel should exist")
                .clone(),
            data,
            timeout_height_on_b: msg.timeout_height_on_b,
            timeout_timestamp_on_b: msg.timeout_timestamp_on_b,
        }
    }

    fn commitment(packet: &Packet) -> PacketCommitment {
        let timeout = packet.timeout_timestamp_on_b.nanoseconds().to_be_bytes();
        let revision_number = packet
            .timeout_height_on_b
            .commitment_revision_number()
            .to_be_bytes();
        let revision_height = packet
            .timeout_height_on_b
            .commitment_revision_height()
            .to_be_bytes();
        let data = sha2::Sha256::digest(&packet.data);
        let input = [
            &timeout,
            &revision_number,
            &revision_height,
            data.as_slice(),
        ]
        .concat();
        sha2::Sha256::digest(&input).to_vec().into()
    }

    fn get_nft_class_id() -> PrefixedClassId {
        "nft-transfer/channel-14/myclass".parse().unwrap()
    }

    fn get_nft_id() -> TokenId {
        "mytoken".parse().unwrap()
    }

    const DUMMY_DATA: &str = r#"{"name":{"value":"Crypto Creatures"},"image":{"value":"binary","mime":"image/png"}}"#;
    const DUMMY_URI: &str = "http://example.com";
    fn dummy_nft_class() -> NftClass {
        NftClass {
            class_id: get_nft_class_id(),
            class_uri: Some(DUMMY_URI.parse().unwrap()),
            class_data: Some(DUMMY_DATA.parse().unwrap()),
        }
    }

    fn dummy_nft_metadata() -> NftMetadata {
        NftMetadata {
            class_id: get_nft_class_id(),
            token_id: get_nft_id(),
            token_uri: Some(DUMMY_URI.parse().unwrap()),
            token_data: Some(DUMMY_DATA.parse().unwrap()),
        }
    }

    #[test]
    fn test_create_client() {
        let mut wl_storage = init_storage();
        let mut keys_changed = BTreeSet::new();

        let height = Height::new(0, 1).unwrap();
        let header = MockHeader {
            height,
            timestamp: Timestamp::now(),
        };
        let client_id = get_client_id();
        // message
        let client_state = MockClientState::new(header);
        let consensus_state = MockConsensusState::new(header);
        let msg = MsgCreateClient {
            client_state: client_state.into(),
            consensus_state: consensus_state.clone().into(),
            signer: "account0".to_string().into(),
        };
        // client state
        let client_state_key = client_state_key(&get_client_id());
        let bytes = Protobuf::<Any>::encode_vec(client_state);
        wl_storage
            .write_log
            .write(&client_state_key, bytes)
            .expect("write failed");
        keys_changed.insert(client_state_key);
        // client consensus
        let consensus_key = consensus_state_key(&client_id, height);
        let bytes = Protobuf::<Any>::encode_vec(consensus_state);
        wl_storage
            .write_log
            .write(&consensus_key, bytes)
            .expect("write failed");
        keys_changed.insert(consensus_key);
        // client counter
        let client_counter_key = client_counter_key();
        increment_counter(&mut wl_storage, &client_counter_key);
        keys_changed.insert(client_counter_key);

        let event = RawIbcEvent::CreateClient(CreateClient::new(
            client_id,
            client_type(),
            client_state.latest_height(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Client);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");
        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let mut outer_tx = Tx::from_type(TxType::Raw);
        outer_tx.header.chain_id = wl_storage.storage.chain_id.clone();
        outer_tx.set_code(Code::new(tx_code, None));
        outer_tx.set_data(Data::new(tx_data));
        outer_tx.add_section(Section::Signature(Signature::new(
            vec![outer_tx.header_hash()],
            [(0, keypair_1())].into_iter().collect(),
            None,
        )));
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &outer_tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );

        let ibc = Ibc { ctx };
        // this should return true because state has been stored
        assert!(
            ibc.validate_tx(&outer_tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_create_client_fail() {
        let mut wl_storage = TestWlStorage::default();

        let mut keys_changed = BTreeSet::new();

        // initialize the storage
        ibc::init_genesis_storage(&mut wl_storage);
        // set a dummy header
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(1))
            .unwrap();

        let height = Height::new(0, 1).unwrap();
        let header = MockHeader {
            height,
            timestamp: Timestamp::now(),
        };
        // insert only client state
        let client_state = MockClientState::new(header);
        let client_state_key = client_state_key(&get_client_id());
        let bytes = Protobuf::<Any>::encode_vec(client_state);
        wl_storage
            .write_log
            .write(&client_state_key, bytes)
            .expect("write failed");
        keys_changed.insert(client_state_key);
        let client_state = MockClientState::new(header);
        let consensus_state = MockConsensusState::new(header);
        // make a correct message
        let msg = MsgCreateClient {
            client_state: client_state.into(),
            consensus_state: consensus_state.into(),
            signer: "account0".to_string().into(),
        };

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");

        let mut tx = Tx::new(wl_storage.storage.chain_id.clone(), None);
        tx.add_code(tx_code, None)
            .add_serialized_data(tx_data)
            .sign_wrapper(keypair_1());

        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );

        let ibc = Ibc { ctx };
        // this should fail because no state is stored
        let result =
            ibc.validate_tx(&tx, &keys_changed, &verifiers).unwrap_err();
        assert_matches!(result, Error::StateChange(_));
    }

    #[test]
    fn test_update_client() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");

        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // update the client
        let client_id = get_client_id();
        let client_state_key = client_state_key(&get_client_id());
        let height = Height::new(0, 11).unwrap();
        // the header should be created before
        let time = (TmTime::now() - std::time::Duration::new(100, 0)).unwrap();
        let header = MockHeader {
            height,
            timestamp: time.into(),
        };
        let msg = MsgUpdateClient {
            client_id: client_id.clone(),
            client_message: header.into(),
            signer: "account0".to_string().into(),
        };
        // client state
        let client_state = MockClientState::new(header);
        let bytes = Protobuf::<Any>::encode_vec(client_state);
        wl_storage
            .write_log
            .write(&client_state_key, bytes)
            .expect("write failed");
        keys_changed.insert(client_state_key);
        // consensus state
        let consensus_key = consensus_state_key(&client_id, height);
        let consensus_state = MockConsensusState::new(header);
        let bytes = Protobuf::<Any>::encode_vec(consensus_state);
        wl_storage
            .write_log
            .write(&consensus_key, bytes)
            .expect("write failed");
        keys_changed.insert(consensus_key);
        // client update time
        let client_update_time_key = client_update_timestamp_key(&client_id);
        let time = wl_storage
            .storage
            .get_block_header(None)
            .unwrap()
            .0
            .unwrap()
            .time;
        let bytes = TmTime::try_from(time).unwrap().encode_vec();
        wl_storage
            .write_log
            .write(&client_update_time_key, bytes)
            .expect("write failed");
        keys_changed.insert(client_update_time_key);
        // client update height
        let client_update_height_key = client_update_height_key(&client_id);
        let host_height = wl_storage.storage.get_block_height().0;
        let host_height =
            Height::new(0, host_height.0).expect("invalid height");
        wl_storage
            .write_log
            .write(&client_update_height_key, host_height.encode_vec())
            .expect("write failed");
        keys_changed.insert(client_update_height_key);
        // event
        let consensus_height = client_state.latest_height();
        let event = RawIbcEvent::UpdateClient(UpdateClient::new(
            client_id,
            client_type(),
            consensus_height,
            vec![consensus_height],
            Protobuf::<Any>::encode_vec(header),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Client);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");

        let mut tx = Tx::new(wl_storage.storage.chain_id.clone(), None);
        tx.add_code(tx_code, None)
            .add_serialized_data(tx_data)
            .sign_wrapper(keypair_1());

        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        // this should return true because state has been stored
        assert!(
            ibc.validate_tx(&tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_init_connection() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // prepare a message
        let mut counterparty = get_conn_counterparty();
        counterparty.connection_id = None;
        let msg = MsgConnectionOpenInit {
            client_id_on_a: get_client_id(),
            counterparty,
            version: Some(ConnVersion::default()),
            delay_period: Duration::new(100, 0),
            signer: "account0".to_string().into(),
        };

        // insert an INIT connection
        let conn_id = get_connection_id();
        let conn_key = connection_key(&conn_id);
        let conn = ConnectionEnd::new(
            ConnState::Init,
            msg.client_id_on_a.clone(),
            msg.counterparty.clone(),
            vec![msg.version.clone().unwrap()],
            msg.delay_period,
        )
        .expect("invalid connection");
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        keys_changed.insert(conn_key);
        // client connection list
        let client_conn_key = client_connections_key(&msg.client_id_on_a);
        let conn_list = conn_id.to_string();
        let bytes = conn_list.serialize_to_vec();
        wl_storage
            .write_log
            .write(&client_conn_key, bytes)
            .expect("write failed");
        keys_changed.insert(client_conn_key);
        // connection counter
        let conn_counter_key = connection_counter_key();
        increment_counter(&mut wl_storage, &conn_counter_key);
        keys_changed.insert(conn_counter_key);
        // event
        let event = RawIbcEvent::OpenInitConnection(ConnOpenInit::new(
            conn_id,
            msg.client_id_on_a.clone(),
            msg.counterparty.client_id().clone(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Connection);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");
        let mut outer_tx = Tx::from_type(TxType::Raw);
        outer_tx.header.chain_id = wl_storage.storage.chain_id.clone();
        outer_tx.set_code(Code::new(tx_code, None));
        outer_tx.set_data(Data::new(tx_data));
        outer_tx.add_section(Section::Signature(Signature::new(
            vec![outer_tx.header_hash()],
            [(0, keypair_1())].into_iter().collect(),
            None,
        )));
        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &outer_tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        // this should return true because state has been stored
        assert!(
            ibc.validate_tx(&outer_tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_init_connection_fail() {
        let mut wl_storage = TestWlStorage::default();
        let mut keys_changed = BTreeSet::new();

        // initialize the storage
        ibc::init_genesis_storage(&mut wl_storage);
        // set a dummy header
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(1))
            .unwrap();

        // prepare data
        let mut counterparty = get_conn_counterparty();
        counterparty.connection_id = None;
        let msg = MsgConnectionOpenInit {
            client_id_on_a: get_client_id(),
            counterparty,
            version: Some(ConnVersion::default()),
            delay_period: Duration::new(100, 0),
            signer: "account0".to_string().into(),
        };

        // insert an Init connection
        let conn_id = get_connection_id();
        let conn_key = connection_key(&conn_id);
        let conn = ConnectionEnd::new(
            ConnState::Init,
            msg.client_id_on_a.clone(),
            msg.counterparty.clone(),
            vec![msg.version.clone().unwrap()],
            msg.delay_period,
        )
        .expect("invalid connection");
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        keys_changed.insert(conn_key);
        // client connection list
        let client_conn_key = client_connections_key(&msg.client_id_on_a);
        let conn_list = conn_id.to_string();
        let bytes = conn_list.serialize_to_vec();
        wl_storage
            .write_log
            .write(&client_conn_key, bytes)
            .expect("write failed");
        keys_changed.insert(client_conn_key);
        // connection counter
        let conn_counter_key = connection_counter_key();
        increment_counter(&mut wl_storage, &conn_counter_key);
        keys_changed.insert(conn_counter_key);
        // No event

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");

        let mut tx = Tx::new(wl_storage.storage.chain_id.clone(), None);
        tx.add_code(tx_code, None)
            .add_serialized_data(tx_data)
            .sign_wrapper(keypair_1());

        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        // this should fail because no event
        let result =
            ibc.validate_tx(&tx, &keys_changed, &verifiers).unwrap_err();
        assert_matches!(result, Error::IbcEvent(_));
    }

    #[test]
    fn test_try_connection() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // prepare data
        let height = Height::new(0, 1).unwrap();
        let header = MockHeader {
            height,
            timestamp: Timestamp::now(),
        };
        let client_state = MockClientState::new(header);
        let proof_height = Height::new(0, 1).unwrap();
        #[allow(deprecated)]
        let msg = MsgConnectionOpenTry {
            client_id_on_b: get_client_id(),
            client_state_of_b_on_a: client_state.into(),
            counterparty: get_conn_counterparty(),
            versions_on_a: vec![ConnVersion::default()],
            proofs_height_on_a: proof_height,
            proof_conn_end_on_a: dummy_proof(),
            proof_client_state_of_b_on_a: dummy_proof(),
            proof_consensus_state_of_b_on_a: dummy_proof(),
            consensus_height_of_b_on_a: client_state.latest_height(),
            delay_period: Duration::from_secs(0),
            signer: "account0".to_string().into(),
            proof_consensus_state_of_b: Some(dummy_proof()),
            previous_connection_id: ConnectionId::default().to_string(),
        };

        // insert a TryOpen connection
        let conn_id = get_connection_id();
        let conn_key = connection_key(&conn_id);
        let conn = ConnectionEnd::new(
            ConnState::TryOpen,
            msg.client_id_on_b.clone(),
            msg.counterparty.clone(),
            msg.versions_on_a.clone(),
            msg.delay_period,
        )
        .expect("invalid connection");
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        keys_changed.insert(conn_key);
        // client connection list
        let client_conn_key = client_connections_key(&msg.client_id_on_b);
        let conn_list = conn_id.to_string();
        let bytes = conn_list.serialize_to_vec();
        wl_storage
            .write_log
            .write(&client_conn_key, bytes)
            .expect("write failed");
        keys_changed.insert(client_conn_key);
        // connection counter
        let conn_counter_key = connection_counter_key();
        increment_counter(&mut wl_storage, &conn_counter_key);
        keys_changed.insert(conn_counter_key);
        // event
        let event = RawIbcEvent::OpenTryConnection(ConnOpenTry::new(
            conn_id,
            msg.client_id_on_b.clone(),
            msg.counterparty.connection_id().cloned().unwrap(),
            msg.counterparty.client_id().clone(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Connection);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");
        let mut tx = Tx::new(wl_storage.storage.chain_id.clone(), None);
        tx.add_code(tx_code, None)
            .add_serialized_data(tx_data)
            .sign_wrapper(keypair_1());

        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        // this should return true because state has been stored
        assert!(
            ibc.validate_tx(&tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_ack_connection() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);

        // insert an Init connection
        let conn_key = connection_key(&get_connection_id());
        let conn = get_connection(ConnState::Init);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // update the connection to Open
        let conn = get_connection(ConnState::Open);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        keys_changed.insert(conn_key);

        // prepare data
        let height = Height::new(0, 1).unwrap();
        let header = MockHeader {
            height,
            timestamp: Timestamp::now(),
        };
        let client_state = MockClientState::new(header);
        let counterparty = get_conn_counterparty();
        let proof_height = Height::new(0, 1).unwrap();

        let msg = MsgConnectionOpenAck {
            conn_id_on_a: get_connection_id(),
            conn_id_on_b: counterparty.connection_id().cloned().unwrap(),
            client_state_of_a_on_b: client_state.into(),
            proof_conn_end_on_b: dummy_proof(),
            proof_client_state_of_a_on_b: dummy_proof(),
            proof_consensus_state_of_a_on_b: dummy_proof(),
            proofs_height_on_b: proof_height,
            consensus_height_of_a_on_b: client_state.latest_height(),
            version: ConnVersion::default(),
            signer: "account0".to_string().into(),
            proof_consensus_state_of_a: None,
        };
        // event
        let event = RawIbcEvent::OpenAckConnection(ConnOpenAck::new(
            msg.conn_id_on_a.clone(),
            get_client_id(),
            msg.conn_id_on_b.clone(),
            counterparty.client_id().clone(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Connection);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_code = vec![];
        let tx_index = TxIndex::default();
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");
        let mut outer_tx = Tx::from_type(TxType::Raw);
        outer_tx.header.chain_id = wl_storage.storage.chain_id.clone();
        outer_tx.set_code(Code::new(tx_code, None));
        outer_tx.set_data(Data::new(tx_data));
        outer_tx.add_section(Section::Signature(Signature::new(
            vec![outer_tx.header_hash()],
            [(0, keypair_1())].into_iter().collect(),
            None,
        )));
        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &outer_tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        assert!(
            ibc.validate_tx(&outer_tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_confirm_connection() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);

        // insert a TryOpen connection
        let conn_key = connection_key(&get_connection_id());
        let conn = get_connection(ConnState::TryOpen);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // update the connection to Open
        let conn = get_connection(ConnState::Open);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        keys_changed.insert(conn_key);

        // prepare data
        let proof_height = Height::new(0, 1).unwrap();
        let msg = MsgConnectionOpenConfirm {
            conn_id_on_b: get_connection_id(),
            proof_conn_end_on_a: dummy_proof(),
            proof_height_on_a: proof_height,
            signer: "account0".to_string().into(),
        };
        // event
        let counterparty = get_conn_counterparty();
        let event = RawIbcEvent::OpenConfirmConnection(ConnOpenConfirm::new(
            get_connection_id(),
            get_client_id(),
            counterparty.connection_id().cloned().unwrap(),
            counterparty.client_id().clone(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Connection);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_code = vec![];
        let tx_index = TxIndex::default();
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");
        let mut outer_tx = Tx::from_type(TxType::Raw);
        outer_tx.header.chain_id = wl_storage.storage.chain_id.clone();
        outer_tx.set_code(Code::new(tx_code, None));
        outer_tx.set_data(Data::new(tx_data));
        outer_tx.add_section(Section::Signature(Signature::new(
            vec![outer_tx.header_hash()],
            [(0, keypair_1())].into_iter().collect(),
            None,
        )));
        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &outer_tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        assert!(
            ibc.validate_tx(&outer_tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_init_channel() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);

        // insert an opened connection
        let conn_id = get_connection_id();
        let conn_key = connection_key(&conn_id);
        let conn = get_connection(ConnState::Open);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // prepare data
        let msg = MsgChannelOpenInit {
            port_id_on_a: get_port_id(),
            connection_hops_on_a: vec![conn_id.clone()],
            port_id_on_b: get_port_id(),
            ordering: Order::Unordered,
            signer: "account0".to_string().into(),
            version_proposal: ChanVersion::new(VERSION.to_string()),
        };

        // insert an Init channel
        let channel_key = channel_key(&get_port_id(), &get_channel_id());
        let mut counterparty = get_channel_counterparty();
        counterparty.channel_id = None;
        let channel = ChannelEnd::new(
            ChanState::Init,
            msg.ordering,
            counterparty.clone(),
            msg.connection_hops_on_a.clone(),
            msg.version_proposal.clone(),
        )
        .unwrap();
        let bytes = channel.encode_vec();
        wl_storage
            .write_log
            .write(&channel_key, bytes)
            .expect("write failed");
        keys_changed.insert(channel_key);
        // channel counter
        let chan_counter_key = channel_counter_key();
        increment_counter(&mut wl_storage, &chan_counter_key);
        keys_changed.insert(chan_counter_key);
        // sequences
        let channel_id = get_channel_id();
        let port_id = msg.port_id_on_a.clone();
        let send_key = next_sequence_send_key(&port_id, &channel_id);
        increment_sequence(&mut wl_storage, &send_key);
        keys_changed.insert(send_key);
        let recv_key = next_sequence_recv_key(&port_id, &channel_id);
        increment_sequence(&mut wl_storage, &recv_key);
        keys_changed.insert(recv_key);
        let ack_key = next_sequence_ack_key(&port_id, &channel_id);
        increment_sequence(&mut wl_storage, &ack_key);
        keys_changed.insert(ack_key);
        // event
        let event = RawIbcEvent::OpenInitChannel(ChanOpenInit::new(
            msg.port_id_on_a.clone(),
            get_channel_id(),
            counterparty.port_id().clone(),
            conn_id,
            msg.version_proposal.clone(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Channel);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");
        let mut outer_tx = Tx::from_type(TxType::Raw);
        outer_tx.header.chain_id = wl_storage.storage.chain_id.clone();
        outer_tx.set_code(Code::new(tx_code, None));
        outer_tx.set_data(Data::new(tx_data));
        outer_tx.add_section(Section::Signature(Signature::new(
            vec![outer_tx.header_hash()],
            [(0, keypair_1())].into_iter().collect(),
            None,
        )));
        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &outer_tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        assert!(
            ibc.validate_tx(&outer_tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_try_channel() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);

        // insert an open connection
        let conn_key = connection_key(&get_connection_id());
        let conn = get_connection(ConnState::Open);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // prepare data
        let proof_height = Height::new(0, 1).unwrap();
        let conn_id = get_connection_id();
        let counterparty = get_channel_counterparty();
        #[allow(deprecated)]
        let msg = MsgChannelOpenTry {
            port_id_on_b: get_port_id(),
            connection_hops_on_b: vec![conn_id.clone()],
            port_id_on_a: counterparty.port_id().clone(),
            chan_id_on_a: counterparty.channel_id().cloned().unwrap(),
            version_supported_on_a: ChanVersion::new(VERSION.to_string()),
            proof_chan_end_on_a: dummy_proof(),
            proof_height_on_a: proof_height,
            ordering: Order::Unordered,
            signer: "account0".to_string().into(),
            version_proposal: ChanVersion::default(),
        };

        // insert a TryOpen channel
        let channel_key = channel_key(&get_port_id(), &get_channel_id());
        let channel = get_channel(ChanState::TryOpen, Order::Unordered);
        let bytes = channel.encode_vec();
        wl_storage
            .write_log
            .write(&channel_key, bytes)
            .expect("write failed");
        keys_changed.insert(channel_key);
        // channel counter
        let chan_counter_key = channel_counter_key();
        increment_counter(&mut wl_storage, &chan_counter_key);
        keys_changed.insert(chan_counter_key);
        // sequences
        let channel_id = get_channel_id();
        let port_id = msg.port_id_on_a.clone();
        let send_key = next_sequence_send_key(&port_id, &channel_id);
        increment_sequence(&mut wl_storage, &send_key);
        keys_changed.insert(send_key);
        let recv_key = next_sequence_recv_key(&port_id, &channel_id);
        increment_sequence(&mut wl_storage, &recv_key);
        keys_changed.insert(recv_key);
        let ack_key = next_sequence_ack_key(&port_id, &channel_id);
        increment_sequence(&mut wl_storage, &ack_key);
        keys_changed.insert(ack_key);
        // event
        let event = RawIbcEvent::OpenTryChannel(ChanOpenTry::new(
            msg.port_id_on_a.clone(),
            get_channel_id(),
            counterparty.port_id().clone(),
            counterparty.channel_id().cloned().unwrap(),
            conn_id,
            msg.version_supported_on_a.clone(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Channel);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");
        let mut outer_tx = Tx::from_type(TxType::Raw);
        outer_tx.header.chain_id = wl_storage.storage.chain_id.clone();
        outer_tx.set_code(Code::new(tx_code, None));
        outer_tx.set_data(Data::new(tx_data));
        outer_tx.add_section(Section::Signature(Signature::new(
            vec![outer_tx.header_hash()],
            [(0, keypair_1())].into_iter().collect(),
            None,
        )));
        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &outer_tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        assert!(
            ibc.validate_tx(&outer_tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_ack_channel() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);

        // insert an open connection
        let conn_key = connection_key(&get_connection_id());
        let conn = get_connection(ConnState::Open);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        // insert an Init channel
        let channel_key = channel_key(&get_port_id(), &get_channel_id());
        let channel = get_channel(ChanState::Init, Order::Unordered);
        let bytes = channel.encode_vec();
        wl_storage
            .write_log
            .write(&channel_key, bytes)
            .expect("write failed");
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // prepare data
        let proof_height = Height::new(0, 1).unwrap();
        let counterparty = get_channel_counterparty();
        let msg = MsgChannelOpenAck {
            port_id_on_a: get_port_id(),
            chan_id_on_a: get_channel_id(),
            chan_id_on_b: counterparty.channel_id().cloned().unwrap(),
            version_on_b: ChanVersion::new(VERSION.to_string()),
            proof_chan_end_on_b: dummy_proof(),
            proof_height_on_b: proof_height,
            signer: "account0".to_string().into(),
        };

        // update the channel to Open
        let channel = get_channel(ChanState::Open, Order::Unordered);
        let bytes = channel.encode_vec();
        wl_storage
            .write_log
            .write(&channel_key, bytes)
            .expect("write failed");
        keys_changed.insert(channel_key);
        // event
        let event = RawIbcEvent::OpenAckChannel(ChanOpenAck::new(
            msg.port_id_on_a.clone(),
            msg.chan_id_on_a.clone(),
            counterparty.port_id().clone(),
            counterparty.channel_id().cloned().unwrap(),
            get_connection_id(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Channel);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");
        let mut outer_tx = Tx::from_type(TxType::Raw);
        outer_tx.header.chain_id = wl_storage.storage.chain_id.clone();
        outer_tx.set_code(Code::new(tx_code, None));
        outer_tx.set_data(Data::new(tx_data));
        outer_tx.add_section(Section::Signature(Signature::new(
            vec![outer_tx.header_hash()],
            [(0, keypair_1())].into_iter().collect(),
            None,
        )));
        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &outer_tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        assert!(
            ibc.validate_tx(&outer_tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_confirm_channel() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);

        // insert an open connection
        let conn_key = connection_key(&get_connection_id());
        let conn = get_connection(ConnState::Open);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        // insert a TryOpen channel
        let channel_key = channel_key(&get_port_id(), &get_channel_id());
        let channel = get_channel(ChanState::TryOpen, Order::Ordered);
        let bytes = channel.encode_vec();
        wl_storage
            .write_log
            .write(&channel_key, bytes)
            .expect("write failed");
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // prepare data
        let proof_height = Height::new(0, 1).unwrap();
        let msg = MsgChannelOpenConfirm {
            port_id_on_b: get_port_id(),
            chan_id_on_b: get_channel_id(),
            proof_chan_end_on_a: dummy_proof(),
            proof_height_on_a: proof_height,
            signer: "account0".to_string().into(),
        };

        // update the channel to Open
        let channel = get_channel(ChanState::Open, Order::Ordered);
        let bytes = channel.encode_vec();
        wl_storage
            .write_log
            .write(&channel_key, bytes)
            .expect("write failed");
        keys_changed.insert(channel_key);
        // event
        let counterparty = get_channel_counterparty();
        let event = RawIbcEvent::OpenConfirmChannel(ChanOpenConfirm::new(
            msg.port_id_on_b.clone(),
            msg.chan_id_on_b.clone(),
            counterparty.port_id().clone(),
            counterparty.channel_id().cloned().unwrap(),
            get_connection_id(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Channel);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");

        let mut tx = Tx::new(wl_storage.storage.chain_id.clone(), None);
        tx.add_code(tx_code, None)
            .add_serialized_data(tx_data)
            .sign_wrapper(keypair_1());

        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        assert!(
            ibc.validate_tx(&tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    // skip test_close_init_channel() and test_close_confirm_channel() since it
    // is not allowed to close the transfer channel

    #[test]
    fn test_send_packet() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);

        // insert an open connection
        let conn_key = connection_key(&get_connection_id());
        let conn = get_connection(ConnState::Open);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        // insert an Open channel
        let channel_key = channel_key(&get_port_id(), &get_channel_id());
        let channel = get_channel(ChanState::Open, Order::Unordered);
        let bytes = channel.encode_vec();
        wl_storage
            .write_log
            .write(&channel_key, bytes)
            .expect("write failed");
        // init balance
        let sender = established_address_1();
        let balance_key = balance_key(&nam(), &sender);
        let amount = Amount::native_whole(100);
        wl_storage
            .write_log
            .write(&balance_key, amount.serialize_to_vec())
            .expect("write failed");
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // prepare data
        let msg = IbcMsgTransfer {
            port_id_on_a: get_port_id(),
            chan_id_on_a: get_channel_id(),
            packet_data: PacketData {
                token: PrefixedCoin {
                    denom: nam().to_string().parse().unwrap(),
                    amount: 100.into(),
                },
                sender: sender.to_string().into(),
                receiver: "receiver".to_string().into(),
                memo: "memo".to_string().into(),
            },
            timeout_height_on_b: TimeoutHeight::At(Height::new(0, 10).unwrap()),
            timeout_timestamp_on_b: Timestamp::none(),
        };

        // the sequence send
        let seq_key = next_sequence_send_key(&get_port_id(), &get_channel_id());
        let sequence = get_next_seq(&wl_storage, &seq_key);
        wl_storage
            .write_log
            .write(&seq_key, (u64::from(sequence) + 1).to_be_bytes().to_vec())
            .expect("write failed");
        keys_changed.insert(seq_key);
        // packet commitment
        let packet =
            packet_from_message(&msg, sequence, &get_channel_counterparty());
        let commitment_key =
            commitment_key(&msg.port_id_on_a, &msg.chan_id_on_a, sequence);
        let commitment = commitment(&packet);
        let bytes = commitment.into_vec();
        wl_storage
            .write_log
            .write(&commitment_key, bytes)
            .expect("write failed");
        keys_changed.insert(commitment_key);
        // withdraw
        let withdraw_key = withdraw_key(&nam());
        let bytes = Amount::from_str(
            msg.packet_data.token.amount.to_string(),
            NATIVE_MAX_DECIMAL_PLACES,
        )
        .unwrap()
        .serialize_to_vec();
        wl_storage
            .write_log
            .write(&withdraw_key, bytes)
            .expect("write failed");
        keys_changed.insert(withdraw_key);
        // event
        let transfer_event = TransferEvent {
            sender: msg.packet_data.sender.clone(),
            receiver: msg.packet_data.receiver.clone(),
            amount: msg.packet_data.token.amount,
            denom: msg.packet_data.token.denom.clone(),
            memo: msg.packet_data.memo.clone(),
        };
        let event = RawIbcEvent::Module(ModuleEvent::from(transfer_event));
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());
        let event = RawIbcEvent::SendPacket(SendPacket::new(
            packet,
            Order::Unordered,
            get_connection_id(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Channel);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let tx_data = MsgTransfer {
            message: msg,
            shielded_transfer: None,
        }
        .serialize_to_vec();

        let mut tx = Tx::new(wl_storage.storage.chain_id.clone(), None);
        tx.add_code(tx_code, None)
            .add_serialized_data(tx_data)
            .sign_wrapper(keypair_1());

        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        assert!(
            ibc.validate_tx(&tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_recv_packet() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);

        // insert an open connection
        let conn_key = connection_key(&get_connection_id());
        let conn = get_connection(ConnState::Open);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        // insert an open channel
        let channel_key = channel_key(&get_port_id(), &get_channel_id());
        let channel = get_channel(ChanState::Open, Order::Unordered);
        let bytes = channel.encode_vec();
        wl_storage
            .write_log
            .write(&channel_key, bytes)
            .expect("write failed");
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // prepare data
        let sender = established_address_1();
        let receiver = established_address_2();
        let transfer_msg = IbcMsgTransfer {
            port_id_on_a: get_port_id(),
            chan_id_on_a: get_channel_id(),
            packet_data: PacketData {
                token: PrefixedCoin {
                    denom: nam().to_string().parse().unwrap(),
                    amount: 100u64.into(),
                },
                sender: sender.to_string().into(),
                receiver: receiver.to_string().into(),
                memo: "memo".to_string().into(),
            },
            timeout_height_on_b: TimeoutHeight::At(Height::new(0, 10).unwrap()),
            timeout_timestamp_on_b: Timestamp::none(),
        };
        let counterparty = get_channel_counterparty();
        let packet =
            packet_from_message(&transfer_msg, 1.into(), &counterparty);
        let msg = MsgRecvPacket {
            packet: packet.clone(),
            proof_commitment_on_a: dummy_proof(),
            proof_height_on_a: Height::new(0, 1).unwrap(),
            signer: "account0".to_string().into(),
        };

        // the sequence send
        let receipt_key = receipt_key(
            &msg.packet.port_id_on_b,
            &msg.packet.chan_id_on_b,
            msg.packet.seq_on_a,
        );
        let bytes = [1_u8].to_vec();
        wl_storage
            .write_log
            .write(&receipt_key, bytes)
            .expect("write failed");
        keys_changed.insert(receipt_key);
        // packet commitment
        let ack_key = ack_key(
            &packet.port_id_on_b,
            &packet.chan_id_on_b,
            msg.packet.seq_on_a,
        );
        let transfer_ack = AcknowledgementStatus::success(ack_success_b64());
        let acknowledgement: Acknowledgement = transfer_ack.into();
        let bytes = sha2::Sha256::digest(acknowledgement.as_bytes()).to_vec();
        wl_storage
            .write_log
            .write(&ack_key, bytes)
            .expect("write failed");
        keys_changed.insert(ack_key);

        let mut coin = transfer_msg.packet_data.token;
        coin.denom.add_trace_prefix(TracePrefix::new(
            packet.port_id_on_b.clone(),
            packet.chan_id_on_b.clone(),
        ));
        // mint
        let ibc_token = ibc_token(coin.denom.to_string());
        let mint_key = mint_amount_key(&ibc_token);
        let bytes = Amount::from_str(coin.amount.to_string(), 0)
            .unwrap()
            .serialize_to_vec();
        wl_storage
            .write_log
            .write(&mint_key, bytes)
            .expect("write failed");
        keys_changed.insert(mint_key);
        // deposit
        let deposit_key = deposit_key(&ibc_token);
        let bytes = Amount::from_str(coin.amount.to_string(), 0)
            .unwrap()
            .serialize_to_vec();
        wl_storage
            .write_log
            .write(&deposit_key, bytes)
            .expect("write failed");
        keys_changed.insert(deposit_key);
        // denom
        let trace_hash = calc_hash(coin.denom.to_string());
        let trace_key = ibc_trace_key(receiver.to_string(), &trace_hash);
        let bytes = coin.denom.to_string().serialize_to_vec();
        wl_storage
            .write_log
            .write(&trace_key, bytes)
            .expect("write failed");
        keys_changed.insert(trace_key);
        let trace_key = ibc_trace_key(nam().to_string(), &trace_hash);
        let bytes = coin.denom.to_string().serialize_to_vec();
        wl_storage
            .write_log
            .write(&trace_key, bytes)
            .expect("write failed");
        keys_changed.insert(trace_key);
        // event
        let recv_event = RecvEvent {
            sender: sender.to_string().into(),
            receiver: receiver.to_string().into(),
            denom: nam().to_string().parse().unwrap(),
            amount: 100u64.into(),
            memo: "memo".to_string().into(),
            success: true,
        };
        let event = RawIbcEvent::Module(ModuleEvent::from(recv_event));
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());
        let denom_trace_event = DenomTraceEvent {
            trace_hash: Some(trace_hash),
            denom: coin.denom,
        };
        let event = RawIbcEvent::Module(ModuleEvent::from(denom_trace_event));
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());
        let event = RawIbcEvent::ReceivePacket(ReceivePacket::new(
            msg.packet.clone(),
            Order::Unordered,
            get_connection_id(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Channel);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());
        let event =
            RawIbcEvent::WriteAcknowledgement(WriteAcknowledgement::new(
                packet,
                acknowledgement,
                get_connection_id(),
            ));
        let message_event = RawIbcEvent::Message(MessageEvent::Channel);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");

        let mut tx = Tx::new(wl_storage.storage.chain_id.clone(), None);
        tx.add_code(tx_code, None)
            .add_serialized_data(tx_data)
            .sign_wrapper(keypair_1());

        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        assert!(
            ibc.validate_tx(&tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_ack_packet() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);

        // insert an open connection
        let conn_key = connection_key(&get_connection_id());
        let conn = get_connection(ConnState::Open);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        // insert an Open channel
        let channel_key = channel_key(&get_port_id(), &get_channel_id());
        let channel = get_channel(ChanState::Open, Order::Unordered);
        let bytes = channel.encode_vec();
        wl_storage
            .write_log
            .write(&channel_key, bytes)
            .expect("write failed");
        // commitment
        let sender = established_address_1();
        let transfer_msg = IbcMsgTransfer {
            port_id_on_a: get_port_id(),
            chan_id_on_a: get_channel_id(),
            packet_data: PacketData {
                token: PrefixedCoin {
                    denom: nam().to_string().parse().unwrap(),
                    amount: 100u64.into(),
                },
                sender: sender.to_string().into(),
                receiver: "receiver".to_string().into(),
                memo: "memo".to_string().into(),
            },
            timeout_height_on_b: TimeoutHeight::At(Height::new(0, 10).unwrap()),
            timeout_timestamp_on_b: Timestamp::none(),
        };
        let sequence = 1.into();
        let packet = packet_from_message(
            &transfer_msg,
            sequence,
            &get_channel_counterparty(),
        );
        let commitment_key = commitment_key(
            &transfer_msg.port_id_on_a,
            &transfer_msg.chan_id_on_a,
            sequence,
        );
        let commitment = commitment(&packet);
        let bytes = commitment.into_vec();
        wl_storage
            .write_log
            .write(&commitment_key, bytes)
            .expect("write failed");
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // prepare data
        let transfer_ack = AcknowledgementStatus::success(ack_success_b64());
        let msg = MsgAcknowledgement {
            packet: packet.clone(),
            acknowledgement: transfer_ack.clone().into(),
            proof_acked_on_b: dummy_proof(),
            proof_height_on_b: Height::new(0, 1).unwrap(),
            signer: "account0".to_string().into(),
        };

        // delete the commitment
        wl_storage
            .write_log
            .delete(&commitment_key)
            .expect("delete failed");
        keys_changed.insert(commitment_key);
        // event
        let data = serde_json::from_slice::<PacketData>(&packet.data)
            .expect("decoding packet data failed");
        let ack_event = AckEvent {
            sender: data.sender,
            receiver: data.receiver,
            denom: data.token.denom,
            amount: data.token.amount,
            memo: data.memo,
            acknowledgement: transfer_ack,
        };
        let event = RawIbcEvent::Module(ModuleEvent::from(ack_event));
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());
        let event = RawIbcEvent::AcknowledgePacket(AcknowledgePacket::new(
            packet,
            Order::Unordered,
            get_connection_id(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Channel);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");

        let mut tx = Tx::new(wl_storage.storage.chain_id.clone(), None);
        tx.add_code(tx_code, None)
            .add_serialized_data(tx_data)
            .sign_wrapper(keypair_1());

        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        assert!(
            ibc.validate_tx(&tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_timeout_packet() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);

        // insert an open connection
        let conn_key = connection_key(&get_connection_id());
        let conn = get_connection(ConnState::Open);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        // insert an Open channel
        let channel_key = channel_key(&get_port_id(), &get_channel_id());
        let channel = get_channel(ChanState::Open, Order::Unordered);
        let bytes = channel.encode_vec();
        wl_storage
            .write_log
            .write(&channel_key, bytes)
            .expect("write failed");
        // init the escrow balance
        let balance_key =
            balance_key(&nam(), &Address::Internal(InternalAddress::Ibc));
        let amount = Amount::native_whole(100);
        wl_storage
            .write_log
            .write(&balance_key, amount.serialize_to_vec())
            .expect("write failed");
        // commitment
        let transfer_msg = IbcMsgTransfer {
            port_id_on_a: get_port_id(),
            chan_id_on_a: get_channel_id(),
            packet_data: PacketData {
                token: PrefixedCoin {
                    denom: nam().to_string().parse().unwrap(),
                    amount: 100u64.into(),
                },
                sender: established_address_1().to_string().into(),
                receiver: "receiver".to_string().into(),
                memo: "memo".to_string().into(),
            },
            timeout_height_on_b: TimeoutHeight::Never,
            timeout_timestamp_on_b: (Timestamp::now() - Duration::new(10, 0))
                .unwrap(),
        };
        let sequence = 1.into();
        let packet = packet_from_message(
            &transfer_msg,
            sequence,
            &get_channel_counterparty(),
        );
        let commitment_key = commitment_key(
            &transfer_msg.port_id_on_a,
            &transfer_msg.chan_id_on_a,
            sequence,
        );
        let commitment = commitment(&packet);
        let bytes = commitment.into_vec();
        wl_storage
            .write_log
            .write(&commitment_key, bytes)
            .expect("write failed");
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // prepare data
        let msg = MsgTimeout {
            packet: packet.clone(),
            next_seq_recv_on_b: sequence,
            proof_unreceived_on_b: dummy_proof(),
            proof_height_on_b: Height::new(0, 1).unwrap(),
            signer: "account0".to_string().into(),
        };

        // delete the commitment
        wl_storage
            .write_log
            .delete(&commitment_key)
            .expect("delete failed");
        keys_changed.insert(commitment_key);
        // deposit
        let data = serde_json::from_slice::<PacketData>(&packet.data)
            .expect("decoding packet data failed");
        let deposit_key = deposit_key(&nam());
        let bytes = Amount::from_str(
            data.token.amount.to_string(),
            NATIVE_MAX_DECIMAL_PLACES,
        )
        .unwrap()
        .serialize_to_vec();
        wl_storage
            .write_log
            .write(&deposit_key, bytes)
            .expect("write failed");
        keys_changed.insert(deposit_key);
        // event
        let timeout_event = TimeoutEvent {
            refund_receiver: data.sender,
            refund_denom: data.token.denom,
            refund_amount: data.token.amount,
            memo: data.memo,
        };
        let event = RawIbcEvent::Module(ModuleEvent::from(timeout_event));
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());
        let event = RawIbcEvent::TimeoutPacket(TimeoutPacket::new(
            packet,
            Order::Unordered,
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Channel);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");

        let mut tx = Tx::new(wl_storage.storage.chain_id.clone(), None);
        tx.add_code(tx_code, None)
            .add_serialized_data(tx_data)
            .sign_wrapper(keypair_1());

        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        assert!(
            ibc.validate_tx(&tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_timeout_on_close_packet() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);

        // insert an open connection
        let conn_key = connection_key(&get_connection_id());
        let conn = get_connection(ConnState::Open);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        // insert an Open channel
        let channel_key = channel_key(&get_port_id(), &get_channel_id());
        let channel = get_channel(ChanState::Open, Order::Unordered);
        let bytes = channel.encode_vec();
        wl_storage
            .write_log
            .write(&channel_key, bytes)
            .expect("write failed");
        // init the escrow balance
        let balance_key =
            balance_key(&nam(), &Address::Internal(InternalAddress::Ibc));
        let amount = Amount::native_whole(100);
        wl_storage
            .write_log
            .write(&balance_key, amount.serialize_to_vec())
            .expect("write failed");
        // commitment
        let sender = established_address_1();
        let transfer_msg = IbcMsgTransfer {
            port_id_on_a: get_port_id(),
            chan_id_on_a: get_channel_id(),
            packet_data: PacketData {
                token: PrefixedCoin {
                    denom: nam().to_string().parse().unwrap(),
                    amount: 100u64.into(),
                },
                sender: sender.to_string().into(),
                receiver: "receiver".to_string().into(),
                memo: "memo".to_string().into(),
            },
            timeout_height_on_b: TimeoutHeight::At(Height::new(0, 10).unwrap()),
            timeout_timestamp_on_b: Timestamp::none(),
        };
        let sequence = 1.into();
        let packet = packet_from_message(
            &transfer_msg,
            sequence,
            &get_channel_counterparty(),
        );
        let commitment_key = commitment_key(
            &transfer_msg.port_id_on_a,
            &transfer_msg.chan_id_on_a,
            sequence,
        );
        let commitment = commitment(&packet);
        let bytes = commitment.into_vec();
        wl_storage
            .write_log
            .write(&commitment_key, bytes)
            .expect("write failed");
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // prepare data
        let msg = MsgTimeoutOnClose {
            packet: packet.clone(),
            next_seq_recv_on_b: sequence,
            proof_unreceived_on_b: dummy_proof(),
            proof_close_on_b: dummy_proof(),
            proof_height_on_b: Height::new(0, 1).unwrap(),
            signer: "account0".to_string().into(),
        };

        // delete the commitment
        wl_storage
            .write_log
            .delete(&commitment_key)
            .expect("delete failed");
        keys_changed.insert(commitment_key);
        // deposit
        let data = serde_json::from_slice::<PacketData>(&packet.data)
            .expect("decoding packet data failed");
        let deposit_key = deposit_key(&nam());
        let bytes = Amount::from_str(
            data.token.amount.to_string(),
            NATIVE_MAX_DECIMAL_PLACES,
        )
        .unwrap()
        .serialize_to_vec();
        wl_storage
            .write_log
            .write(&deposit_key, bytes)
            .expect("write failed");
        keys_changed.insert(deposit_key);
        // event
        let timeout_event = TimeoutEvent {
            refund_receiver: data.sender,
            refund_denom: data.token.denom,
            refund_amount: data.token.amount,
            memo: data.memo,
        };
        let event = RawIbcEvent::Module(ModuleEvent::from(timeout_event));
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());
        let event = RawIbcEvent::TimeoutPacket(TimeoutPacket::new(
            packet,
            Order::Unordered,
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Channel);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");

        let mut tx = Tx::new(wl_storage.storage.chain_id.clone(), None);
        tx.add_code(tx_code, None)
            .add_serialized_data(tx_data)
            .sign_wrapper(keypair_1());

        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        assert!(
            ibc.validate_tx(&tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_send_packet_for_nft() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);

        // insert an open connection
        let conn_key = connection_key(&get_connection_id());
        let conn = get_connection(ConnState::Open);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        // insert an Open channel
        let channel_key = channel_key(&get_nft_port_id(), &get_channel_id());
        let channel = get_channel_for_nft(ChanState::Open, Order::Unordered);
        let bytes = channel.encode_vec();
        wl_storage
            .write_log
            .write(&channel_key, bytes)
            .expect("write failed");
        // init nft
        let class_id = get_nft_class_id();
        let token_id = get_nft_id();
        let sender = established_address_1();
        let ibc_token = ibc::storage::ibc_token_for_nft(&class_id, &token_id);
        let balance_key = balance_key(&ibc_token, &sender);
        let amount = Amount::from_u64(1);
        wl_storage
            .write_log
            .write(&balance_key, amount.serialize_to_vec())
            .expect("write failed");
        // nft class
        let class = dummy_nft_class();
        let class_key = ibc::storage::nft_class_key(&class_id);
        wl_storage
            .write_log
            .write(&class_key, class.serialize_to_vec())
            .expect("write failed");
        // nft metadata
        let metadata = dummy_nft_metadata();
        let metadata_key = ibc::storage::nft_metadata_key(&class_id, &token_id);
        wl_storage
            .write_log
            .write(&metadata_key, metadata.serialize_to_vec())
            .expect("write failed");

        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // prepare data
        let msg = IbcMsgNftTransfer {
            port_id_on_a: get_nft_port_id(),
            chan_id_on_a: get_channel_id(),
            packet_data: NftPacketData {
                class_id,
                class_uri: None,
                class_data: None,
                token_ids: TokenIds(vec![token_id]),
                token_uris: None,
                token_data: None,
                sender: sender.to_string().into(),
                receiver: "receiver".to_string().into(),
                memo: Some("memo".to_string().into()),
            },
            timeout_height_on_b: TimeoutHeight::At(Height::new(0, 10).unwrap()),
            timeout_timestamp_on_b: Timestamp::none(),
        };

        // the sequence send
        let seq_key =
            next_sequence_send_key(&get_nft_port_id(), &get_channel_id());
        let sequence = get_next_seq(&wl_storage, &seq_key);
        wl_storage
            .write_log
            .write(&seq_key, (u64::from(sequence) + 1).to_be_bytes().to_vec())
            .expect("write failed");
        keys_changed.insert(seq_key);
        // packet commitment
        let packet = nft_packet_from_message(
            &msg,
            sequence,
            &get_channel_counterparty_for_nft(),
        );
        let commitment_key =
            commitment_key(&msg.port_id_on_a, &msg.chan_id_on_a, sequence);
        let commitment = commitment(&packet);
        let bytes = commitment.into_vec();
        wl_storage
            .write_log
            .write(&commitment_key, bytes)
            .expect("write failed");
        keys_changed.insert(commitment_key);
        // withdraw
        let withdraw_key = withdraw_key(&ibc_token);
        let bytes = Amount::from_u64(1).serialize_to_vec();
        wl_storage
            .write_log
            .write(&withdraw_key, bytes)
            .expect("write failed");
        keys_changed.insert(withdraw_key);
        // event
        let transfer_event = NftTransferEvent {
            sender: msg.packet_data.sender.clone(),
            receiver: msg.packet_data.receiver.clone(),
            class: msg.packet_data.class_id.clone(),
            tokens: msg.packet_data.token_ids.clone(),
            memo: msg.packet_data.memo.clone().unwrap_or_default(),
        };
        let event = RawIbcEvent::Module(ModuleEvent::from(transfer_event));
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());
        let event = RawIbcEvent::SendPacket(SendPacket::new(
            packet,
            Order::Unordered,
            get_connection_id(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Channel);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let tx_data = MsgNftTransfer {
            message: msg,
            shielded_transfer: None,
        }
        .serialize_to_vec();

        let mut tx = Tx::new(wl_storage.storage.chain_id.clone(), None);
        tx.add_code(tx_code, None)
            .add_serialized_data(tx_data)
            .sign_wrapper(keypair_1());

        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        assert!(
            ibc.validate_tx(&tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }

    #[test]
    fn test_recv_packet_for_nft() {
        let mut keys_changed = BTreeSet::new();
        let mut wl_storage = init_storage();
        insert_init_client(&mut wl_storage);

        // insert an open connection
        let conn_key = connection_key(&get_connection_id());
        let conn = get_connection(ConnState::Open);
        let bytes = conn.encode_vec();
        wl_storage
            .write_log
            .write(&conn_key, bytes)
            .expect("write failed");
        // insert an open channel
        let channel_key = channel_key(&get_nft_port_id(), &get_channel_id());
        let channel = get_channel_for_nft(ChanState::Open, Order::Unordered);
        let bytes = channel.encode_vec();
        wl_storage
            .write_log
            .write(&channel_key, bytes)
            .expect("write failed");
        wl_storage.write_log.commit_tx();
        wl_storage.commit_block().expect("commit failed");
        // for next block
        wl_storage
            .storage
            .set_header(get_dummy_header())
            .expect("Setting a dummy header shouldn't fail");
        wl_storage
            .storage
            .begin_block(BlockHash::default(), BlockHeight(2))
            .unwrap();

        // prepare data
        let sender = established_address_1();
        let receiver = established_address_2();
        let class = dummy_nft_class();
        let metadata = dummy_nft_metadata();
        let transfer_msg = IbcMsgNftTransfer {
            port_id_on_a: get_nft_port_id(),
            chan_id_on_a: get_channel_id(),
            packet_data: NftPacketData {
                class_id: class.class_id.clone(),
                class_uri: class.class_uri.clone(),
                class_data: class.class_data,
                token_ids: TokenIds(vec![metadata.token_id.clone()]),
                token_uris: Some(vec![metadata.token_uri.unwrap()]),
                token_data: Some(vec![metadata.token_data.unwrap()]),
                sender: sender.to_string().into(),
                receiver: receiver.to_string().into(),
                memo: Some("memo".to_string().into()),
            },
            timeout_height_on_b: TimeoutHeight::At(Height::new(0, 10).unwrap()),
            timeout_timestamp_on_b: Timestamp::none(),
        };
        let counterparty = get_channel_counterparty_for_nft();
        let packet =
            nft_packet_from_message(&transfer_msg, 1.into(), &counterparty);
        let msg = MsgRecvPacket {
            packet: packet.clone(),
            proof_commitment_on_a: dummy_proof(),
            proof_height_on_a: Height::new(0, 1).unwrap(),
            signer: "account0".to_string().into(),
        };

        // the sequence send
        let receipt_key = receipt_key(
            &msg.packet.port_id_on_b,
            &msg.packet.chan_id_on_b,
            msg.packet.seq_on_a,
        );
        let bytes = [1_u8].to_vec();
        wl_storage
            .write_log
            .write(&receipt_key, bytes)
            .expect("write failed");
        keys_changed.insert(receipt_key);
        // packet commitment
        let ack_key = ack_key(
            &packet.port_id_on_b,
            &packet.chan_id_on_b,
            msg.packet.seq_on_a,
        );
        let transfer_ack =
            AcknowledgementStatus::success(nft_types::ack_success_b64());
        let acknowledgement: Acknowledgement = transfer_ack.into();
        let bytes = sha2::Sha256::digest(acknowledgement.as_bytes()).to_vec();
        wl_storage
            .write_log
            .write(&ack_key, bytes)
            .expect("write failed");
        keys_changed.insert(ack_key);
        // trace
        let mut class_id = transfer_msg.packet_data.class_id.clone();
        class_id.add_trace_prefix(nft_types::TracePrefix::new(
            packet.port_id_on_b.clone(),
            packet.chan_id_on_b.clone(),
        ));
        let token_id = transfer_msg.packet_data.token_ids.0.first().unwrap();
        let ibc_trace = format!("{class_id}/{token_id}");
        let trace_hash = calc_hash(&ibc_trace);
        let trace_key = ibc_trace_key(receiver.to_string(), &trace_hash);
        let bytes = ibc_trace.serialize_to_vec();
        wl_storage
            .write_log
            .write(&trace_key, bytes)
            .expect("write failed");
        keys_changed.insert(trace_key);
        let trace_key = ibc_trace_key(token_id, &trace_hash);
        let bytes = ibc_trace.serialize_to_vec();
        wl_storage
            .write_log
            .write(&trace_key, bytes)
            .expect("write failed");
        keys_changed.insert(trace_key);
        // NFT class
        let class_key = nft_class_key(&class_id);
        let mut class = dummy_nft_class();
        class.class_id = class_id.clone();
        let bytes = class.serialize_to_vec();
        wl_storage
            .write_log
            .write(&class_key, bytes)
            .expect("write failed");
        keys_changed.insert(class_key);
        // NFT metadata
        let metadata_key = nft_metadata_key(&class_id, token_id);
        let mut metadata = dummy_nft_metadata();
        metadata.class_id = class_id.clone();
        let bytes = metadata.serialize_to_vec();
        wl_storage
            .write_log
            .write(&metadata_key, bytes)
            .expect("write failed");
        keys_changed.insert(metadata_key);
        // mint
        let ibc_token = ibc_token(&ibc_trace);
        let mint_key = mint_amount_key(&ibc_token);
        let bytes = Amount::from_u64(1).serialize_to_vec();
        wl_storage
            .write_log
            .write(&mint_key, bytes)
            .expect("write failed");
        keys_changed.insert(mint_key);
        // deposit
        let deposit_key = deposit_key(&ibc_token);
        let bytes = Amount::from_u64(1).serialize_to_vec();
        wl_storage
            .write_log
            .write(&deposit_key, bytes)
            .expect("write failed");
        keys_changed.insert(deposit_key);
        // event
        let recv_event = NftRecvEvent {
            sender: sender.to_string().into(),
            receiver: receiver.to_string().into(),
            class: transfer_msg.packet_data.class_id.clone(),
            tokens: TokenIds(vec![token_id.clone()]),
            memo: "memo".to_string().into(),
            success: true,
        };
        let event = RawIbcEvent::Module(ModuleEvent::from(recv_event));
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());
        let trace_event = TokenTraceEvent {
            trace_hash: Some(trace_hash),
            class: class_id,
            token: token_id.clone(),
        };
        let event = RawIbcEvent::Module(ModuleEvent::from(trace_event));
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());
        let event = RawIbcEvent::ReceivePacket(ReceivePacket::new(
            msg.packet.clone(),
            Order::Unordered,
            get_connection_id(),
        ));
        let message_event = RawIbcEvent::Message(MessageEvent::Channel);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());
        let event =
            RawIbcEvent::WriteAcknowledgement(WriteAcknowledgement::new(
                packet,
                acknowledgement,
                get_connection_id(),
            ));
        let message_event = RawIbcEvent::Message(MessageEvent::Channel);
        wl_storage
            .write_log
            .emit_ibc_event(message_event.try_into().unwrap());
        wl_storage
            .write_log
            .emit_ibc_event(event.try_into().unwrap());

        let tx_index = TxIndex::default();
        let tx_code = vec![];
        let mut tx_data = vec![];
        msg.to_any().encode(&mut tx_data).expect("encoding failed");

        let mut tx = Tx::new(wl_storage.storage.chain_id.clone(), None);
        tx.add_code(tx_code, None)
            .add_serialized_data(tx_data)
            .sign_wrapper(keypair_1());

        let gas_meter = VpGasMeter::new_from_tx_meter(
            &TxGasMeter::new_from_sub_limit(TX_GAS_LIMIT.into()),
        );
        let (vp_wasm_cache, _vp_cache_dir) =
            wasm::compilation_cache::common::testing::cache();

        let verifiers = BTreeSet::new();
        let ctx = Ctx::new(
            &ADDRESS,
            &wl_storage.storage,
            &wl_storage.write_log,
            &tx,
            &tx_index,
            gas_meter,
            &keys_changed,
            &verifiers,
            vp_wasm_cache,
        );
        let ibc = Ibc { ctx };
        assert!(
            ibc.validate_tx(&tx, &keys_changed, &verifiers)
                .expect("validation failed")
        );
    }
}

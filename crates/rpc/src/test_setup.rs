use pathfinder_common::{macro_prelude::*, StarknetVersion, StorageAddress};

use pathfinder_common::{
    felt, BlockHash, BlockHeader, BlockNumber, BlockTimestamp, ContractAddress, GasPrice,
    StateUpdate,
};
use pathfinder_storage::Storage;
use starknet_gateway_test_fixtures::class_definitions::{DUMMY_ACCOUNT, DUMMY_ACCOUNT_CLASS_HASH};

use crate::context::RpcContext;

pub async fn test_storage<F: FnOnce(StateUpdate) -> StateUpdate>(
    customize_state_update: F,
) -> (Storage, BlockHeader, ContractAddress, ContractAddress) {
    let storage = Storage::in_memory().unwrap();
    let mut db = storage.connection().unwrap();
    let tx = db.transaction().unwrap();

    // Empty genesis block
    let header = BlockHeader::builder()
        .with_number(BlockNumber::GENESIS)
        .with_timestamp(BlockTimestamp::new_or_panic(0))
        .finalize_with_hash(BlockHash(felt!("0xb00")));
    tx.insert_block_header(&header).unwrap();

    // Declare & deploy an account class, a universal deployer class and the fee token ERC20 class
    let block1_number = BlockNumber::GENESIS + 1;
    let block1_hash = BlockHash(felt!("0xb01"));

    tx.insert_cairo_class(DUMMY_ACCOUNT_CLASS_HASH, DUMMY_ACCOUNT)
        .unwrap();

    let universal_deployer_definition =
        include_bytes!("../fixtures/contracts/universal_deployer.json");
    let universal_deployer_class_hash =
        class_hash!("0x06f38fb91ddbf325a0625533576bb6f6eafd9341868a9ec3faa4b01ce6c4f4dc");

    tx.insert_cairo_class(universal_deployer_class_hash, universal_deployer_definition)
        .unwrap();

    let erc20_class_hash =
        starknet_gateway_test_fixtures::class_definitions::ERC20_CONTRACT_DEFINITION_CLASS_HASH;
    let erc20_class_definition =
        starknet_gateway_test_fixtures::class_definitions::ERC20_CONTRACT_DEFINITION;

    tx.insert_cairo_class(erc20_class_hash, erc20_class_definition)
        .unwrap();

    let header = BlockHeader::builder()
        .with_number(block1_number)
        .with_timestamp(BlockTimestamp::new_or_panic(1))
        .with_eth_l1_gas_price(GasPrice(1))
        .with_sequencer_address(sequencer_address!(
            "0x1176a1bd84444c89232ec27754698e5d2e7e1a7f1539f12027f28b23ec9f3d8"
        ))
        .with_starknet_version(StarknetVersion::new(0, 12, 3))
        .finalize_with_hash(block1_hash);
    tx.insert_block_header(&header).unwrap();

    let account_contract_address = contract_address!("0xc01");
    let universal_deployer_address = contract_address!("0xc02");

    let account_balance_key =
        StorageAddress::from_map_name_and_key(b"ERC20_balances", account_contract_address.0);

    let state_update = StateUpdate::default()
        .with_block_hash(block1_hash)
        .with_declared_cairo_class(DUMMY_ACCOUNT_CLASS_HASH)
        .with_declared_cairo_class(universal_deployer_class_hash)
        .with_declared_cairo_class(erc20_class_hash)
        .with_deployed_contract(account_contract_address, DUMMY_ACCOUNT_CLASS_HASH)
        .with_deployed_contract(universal_deployer_address, universal_deployer_class_hash)
        .with_deployed_contract(pathfinder_executor::ETH_FEE_TOKEN_ADDRESS, erc20_class_hash)
        .with_storage_update(
            pathfinder_executor::ETH_FEE_TOKEN_ADDRESS,
            account_balance_key,
            storage_value!("0x10000000000000000000000000000"),
        );
    let state_update = customize_state_update(state_update);
    tx.insert_state_update(block1_number, &state_update)
        .unwrap();

    tx.commit().unwrap();

    (
        storage,
        header,
        account_contract_address,
        universal_deployer_address,
    )
}

pub async fn test_context() -> (RpcContext, BlockHeader, ContractAddress, ContractAddress) {
    let (storage, header, account_contract_address, universal_deployer_address) =
        test_storage(|state_update| state_update).await;

    let context = RpcContext::for_tests().with_storage(storage);

    (
        context,
        header,
        account_contract_address,
        universal_deployer_address,
    )
}

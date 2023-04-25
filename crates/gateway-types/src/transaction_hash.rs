//! Calculate transaction hashes.

use crate::reply::transaction::{
    DeclareTransaction, DeclareTransactionV0V1, DeclareTransactionV2, DeployAccountTransaction,
    DeployTransaction, InvokeTransaction, InvokeTransactionV0, InvokeTransactionV1,
    L1HandlerTransaction, Transaction,
};
use pathfinder_common::{
    CasmHash, ClassHash, ContractAddress, EntryPoint, Fee, StarknetTransactionHash,
    TransactionNonce, TransactionVersion,
};

use crate::class_hash::truncated_keccak;
use anyhow::{Context, Result};
use pathfinder_common::ChainId;
use sha3::{Digest, Keccak256};
use stark_hash::{Felt, HashChain};

#[derive(Debug, PartialEq)]
pub enum ComputedTransactionHash {
    DeclareV0(StarknetTransactionHash),
    DeclareV1(StarknetTransactionHash),
    DeclareV2(StarknetTransactionHash),
    Deploy(StarknetTransactionHash),
    DeployAccount(StarknetTransactionHash),
    InvokeV0(StarknetTransactionHash),
    InvokeV1(StarknetTransactionHash),
    L1Handler(StarknetTransactionHash),
}

impl ComputedTransactionHash {
    pub fn hash(&self) -> StarknetTransactionHash {
        match self {
            ComputedTransactionHash::DeclareV0(h) => *h,
            ComputedTransactionHash::DeclareV1(h) => *h,
            ComputedTransactionHash::DeclareV2(h) => *h,
            ComputedTransactionHash::Deploy(h) => *h,
            ComputedTransactionHash::DeployAccount(h) => *h,
            ComputedTransactionHash::InvokeV0(h) => *h,
            ComputedTransactionHash::InvokeV1(h) => *h,
            ComputedTransactionHash::L1Handler(h) => *h,
        }
    }
}

/// Computes transaction hash according to the formulas from [starknet docs](https://docs.starknet.io/documentation/architecture_and_concepts/Blocks/transactions/).
pub fn compute_transaction_hash(
    txn: &Transaction,
    chain_id: ChainId,
) -> Result<ComputedTransactionHash> {
    match txn {
        Transaction::Declare(DeclareTransaction::V0(txn)) => compute_declare_v0_hash(txn, chain_id),
        Transaction::Declare(DeclareTransaction::V1(txn)) => compute_declare_v1_hash(txn, chain_id),
        Transaction::Declare(DeclareTransaction::V2(txn)) => compute_declare_v2_hash(txn, chain_id),
        Transaction::Deploy(txn) => compute_deploy_hash(txn, chain_id),
        Transaction::DeployAccount(txn) => compute_deploy_account_hash(txn, chain_id),
        Transaction::Invoke(InvokeTransaction::V0(txn)) => compute_invoke_v0_hash(txn, chain_id),
        Transaction::Invoke(InvokeTransaction::V1(txn)) => compute_invoke_v1_hash(txn, chain_id),
        Transaction::L1Handler(txn) => compute_l1_handler_hash(txn, chain_id),
    }
}

/// Computes declare v0 transaction hash based on [this formula](https://docs.starknet.io/documentation/architecture_and_concepts/Blocks/transactions/#v0_hash_calculation_2):
/// ```text=
/// declare_v0_tx_hash = h("declare", version, sender_address,
///     0, h([]), max_fee, chain_id, class_hash)
/// ```
///
/// FIXME: SW should fix the formula in the docs
///
/// Where `h` is [Pedersen hash](https://docs.starknet.io/documentation/architecture_and_concepts/Hashing/hash-functions/#pedersen_hash)
fn compute_declare_v0_hash(
    txn: &DeclareTransactionV0V1,
    chain_id: ChainId,
) -> Result<ComputedTransactionHash> {
    compute_txn_hash_ge_0_8_0(
        b"declare",
        TransactionVersion::ZERO,
        txn.sender_address,
        None,
        HashChain::default().finalize(), // Hash of an empty Felt list
        None,
        chain_id,
        txn.class_hash,
        None,
    )
    .map(ComputedTransactionHash::DeclareV0)
}

/// Computes declare v1 transaction hash based on [this formula](https://docs.starknet.io/documentation/architecture_and_concepts/Blocks/transactions/#v1_hash_calculation_2):
/// ```text=
/// declare_v1_tx_hash = h("declare", version, sender_address,
///     0, h([class_hash]), max_fee, chain_id, nonce)
/// ```
///
/// FIXME: SW should fix the formula in the docs
///
/// Where `h` is [Pedersen hash](https://docs.starknet.io/documentation/architecture_and_concepts/Hashing/hash-functions/#pedersen_hash)
fn compute_declare_v1_hash(
    txn: &DeclareTransactionV0V1,
    chain_id: ChainId,
) -> Result<ComputedTransactionHash> {
    compute_txn_hash_ge_0_8_0(
        b"declare",
        TransactionVersion::ONE,
        txn.sender_address,
        None,
        {
            let mut h = HashChain::default();
            h.update(txn.class_hash.0);
            h.finalize()
        },
        Some(txn.max_fee),
        chain_id,
        txn.nonce,
        None,
    )
    .map(ComputedTransactionHash::DeclareV1)
}

/// Computes declare v2 transaction hash based on [this formula](https://docs.starknet.io/documentation/architecture_and_concepts/Blocks/transactions/#v2_hash_calculation):
/// ```text=
/// declare_v2_tx_hash = h("declare", version, sender_address,
///     0, h([class_hash]), max_fee, chain_id, nonce, compiled_class_hash)
/// ```
///
/// FIXME: SW should fix the formula in the docs
///
/// Where `h` is [Pedersen hash](https://docs.starknet.io/documentation/architecture_and_concepts/Hashing/hash-functions/#pedersen_hash)
fn compute_declare_v2_hash(
    txn: &DeclareTransactionV2,
    chain_id: ChainId,
) -> Result<ComputedTransactionHash> {
    compute_txn_hash_ge_0_8_0(
        b"declare",
        TransactionVersion::TWO,
        txn.sender_address,
        None,
        {
            let mut h = HashChain::default();
            h.update(txn.class_hash.0);
            h.finalize()
        },
        Some(txn.max_fee),
        chain_id,
        txn.nonce,
        Some(txn.compiled_class_hash),
    )
    .map(ComputedTransactionHash::DeclareV2)
}

/// Computes deploy transaction hash based on [this formula](https://docs.starknet.io/documentation/architecture_and_concepts/Blocks/transactions/#deploy_transaction):
/// ```text=
/// deploy_tx_hash = h(
///     "deploy", version, contract_address, sn_keccak("constructor"),
///     h(constructor_calldata), 0, chain_id)
/// ```
///
/// Where `h` is [Pedersen hash](https://docs.starknet.io/documentation/architecture_and_concepts/Hashing/hash-functions/#pedersen_hash), and `sn_keccak` is [Starknet Keccak](https://docs.starknet.io/documentation/architecture_and_concepts/Hashing/hash-functions/#Starknet-keccak)
fn compute_deploy_hash(
    txn: &DeployTransaction,
    chain_id: ChainId,
) -> Result<ComputedTransactionHash> {
    lazy_static::lazy_static!(
        static ref CONSTRUCTOR: EntryPoint = {
            let mut keccak = Keccak256::default();
            keccak.update(b"constructor");
            EntryPoint(truncated_keccak(<[u8; 32]>::from(keccak.finalize())))};
    );

    let constructor_params_hash = {
        let hh = txn.constructor_calldata.iter().fold(
            HashChain::default(),
            |mut hh, constructor_param| {
                hh.update(constructor_param.0);
                hh
            },
        );
        hh.finalize()
    };
    let h = compute_txn_hash_lt_0_8_0(
        b"deploy",
        txn.contract_address,
        Some(*CONSTRUCTOR),
        constructor_params_hash,
        chain_id,
    )?;
    {
        if h == txn.transaction_hash {
            Ok(h)
        } else {
            compute_txn_hash_ge_0_8_0(
                b"deploy",
                txn.version,
                txn.contract_address,
                Some(*CONSTRUCTOR),
                constructor_params_hash,
                None,
                chain_id,
                (),
                None,
            )
        }
    }
    .map(ComputedTransactionHash::Deploy)
}

/// Computes deploy account transaction hash based on [this formula](https://docs.starknet.io/documentation/architecture_and_concepts/Blocks/transactions/#deploy_account_hash_calculation):
/// ```text=
/// deploy_account_tx_hash = h(
///     "deploy_account", version, contract_address, 0,
///     h(class_hash, contract_address_salt, constructor_calldata),
///     max_fee, chain_id, nonce)
/// ```
///
/// FIXME: SW should fix the formula in the docs
///
/// Where `h` is [Pedersen hash](https://docs.starknet.io/documentation/architecture_and_concepts/Hashing/hash-functions/#pedersen_hash)
fn compute_deploy_account_hash(
    txn: &DeployAccountTransaction,
    chain_id: ChainId,
) -> Result<ComputedTransactionHash> {
    compute_txn_hash_ge_0_8_0(
        b"deploy_account",
        txn.version,
        txn.contract_address,
        None,
        {
            let mut hh = HashChain::default();
            hh.update(txn.class_hash.0);
            hh.update(txn.contract_address_salt.0);
            hh = txn
                .constructor_calldata
                .iter()
                .fold(hh, |mut hh, constructor_param| {
                    hh.update(constructor_param.0);
                    hh
                });
            hh.finalize()
        },
        Some(txn.max_fee),
        chain_id,
        txn.nonce,
        None,
    )
    .map(ComputedTransactionHash::DeployAccount)
}

/// Computes invoke v0 account transaction hash based on [this formula](https://docs.starknet.io/documentation/architecture_and_concepts/Blocks/transactions/#v0_hash_calculation):
/// ```text=
/// invoke_v0_tx_hash = h("invoke", version, sender_address,
///     entry_point_selector, h(calldata), max_fee, chain_id)
/// ```
///
/// FIXME: SW should fix the formula in the docs
///
/// Where `h` is [Pedersen hash](https://docs.starknet.io/documentation/architecture_and_concepts/Hashing/hash-functions/#pedersen_hash)
fn compute_invoke_v0_hash(
    txn: &InvokeTransactionV0,
    chain_id: ChainId,
) -> Result<ComputedTransactionHash> {
    let list_hash = {
        let mut hh = HashChain::default();
        hh = txn.calldata.iter().fold(hh, |mut hh, call_param| {
            hh.update(call_param.0);
            hh
        });
        hh.finalize()
    };
    let h = compute_txn_hash_lt_0_8_0(
        b"invoke",
        txn.sender_address,
        Some(txn.entry_point_selector),
        list_hash,
        chain_id,
    )?;
    if h == txn.transaction_hash {
        Ok(h)
    } else {
        compute_txn_hash_ge_0_8_0(
            b"invoke",
            TransactionVersion::ZERO,
            txn.sender_address,
            Some(txn.entry_point_selector),
            list_hash,
            Some(txn.max_fee),
            chain_id,
            (),
            None,
        )
    }
    .map(ComputedTransactionHash::InvokeV0)
}

/// Computes invoke v1 transaction hash based on [this formula](https://docs.starknet.io/documentation/architecture_and_concepts/Blocks/transactions/#v1_hash_calculation):
/// ```text=
/// invoke_v1_tx_hash = h("invoke", version, sender_address,
///     0, h(calldata), max_fee, chain_id, nonce)
/// ```
///
/// Where `h` is [Pedersen hash](https://docs.starknet.io/documentation/architecture_and_concepts/Hashing/hash-functions/#pedersen_hash)
fn compute_invoke_v1_hash(
    txn: &InvokeTransactionV1,
    chain_id: ChainId,
) -> Result<ComputedTransactionHash> {
    compute_txn_hash_ge_0_8_0(
        b"invoke",
        TransactionVersion::ONE,
        txn.sender_address,
        None,
        {
            let mut hh = HashChain::default();
            hh = txn.calldata.iter().fold(hh, |mut hh, call_param| {
                hh.update(call_param.0);
                hh
            });
            hh.finalize()
        },
        Some(txn.max_fee),
        chain_id,
        txn.nonce,
        None,
    )
    .map(ComputedTransactionHash::InvokeV1)
}

/// Computes l1 handler transaction hash based on [this formula](https://docs.starknet.io/documentation/architecture_and_concepts/L1-L2_Communication/messaging-mechanism/#structure_and_hashing_l1-l2):
/// ```text=
/// l1_handler_tx_hash = h("l1_handler", version, contract_address,
///     entry_point_selector, h(calldata), 0, chain_id, nonce)
/// ```
///
/// FIXME: SW should fix the formula in the docs
///
/// Where `h` is [Pedersen hash](https://docs.starknet.io/documentation/architecture_and_concepts/Hashing/hash-functions/#pedersen_hash)
///
/// ## Important
///
/// Guarantees correct computation for Starknet **0.9.1** transactions onwards
fn compute_l1_handler_hash(
    txn: &L1HandlerTransaction,
    chain_id: ChainId,
) -> Result<ComputedTransactionHash> {
    compute_txn_hash_ge_0_8_0(
        b"l1_handler",
        txn.version,
        txn.contract_address,
        Some(txn.entry_point_selector),
        {
            let mut hh = HashChain::default();
            hh = txn.calldata.iter().fold(hh, |mut hh, call_param| {
                hh.update(call_param.0);
                hh
            });
            hh.finalize()
        },
        None,
        chain_id,
        txn.nonce,
        None,
    )
    .map(ComputedTransactionHash::L1Handler)
}

#[derive(Copy, Clone, Debug)]
enum NonceOrClassHash {
    Nonce(TransactionNonce),
    ClassHash(ClassHash),
    None,
}

impl From<TransactionNonce> for NonceOrClassHash {
    fn from(n: TransactionNonce) -> Self {
        Self::Nonce(n)
    }
}

impl From<ClassHash> for NonceOrClassHash {
    fn from(c: ClassHash) -> Self {
        Self::ClassHash(c)
    }
}

impl From<()> for NonceOrClassHash {
    fn from(_: ()) -> Self {
        Self::None
    }
}

/// _Generic_ compute transaction hash for transactions preceding Starknet 0.8.0
fn compute_txn_hash_lt_0_8_0(
    prefix: &[u8],
    address: ContractAddress,
    entry_point_selector: Option<EntryPoint>,
    list_hash: Felt,
    chain_id: ChainId,
) -> Result<StarknetTransactionHash> {
    let mut h = HashChain::default();
    h.update(Felt::from_be_slice(prefix).context("Converting prefix into felt")?);
    h.update(*address.get());
    h.update(entry_point_selector.map(|e| e.0).unwrap_or(Felt::ZERO));
    h.update(list_hash);
    h.update(chain_id.0);

    Ok(StarknetTransactionHash(h.finalize()))
}

/// _Generic_ compute transaction hash for transactions from Starknet 0.8.0 onwards
fn compute_txn_hash_ge_0_8_0(
    prefix: &[u8],
    version: TransactionVersion,
    address: ContractAddress,
    entry_point_selector: Option<EntryPoint>,
    list_hash: Felt,
    max_fee: Option<Fee>,
    chain_id: ChainId,
    nonce_or_class_hash: impl Into<NonceOrClassHash>,
    compiled_class_hash: Option<CasmHash>,
) -> Result<StarknetTransactionHash> {
    let mut h = HashChain::default();
    h.update(Felt::from_be_slice(prefix).context("Converting prefix into felt")?);
    h.update(Felt::from_be_slice(version.0.as_bytes()).context("Converting version into felt")?);
    h.update(*address.get());
    h.update(entry_point_selector.map(|e| e.0).unwrap_or(Felt::ZERO));
    h.update(list_hash);
    h.update(max_fee.map(|e| e.0).unwrap_or(Felt::ZERO));
    h.update(chain_id.0);

    match nonce_or_class_hash.into() {
        NonceOrClassHash::Nonce(nonce) => h.update(nonce.0),
        NonceOrClassHash::ClassHash(class_hash) => h.update(class_hash.0),
        NonceOrClassHash::None => {}
    }

    if let Some(compiled_class_hash) = compiled_class_hash {
        h.update(compiled_class_hash.0);
    }

    Ok(StarknetTransactionHash(h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::compute_transaction_hash;
    use crate::reply::Transaction;
    use pathfinder_common::ChainId;
    use starknet_gateway_test_fixtures::{v0_11_0, v0_8_2, v0_9_0};

    macro_rules! case {
        ($target:expr) => {{
            let line = line!();

            (
                serde_json::from_str::<Transaction>($target)
                    .expect(&format!("deserialization is Ok, line: {line}"))
                    .transaction
                    .expect(&format!("transaction is Some, line: {line}")),
                line,
            )
        }};
    }

    #[test]
    fn success() {
        // Block on testnet where starknet version was added (0.9.1)
        // https://alpha4.starknet.io/feeder_gateway/get_block?blockNumber=272881

        let declare_v0_231579 = case!(v0_9_0::transaction::DECLARE);
        let declare_v1_463319 = case!(v0_11_0::transaction::declare::v1::BLOCK_463319);
        let declare_v1_797215 = case!(v0_11_0::transaction::declare::v1::BLOCK_797215);
        let declare_v2_797220 = case!(v0_11_0::transaction::declare::v2::BLOCK_797220);

        let deploy_v0_genesis = case!(v0_11_0::transaction::deploy::v0::GENESIS);
        let deploy_v0_231579 = case!(v0_9_0::transaction::DEPLOY);
        let deploy_v1_485004 = case!(v0_11_0::transaction::deploy::v1::BLOCK_485004);

        let deploy_account_v1_375919 =
            case!(v0_11_0::transaction::deploy_account::v1::BLOCK_375919);
        let deploy_account_v1_797k = case!(v0_11_0::transaction::deploy_account::v1::BLOCK_797K);

        let invoke_v0_genesis = case!(v0_11_0::transaction::invoke::v0::GENESIS);
        let invoke_v0_21520 = case!(v0_8_2::transaction::INVOKE);
        let invoke_v0_231579 = case!(v0_9_0::transaction::INVOKE);
        let invoke_v1_420k = case!(v0_11_0::transaction::invoke::v1::BLOCK_420K);
        let invoke_v1_790k = case!(v0_11_0::transaction::invoke::v1::BLOCK_790K);

        let _l1_handler_v0_1564 = case!(v0_11_0::transaction::l1_handler::v0::BLOCK_1564);
        let l1_handler_v0_272866 = case!(v0_11_0::transaction::l1_handler::v0::BLOCK_272866);
        let l1_handler_v0_790k = case!(v0_11_0::transaction::l1_handler::v0::BLOCK_790K);

        [
            declare_v0_231579, // First declare
            declare_v1_463319,
            declare_v1_797215,
            declare_v2_797220,
            deploy_v0_genesis, // < 0.8.0
            deploy_v0_231579,  // >= 0.8.0
            deploy_v1_485004,  // Last deploy
            deploy_account_v1_375919,
            deploy_account_v1_797k,
            invoke_v0_genesis, // < 0.8.0
            invoke_v0_21520,   // < 0.8.0
            invoke_v0_231579,  // >= 0.8.0
            invoke_v1_420k,
            invoke_v1_790k,
            // _l1_handler_v0_1564, // < ? , always fails, even if fallback hash implementation is used (similarly to deploy and ignore)
            l1_handler_v0_272866, // < cairo 0.9.1
            l1_handler_v0_790k,
        ]
        .iter()
        .for_each(|(txn, line)| {
            assert_eq!(
                compute_transaction_hash(txn, ChainId::TESTNET)
                    .expect(&format!("line: {line}"))
                    .hash(),
                txn.hash(),
                "line: {line}"
            )
        })
    }
}

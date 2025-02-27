use super::Cheatcodes;
use crate::{
    abi::HEVMCalls,
    executor::backend::{
        error::{DatabaseError, DatabaseResult},
        DatabaseExt,
    },
    utils::h256_to_u256_be,
};
use bytes::{BufMut, Bytes, BytesMut};
use ethers::{
    abi::{AbiEncode, Address, ParamType, Token},
    core::k256::elliptic_curve::Curve,
    prelude::{
        k256::{ecdsa::SigningKey, elliptic_curve::bigint::Encoding, Secp256k1},
        LocalWallet, Signer, H160, *,
    },
    signers::{coins_bip39::English, MnemonicBuilder},
    types::{transaction::eip2718::TypedTransaction, NameOrAddress, H256, U256},
    utils,
};
use foundry_common::{fmt::*, RpcUrl};
use hex::FromHex;
use revm::{Account, CreateInputs, Database, EVMData, JournaledState, TransactTo};
use std::{collections::VecDeque, str::FromStr};
use tracing::trace;

const DEFAULT_DERIVATION_PATH_PREFIX: &str = "m/44'/60'/0'/0/";

/// Address of the default CREATE2 deployer 0x4e59b44847b379578588920ca78fbf26c0b4956c
pub const DEFAULT_CREATE2_DEPLOYER: H160 = H160([
    78, 89, 180, 72, 71, 179, 121, 87, 133, 136, 146, 12, 167, 143, 191, 38, 192, 180, 149, 108,
]);

/// Helps collecting transactions from different forks.
#[derive(Debug, Clone, Default)]
pub struct BroadcastableTransaction {
    pub rpc: Option<RpcUrl>,
    pub transaction: TypedTransaction,
}

pub type BroadcastableTransactions = VecDeque<BroadcastableTransaction>;

/// Configures the env for the transaction
pub fn configure_tx_env(env: &mut revm::Env, tx: &Transaction) {
    env.tx.caller = tx.from;
    env.tx.gas_limit = tx.gas.as_u64();
    env.tx.gas_price = tx.gas_price.unwrap_or_default();
    env.tx.gas_priority_fee = tx.max_priority_fee_per_gas;
    env.tx.nonce = Some(tx.nonce.as_u64());
    env.tx.access_list = tx
        .access_list
        .clone()
        .unwrap_or_default()
        .0
        .into_iter()
        .map(|item| (item.address, item.storage_keys.into_iter().map(h256_to_u256_be).collect()))
        .collect();
    env.tx.value = tx.value;
    env.tx.data = tx.input.0.clone();
    env.tx.transact_to = tx.to.map(TransactTo::Call).unwrap_or_else(TransactTo::create)
}

/// Applies the given function `f` to the `revm::Account` belonging to the `addr`
///
/// This will ensure the `Account` is loaded and `touched`, see [`JournaledState::touch`]
pub fn with_journaled_account<F, R, DB: Database>(
    journaled_state: &mut JournaledState,
    db: &mut DB,
    addr: Address,
    mut f: F,
) -> Result<R, DB::Error>
where
    F: FnMut(&mut Account) -> R,
{
    journaled_state.load_account(addr, db)?;
    journaled_state.touch(&addr);
    let account = journaled_state.state.get_mut(&addr).expect("account loaded;");
    Ok(f(account))
}

fn addr(private_key: U256) -> Result<Bytes, Bytes> {
    let key = parse_private_key(private_key)?;
    let addr = utils::secret_key_to_address(&key);
    Ok(addr.encode().into())
}

fn sign(private_key: U256, digest: H256, chain_id: U256) -> Result<Bytes, Bytes> {
    let key = parse_private_key(private_key)?;
    let wallet = LocalWallet::from(key).with_chain_id(chain_id.as_u64());

    // The `ecrecover` precompile does not use EIP-155
    let sig = wallet.sign_hash(digest).map_err(|err| err.to_string().encode())?;
    let recovered = sig.recover(digest).map_err(|err| err.to_string().encode())?;

    assert_eq!(recovered, wallet.address());

    let mut r_bytes = [0u8; 32];
    let mut s_bytes = [0u8; 32];
    sig.r.to_big_endian(&mut r_bytes);
    sig.s.to_big_endian(&mut s_bytes);

    Ok((sig.v, r_bytes, s_bytes).encode().into())
}

fn derive_key(mnemonic: &str, path: &str, index: u32) -> Result<Bytes, Bytes> {
    let derivation_path =
        if path.ends_with('/') { format!("{path}{index}") } else { format!("{path}/{index}") };

    let wallet = MnemonicBuilder::<English>::default()
        .phrase(mnemonic)
        .derivation_path(&derivation_path)
        .map_err(|err| err.to_string().encode())?
        .build()
        .map_err(|err| err.to_string().encode())?;

    let private_key = U256::from_big_endian(wallet.signer().to_bytes().as_slice());

    Ok(private_key.encode().into())
}

fn remember_key(state: &mut Cheatcodes, private_key: U256, chain_id: U256) -> Result<Bytes, Bytes> {
    let key = parse_private_key(private_key)?;
    let wallet = LocalWallet::from(key).with_chain_id(chain_id.as_u64());

    state.script_wallets.push(wallet.clone());

    Ok(wallet.address().encode().into())
}

pub fn parse(
    val: Vec<impl AsRef<str> + Clone>,
    r#type: ParamType,
    is_array: bool,
) -> Result<Bytes, Bytes> {
    let msg = format!("Failed to parse `{}` as type `{}`", &val[0].as_ref(), &r#type);
    value_to_abi(val, r#type, is_array).map_err(|e| format!("{msg}: {e}").encode().into())
}

pub fn apply<DB: Database>(
    state: &mut Cheatcodes,
    data: &mut EVMData<'_, DB>,
    call: &HEVMCalls,
) -> Option<Result<Bytes, Bytes>> {
    Some(match call {
        HEVMCalls::Addr(inner) => addr(inner.0),
        HEVMCalls::Sign(inner) => sign(inner.0, inner.1.into(), data.env.cfg.chain_id),
        HEVMCalls::DeriveKey0(inner) => {
            derive_key(&inner.0, DEFAULT_DERIVATION_PATH_PREFIX, inner.1)
        }
        HEVMCalls::DeriveKey1(inner) => derive_key(&inner.0, &inner.1, inner.2),
        HEVMCalls::RememberKey(inner) => remember_key(state, inner.0, data.env.cfg.chain_id),
        HEVMCalls::Label(inner) => {
            state.labels.insert(inner.0, inner.1.clone());
            Ok(Bytes::new())
        }
        HEVMCalls::ToString0(inner) => {
            Ok(ethers::abi::encode(&[Token::String(inner.0.pretty())]).into())
        }
        HEVMCalls::ToString1(inner) => {
            Ok(ethers::abi::encode(&[Token::String(inner.0.pretty())]).into())
        }
        HEVMCalls::ToString2(inner) => {
            Ok(ethers::abi::encode(&[Token::String(inner.0.pretty())]).into())
        }
        HEVMCalls::ToString3(inner) => {
            Ok(ethers::abi::encode(&[Token::String(inner.0.pretty())]).into())
        }
        HEVMCalls::ToString4(inner) => {
            Ok(ethers::abi::encode(&[Token::String(inner.0.pretty())]).into())
        }
        HEVMCalls::ToString5(inner) => {
            Ok(ethers::abi::encode(&[Token::String(inner.0.pretty())]).into())
        }
        HEVMCalls::ParseBytes(inner) => parse(vec![&inner.0], ParamType::Bytes, false),
        HEVMCalls::ParseAddress(inner) => parse(vec![&inner.0], ParamType::Address, false),
        HEVMCalls::ParseUint(inner) => parse(vec![&inner.0], ParamType::Uint(256), false),
        HEVMCalls::ParseInt(inner) => parse(vec![&inner.0], ParamType::Int(256), false),
        HEVMCalls::ParseBytes32(inner) => parse(vec![&inner.0], ParamType::FixedBytes(32), false),
        HEVMCalls::ParseBool(inner) => parse(vec![&inner.0], ParamType::Bool, false),
        _ => return None,
    })
}

pub fn process_create<DB>(
    broadcast_sender: Address,
    bytecode: Bytes,
    data: &mut EVMData<'_, DB>,
    call: &mut CreateInputs,
) -> DatabaseResult<(Bytes, Option<NameOrAddress>, u64)>
where
    DB: Database<Error = DatabaseError>,
{
    match call.scheme {
        revm::CreateScheme::Create => {
            call.caller = broadcast_sender;

            Ok((bytecode, None, data.journaled_state.account(broadcast_sender).info.nonce))
        }
        revm::CreateScheme::Create2 { salt } => {
            // Sanity checks for our CREATE2 deployer
            data.journaled_state.load_account(DEFAULT_CREATE2_DEPLOYER, data.db)?;

            let info = &data.journaled_state.account(DEFAULT_CREATE2_DEPLOYER).info;
            match &info.code {
                Some(code) => {
                    if code.is_empty() {
                        trace!(create2=?DEFAULT_CREATE2_DEPLOYER, "Empty Create 2 deployer code");
                        return Err(DatabaseError::MissingCreate2Deployer)
                    }
                }
                None => {
                    // forked db
                    trace!(create2=?DEFAULT_CREATE2_DEPLOYER, "Missing Create 2 deployer code");
                    if data.db.code_by_hash(info.code_hash)?.is_empty() {
                        return Err(DatabaseError::MissingCreate2Deployer)
                    }
                }
            }

            call.caller = DEFAULT_CREATE2_DEPLOYER;

            // We have to increment the nonce of the user address, since this create2 will be done
            // by the create2_deployer
            let account = data.journaled_state.state().get_mut(&broadcast_sender).unwrap();
            let nonce = account.info.nonce;
            account.info.nonce += 1;

            // Proxy deployer requires the data to be on the following format `salt.init_code`
            let mut calldata = BytesMut::with_capacity(32 + bytecode.len());
            let mut salt_bytes = [0u8; 32];
            salt.to_big_endian(&mut salt_bytes);
            calldata.put_slice(&salt_bytes);
            calldata.put(bytecode);

            Ok((calldata.freeze(), Some(NameOrAddress::Address(DEFAULT_CREATE2_DEPLOYER)), nonce))
        }
    }
}

/// Parses string values into the corresponding `ParamType` and returns it abi-encoded
///
/// If the value is a hex number then it tries to parse
///     1. as hex if `0x` prefix
///     2. as decimal string
///     3. as hex if 2. failed
pub fn value_to_abi(
    val: Vec<impl AsRef<str>>,
    r#type: ParamType,
    is_array: bool,
) -> Result<Bytes, String> {
    if is_array && val.len() == 1 && val.first().unwrap().as_ref().is_empty() {
        return Ok(abi::encode(&[Token::String(String::from(""))]).into())
    }
    let parse_bool = |v: &str| v.to_lowercase().parse::<bool>();
    let parse_uint = |v: &str| {
        if v.starts_with("0x") {
            v.parse::<U256>().map_err(|err| err.to_string())
        } else {
            match U256::from_dec_str(v) {
                Ok(val) => Ok(val),
                Err(dec_err) => v.parse::<U256>().map_err(|hex_err| {
                    format!(
                        "Failed to parse uint value `{v}` from hex and as decimal string {hex_err}, {dec_err}"
                    )
                }),
            }
        }
    };
    let parse_int = |v: &str| {
        // hex string may start with "0x", "+0x", or "-0x" which needs to be stripped for
        // `I256::from_hex_str`
        if v.starts_with("0x") || v.starts_with("+0x") || v.starts_with("-0x") {
            v.replacen("0x", "", 1).parse::<I256>().map_err(|err| err.to_string())
        } else {
            match I256::from_dec_str(v) {
                Ok(val) => Ok(val),
                Err(dec_err) => v.parse::<I256>().map_err(|hex_err| {
                    format!(
                        "Failed to parse int value `{v}` from hex and as decimal string {hex_err}, {dec_err}"
                    )
                }),
            }
        }
        .map(|v| v.into_raw())
    };
    let parse_address = |v: &str| Address::from_str(v);
    let parse_string = |v: &str| -> Result<String, ()> { Ok(v.to_string()) };
    let parse_bytes = |v: &str| Vec::from_hex(v.strip_prefix("0x").unwrap_or(v));

    val.iter()
        .map(AsRef::as_ref)
        .map(|v| match r#type {
            ParamType::Bool => parse_bool(v).map(Token::Bool).map_err(|e| e.to_string()),
            ParamType::Uint(256) => parse_uint(v).map(Token::Uint),
            ParamType::Int(256) => parse_int(v).map(Token::Int),
            ParamType::Address => parse_address(v).map(Token::Address).map_err(|e| e.to_string()),
            ParamType::FixedBytes(32) => {
                parse_bytes(v).map(Token::FixedBytes).map_err(|e| e.to_string())
            }
            ParamType::String => parse_string(v).map(Token::String).map_err(|_| "".to_string()),
            ParamType::Bytes => parse_bytes(v).map(Token::Bytes).map_err(|e| e.to_string()),
            _ => Err(format!("{type} is not a supported type")),
        })
        .collect::<Result<Vec<Token>, String>>()
        .map(|mut tokens| {
            if is_array {
                abi::encode(&[Token::Array(tokens)]).into()
            } else {
                abi::encode(&[tokens.remove(0)]).into()
            }
        })
}

pub fn parse_private_key(private_key: U256) -> Result<SigningKey, Bytes> {
    if private_key.is_zero() {
        return Err("Private key cannot be 0.".to_string().encode().into())
    }

    if private_key >= U256::from_big_endian(&Secp256k1::ORDER.to_be_bytes()) {
        return Err("Private key must be less than 115792089237316195423570985008687907852837564279074904382605163141518161494337 (the secp256k1 curve order).".to_string().encode().into());
    }

    let mut bytes: [u8; 32] = [0; 32];
    private_key.to_big_endian(&mut bytes);

    SigningKey::from_bytes((&bytes).into()).map_err(|err| err.to_string().encode().into())
}

// Determines if the gas limit on a given call was manually set in the script and should therefore
// not be overwritten by later estimations
pub fn check_if_fixed_gas_limit<DB: DatabaseExt>(
    data: &EVMData<'_, DB>,
    call_gas_limit: u64,
) -> bool {
    // If the gas limit was not set in the source code it is set to the estimated gas left at the
    // time of the call, which should be rather close to configured gas limit.
    // TODO: Find a way to reliably make this determination. (for example by
    // generating it in the compilation or evm simulation process)
    U256::from(data.env.tx.gas_limit) > data.env.block.gas_limit &&
        U256::from(call_gas_limit) <= data.env.block.gas_limit
        // Transfers in forge scripts seem to be estimated at 2300 by revm leading to "Intrinsic
        // gas too low" failure when simulated on chain
        && call_gas_limit > 2300
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::abi::AbiDecode;

    #[test]
    fn test_uint_env() {
        let pk = "0x10532cc9d0d992825c3f709c62c969748e317a549634fb2a9fa949326022e81f";
        let val: U256 = pk.parse().unwrap();
        let parsed = value_to_abi(vec![pk], ParamType::Uint(256), false).unwrap();
        let decoded = U256::decode(&parsed).unwrap();
        assert_eq!(val, decoded);

        let parsed =
            value_to_abi(vec![pk.strip_prefix("0x").unwrap()], ParamType::Uint(256), false)
                .unwrap();
        let decoded = U256::decode(&parsed).unwrap();
        assert_eq!(val, decoded);

        let parsed = value_to_abi(vec!["1337"], ParamType::Uint(256), false).unwrap();
        let decoded = U256::decode(&parsed).unwrap();
        assert_eq!(U256::from(1337u64), decoded);
    }

    #[test]
    fn test_int_env() {
        let val = U256::from(100u64);
        let parsed = value_to_abi(vec![format!("0x{val:x}")], ParamType::Int(256), false).unwrap();
        let decoded = I256::decode(parsed).unwrap();
        assert_eq!(val, decoded.try_into().unwrap());

        let parsed = value_to_abi(vec!["100"], ParamType::Int(256), false).unwrap();
        let decoded = I256::decode(parsed).unwrap();
        assert_eq!(U256::from(100u64), decoded.try_into().unwrap());
    }
}

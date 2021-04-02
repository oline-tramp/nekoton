use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use ed25519_dalek::PublicKey;
use ton_abi::TokenValue;
use ton_block::{Deserializable, GetRepresentationHash, MsgAddressInt, Serializable};
use ton_types::{BuilderData, SliceData, UInt256};

use super::{TransferAction, DEFAULT_WORKCHAIN};
use crate::contracts;
use crate::contracts::utils::*;
use crate::storage::keystore::{SignedMessage, UnsignedMessage};
use crate::utils::*;

pub fn prepare_deploy(
    public_key: &PublicKey,
    multisig_type: MultisigType,
    expire_at: u32,
) -> Result<Box<dyn UnsignedMessage>> {
    let state_init = prepare_state_init(public_key, multisig_type);
    let hash = state_init.hash().trust_me();

    let dst = MsgAddressInt::AddrStd(ton_block::MsgAddrStd {
        anycast: None,
        workchain_id: DEFAULT_WORKCHAIN,
        address: hash.into(),
    });

    let mut message =
        ton_block::Message::with_ext_in_header(ton_block::ExternalInboundMessageHeader {
            dst,
            ..Default::default()
        });

    message.set_state_init(state_init);

    let (function, input) =
        MessageBuilder::new(contracts::abi::safe_multisig_wallet(), "constructor")
            .trust_me()
            .arg(vec![UInt256::from(public_key.as_bytes())])
            .arg(1u8) // reqConfirms
            .build();

    let time = Utc::now().timestamp_millis() as u64;
    let mut header = HashMap::with_capacity(3);
    header.insert("time".to_string(), TokenValue::Time(time));
    header.insert("expire".to_string(), TokenValue::Expire(expire_at));
    header.insert(
        "pubkey".to_string(),
        TokenValue::PublicKey(Some(*public_key)),
    );

    let (payload, hash) = function
        .create_unsigned_call(&header, &input, false, true)
        .convert()?;

    Ok(Box::new(UnsignedMultisigMessage {
        hash,
        payload,
        expire_at,
        message,
    }))
}

pub fn prepare_transfer(
    public_key: &PublicKey,
    current_state: &ton_block::AccountStuff,
    destination: MsgAddressInt,
    amount: u64,
    bounce: bool,
    body: Option<SliceData>,
    expire_at: u32,
) -> Result<TransferAction> {
    match &current_state.storage.state {
        ton_block::AccountState::AccountFrozen(_) => {
            return Err(MultisigError::AccountIsFrozen.into())
        }
        ton_block::AccountState::AccountUninit => return Ok(TransferAction::DeployFirst),
        _ => {}
    };

    let message = ton_block::Message::with_ext_in_header(ton_block::ExternalInboundMessageHeader {
        dst: current_state.addr.clone(),
        ..Default::default()
    });

    let (function, input) =
        MessageBuilder::new(contracts::abi::safe_multisig_wallet(), "sendTransaction")
            .trust_me()
            .arg(destination)
            .arg(BigUint128(amount.into()))
            .arg(bounce)
            .arg(3u8) // flags
            .arg(body.unwrap_or_default().serialize().convert()?)
            .build();

    let time = Utc::now().timestamp_millis() as u64;
    let mut header = HashMap::with_capacity(3);
    header.insert("time".to_string(), TokenValue::Time(time));
    header.insert("expire".to_string(), TokenValue::Expire(expire_at));
    header.insert(
        "pubkey".to_string(),
        TokenValue::PublicKey(Some(*public_key)),
    );

    let (payload, hash) = function
        .create_unsigned_call(&header, &input, false, true)
        .convert()?;

    Ok(TransferAction::Sign(Box::new(UnsignedMultisigMessage {
        hash,
        payload,
        expire_at,
        message,
    })))
}

#[derive(Clone)]
struct UnsignedMultisigMessage {
    hash: Vec<u8>,
    payload: BuilderData,
    expire_at: u32,
    message: ton_block::Message,
}

impl UnsignedMessage for UnsignedMultisigMessage {
    fn hash(&self) -> &[u8] {
        self.hash.as_slice()
    }

    fn sign(&self, signature: &[u8; ed25519_dalek::SIGNATURE_LENGTH]) -> Result<SignedMessage> {
        let payload = self.payload.clone();
        let payload = ton_abi::Function::fill_sign(2, Some(signature), None, payload).convert()?;

        let mut message = self.message.clone();
        message.set_body(payload.into());

        Ok(SignedMessage {
            message,
            expire_at: self.expire_at,
        })
    }
}

crate::define_string_enum!(
    pub enum MultisigType {
        SafeMultisigWallet,
        SafeMultisigWallet24h,
        SetcodeMultisigWallet,
        SurfWallet,
    }
);

pub fn compute_contract_address(
    public_key: &PublicKey,
    multisig_type: MultisigType,
    workchain_id: i8,
) -> MsgAddressInt {
    let state_init = prepare_state_init(public_key, multisig_type);
    let hash = state_init.hash().trust_me();

    MsgAddressInt::AddrStd(ton_block::MsgAddrStd {
        anycast: None,
        workchain_id,
        address: hash.into(),
    })
}

fn prepare_state_init(public_key: &PublicKey, multisig_type: MultisigType) -> ton_block::StateInit {
    let mut code = match multisig_type {
        MultisigType::SafeMultisigWallet => contracts::code::safe_multisig_wallet(),
        MultisigType::SafeMultisigWallet24h => contracts::code::safe_multisig_wallet_24h(),
        MultisigType::SetcodeMultisigWallet => contracts::code::setcode_multisig_wallet(),
        MultisigType::SurfWallet => contracts::code::surf_wallet(),
    }
    .into();

    let mut state_init = ton_block::StateInit::construct_from(&mut code).trust_me();

    let new_data = ton_abi::Contract::insert_pubkey(
        state_init.data.clone().unwrap_or_default().into(),
        public_key.as_bytes(),
    )
    .trust_me();
    state_init.set_data(new_data.into_cell());

    state_init
}

#[derive(thiserror::Error, Debug)]
enum MultisigError {
    #[error("Account is frozen")]
    AccountIsFrozen,
}
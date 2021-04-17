use std::collections::hash_map::{self, HashMap};
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use futures::stream::{FuturesUnordered, StreamExt};
use serde::Deserialize;
use tokio::sync::{RwLock, Semaphore};
use ton_block::MsgAddressInt;

use super::models::TokenWalletVersion;
use crate::core::token_wallet::{RootTokenContractState, TokenWalletContractState};
use crate::external::Storage;
use crate::transport::models::{ContractState, ExistingContract};
use crate::transport::Transport;
use crate::utils::{NoFailure, TrustMe};

const STORAGE_OWNERS_CACHE: &str = "owners_cache";

/// Stores a map to resolve owner's wallet address from token wallet address
pub struct OwnersCache {
    storage: Arc<dyn Storage>,
    transport: Arc<dyn Transport>,
    owners: RwLock<HashMap<MsgAddressInt, MsgAddressInt>>,
    token_contract_states: RwLock<HashMap<MsgAddressInt, (ExistingContract, TokenWalletVersion)>>,
    resolver_semaphore: Semaphore,
}

impl OwnersCache {
    pub async fn load(
        storage: Arc<dyn Storage>,
        transport: Arc<dyn Transport>,
        concurrent_resolvers: usize,
    ) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(transparent)]
        struct OwnersMap(Vec<OwnersMapItem>);
        #[derive(Deserialize)]
        struct OwnersMapItem(String, String);

        let data = match storage.get(STORAGE_OWNERS_CACHE).await? {
            Some(data) => serde_json::from_str::<OwnersMap>(&data)?.0,
            None => Default::default(),
        }
        .into_iter()
        .map(|OwnersMapItem(token_wallet, owner_wallet)| {
            let token_wallet = MsgAddressInt::from_str(&token_wallet).convert()?;
            let owner_wallet = MsgAddressInt::from_str(&owner_wallet).convert()?;
            Result::<_, anyhow::Error>::Ok((token_wallet, owner_wallet))
        })
        .collect::<Result<HashMap<_, _>, _>>()?;

        Ok(Self {
            storage,
            transport,
            owners: RwLock::new(data),
            token_contract_states: Default::default(),
            resolver_semaphore: Semaphore::new(concurrent_resolvers),
        })
    }

    pub async fn load_unchecked(
        storage: Arc<dyn Storage>,
        transport: Arc<dyn Transport>,
        concurrent_resolvers: usize,
    ) -> Self {
        Self::load(storage.clone(), transport.clone(), concurrent_resolvers)
            .await
            .unwrap_or_else(|_| Self {
                storage,
                transport,
                owners: Default::default(),
                token_contract_states: Default::default(),
                resolver_semaphore: Semaphore::new(concurrent_resolvers),
            })
    }

    pub async fn check_recipient_wallet(
        &self,
        root_token_contract: &MsgAddressInt,
        owner_wallet: &MsgAddressInt,
    ) -> Result<RecipientWallet> {
        let mut token_contract_states = self.token_contract_states.write().await;
        match token_contract_states.entry(root_token_contract.clone()) {
            hash_map::Entry::Occupied(entry) => {
                check_token_wallet(
                    self.transport.as_ref(),
                    &self.owners,
                    entry.get(),
                    owner_wallet,
                )
                .await
            }
            hash_map::Entry::Vacant(entry) => {
                let state = match self
                    .transport
                    .get_contract_state(root_token_contract)
                    .await?
                {
                    ContractState::Exists(state) => state,
                    ContractState::NotExists => {
                        return Err(OwnersCacheError::InvalidRootTokenContract.into())
                    }
                };

                let version = RootTokenContractState(&state).guess_details()?.version;

                check_token_wallet(
                    self.transport.as_ref(),
                    &self.owners,
                    entry.insert((state, version)),
                    owner_wallet,
                )
                .await
            }
        }
    }

    /// Returns map with token wallet as key and its owner as value.
    /// Populates the cache during the search
    pub async fn resolve_owners(
        &self,
        token_wallets: &[MsgAddressInt],
    ) -> HashMap<MsgAddressInt, MsgAddressInt> {
        let semaphore = &self.resolver_semaphore;
        let transport = self.transport.as_ref();
        let owners = &self.owners;

        let token_wallets = token_wallets.iter().collect::<HashSet<_>>();

        token_wallets
            .into_iter()
            .map(|token_wallet| async move {
                if let Some(owner) = owners.read().await.get(token_wallet) {
                    return Some((token_wallet.clone(), owner.clone()));
                }

                let _permit = semaphore.acquire().await.ok()?;
                let contract_state = match transport.get_contract_state(token_wallet).await.ok()? {
                    ContractState::Exists(state) => state,
                    ContractState::NotExists => return None,
                };
                let state = TokenWalletContractState(&contract_state);
                let version = state.get_version().ok()?;
                let details = state.get_details(version).ok()?;

                owners
                    .write()
                    .await
                    .insert(token_wallet.clone(), details.owner_address.clone());

                Some((token_wallet.clone(), details.owner_address))
            })
            .collect::<FuturesUnordered<_>>()
            .filter_map(|value| async move { value })
            .collect()
            .await
    }

    pub async fn get_owner(&self, token_wallet: &MsgAddressInt) -> Option<MsgAddressInt> {
        self.owners.read().await.get(token_wallet).cloned()
    }

    pub async fn add_entry(&self, token_wallet: MsgAddressInt, owner_wallet: MsgAddressInt) {
        let mut owners = self.owners.write().await;
        owners.insert(token_wallet, owner_wallet);
        self.save(&owners);
    }

    pub async fn add_owners_list<I>(&self, new_owners: I)
    where
        I: Iterator<Item = (MsgAddressInt, MsgAddressInt)>,
    {
        let mut owners = self.owners.write().await;
        owners.extend(new_owners);
        self.save(&*owners);
    }

    fn save(&self, owners: &HashMap<MsgAddressInt, MsgAddressInt>) {
        struct OwnersMap<'a>(&'a HashMap<MsgAddressInt, MsgAddressInt>);
        struct OwnersMapItem<'a>(&'a MsgAddressInt, &'a MsgAddressInt);

        impl<'a> serde::Serialize for OwnersMapItem<'a> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                use serde::ser::SerializeTuple;

                let mut tuple = serializer.serialize_tuple(2)?;
                tuple.serialize_element(&self.0.to_string())?;
                tuple.serialize_element(&self.1.to_string())?;
                tuple.end()
            }
        }

        impl<'a> serde::Serialize for OwnersMap<'a> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                use serde::ser::SerializeSeq;
                let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
                for (token_wallet, owner_wallet) in self.0.iter() {
                    seq.serialize_element(&OwnersMapItem(token_wallet, owner_wallet))?;
                }
                seq.end()
            }
        }

        let data = serde_json::to_string(&OwnersMap(owners)).trust_me();
        self.storage.set_unchecked(STORAGE_OWNERS_CACHE, &data);
    }
}

async fn check_token_wallet<'a>(
    transport: &'a dyn Transport,
    owners: &'a RwLock<OwnersMap>,
    (state, version): &'a (ExistingContract, TokenWalletVersion),
    owner_wallet: &'a MsgAddressInt,
) -> Result<RecipientWallet> {
    let token_wallet = RootTokenContractState(state).get_wallet_address(*version, &owner_wallet)?;

    {
        let mut owners = owners.write().await;
        owners.insert(token_wallet.clone(), owner_wallet.clone());
    }

    Ok(match transport.get_contract_state(&token_wallet).await? {
        ContractState::NotExists => RecipientWallet::NotExists,
        ContractState::Exists(_) => RecipientWallet::Exists(token_wallet),
    })
}

pub enum RecipientWallet {
    NotExists,
    Exists(MsgAddressInt),
}

#[derive(thiserror::Error, Debug)]
pub enum OwnersCacheError {
    #[error("Invalid root token contract")]
    InvalidRootTokenContract,
}

type OwnersMap = HashMap<MsgAddressInt, MsgAddressInt>;
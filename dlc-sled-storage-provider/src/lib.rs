//! # dlc-sled-storage-provider
//! Storage provider for dlc-manager using sled as underlying storage.

#![crate_name = "dlc_sled_storage_provider"]
// Coding conventions
#![deny(non_upper_case_globals)]
#![deny(non_camel_case_types)]
#![deny(non_snake_case)]
#![deny(unused_mut)]
#![deny(dead_code)]
#![deny(unused_imports)]
#![deny(missing_docs)]

extern crate dlc_manager;
extern crate sled;

use dlc_manager::chain_monitor::ChainMonitor;
use dlc_manager::channel::accepted_channel::AcceptedChannel;
use dlc_manager::channel::offered_channel::OfferedChannel;
use dlc_manager::channel::signed_channel::{SignedChannel, SignedChannelStateType};
use dlc_manager::channel::{Channel, FailedAccept, FailedSign};
use dlc_manager::contract::accepted_contract::AcceptedContract;
use dlc_manager::contract::offered_contract::OfferedContract;
use dlc_manager::contract::ser::Serializable;
use dlc_manager::contract::signed_contract::SignedContract;
use dlc_manager::contract::{ClosedContract, Contract, FailedAcceptContract, FailedSignContract};
use dlc_manager::{error::Error, ContractId, Storage};
use sled::transaction::UnabortableTransactionError;
use sled::{Db, Tree};
use std::convert::TryInto;
use std::io::{Cursor, Read};

const CONTRACT_TREE: u8 = 1;
const CHANNEL_TREE: u8 = 2;
const CHAIN_MONITOR_TREE: u8 = 3;
const CHAIN_MONITOR_KEY: u8 = 4;

/// Implementation of Storage interface using the sled DB backend.
pub struct SledStorageProvider {
    db: Db,
}

macro_rules! convertible_enum {
    (enum $name:ident {
        $($vname:ident $(= $val:expr)?,)*;
        $($tname:ident $(= $tval:expr)?,)*
    }, $input:ident) => {
        #[derive(Debug)]
        enum $name {
            $($vname $(= $val)?,)*
            $($tname $(= $tval)?,)*
        }

        impl From<$name> for u8 {
            fn from(prefix: $name) -> u8 {
                prefix as u8
            }
        }

        impl std::convert::TryFrom<u8> for $name {
            type Error = Error;

            fn try_from(v: u8) -> Result<Self, Self::Error> {
                match v {
                    $(x if x == u8::from($name::$vname) => Ok($name::$vname),)*
                    $(x if x == u8::from($name::$tname) => Ok($name::$tname),)*
                    _ => Err(Error::StorageError("Unknown prefix".to_string())),
                }
            }
        }

        impl $name {
            fn get_prefix(input: &$input) -> u8 {
                let prefix = match input {
                    $($input::$vname(_) => $name::$vname,)*
                    $($input::$tname{..} => $name::$tname,)*
                };
                prefix.into()
            }
        }
    }
}

convertible_enum!(
    enum ContractPrefix {
        Offered = 1,
        Accepted,
        Signed,
        Confirmed,
        Closed,
        FailedAccept,
        FailedSign,
        Refunded,;
    },
    Contract
);

convertible_enum!(
    enum ChannelPrefix {
        Offered = 100,
        Accepted,
        Signed,
        FailedAccept,
        FailedSign,;
    },
    Channel
);

convertible_enum!(
    enum SignedChannelPrefix {;
        Established = 1,
        SettledOffered,
        SettledReceived,
        SettledAccepted,
        SettledConfirmed,
        Settled,
        SettleClosing,
        Closing,
        Closed,
        CounterClosed,
        ClosedPunished,
        CollaborativeCloseOffered,
        CollaborativelyClosed,
        RenewAccepted,
        RenewOffered,
        RenewConfirmed,
    },
    SignedChannelStateType
);

fn to_storage_error<T>(e: T) -> Error
where
    T: std::fmt::Display,
{
    Error::StorageError(e.to_string())
}

impl SledStorageProvider {
    /// Creates a new instance of a SledStorageProvider.
    pub fn new(path: &str) -> Result<Self, sled::Error> {
        Ok(SledStorageProvider {
            db: sled::open(path)?,
        })
    }

    fn get_data_with_prefix<T: Serializable>(
        &self,
        tree: &Tree,
        prefix: &[u8],
        consume: Option<u64>,
    ) -> Result<Vec<T>, Error> {
        let iter = tree.iter();
        iter.values()
            .filter_map(|res| {
                let value = res.unwrap();
                let mut cursor = Cursor::new(&value);
                let mut pref = vec![0u8; prefix.len()];
                cursor.read_exact(&mut pref).expect("Error reading prefix");
                if pref == prefix {
                    if let Some(c) = consume {
                        cursor.set_position(cursor.position() + c);
                    }
                    Some(Ok(T::deserialize(&mut cursor).ok()?))
                } else {
                    None
                }
            })
            .collect()
    }

    fn open_tree(&self, tree_id: &[u8; 1]) -> Result<Tree, Error> {
        self.db
            .open_tree(tree_id)
            .map_err(|e| Error::StorageError(format!("Error opening contract tree: {}", e)))
    }

    fn contract_tree(&self) -> Result<Tree, Error> {
        self.open_tree(&[CONTRACT_TREE])
    }

    fn channel_tree(&self) -> Result<Tree, Error> {
        self.open_tree(&[CHANNEL_TREE])
    }
}

impl Storage for SledStorageProvider {
    fn get_contract(&self, contract_id: &ContractId) -> Result<Option<Contract>, Error> {
        match self
            .contract_tree()?
            .get(contract_id)
            .map_err(to_storage_error)?
        {
            Some(res) => Ok(Some(deserialize_contract(&res)?)),
            None => Ok(None),
        }
    }

    fn get_contracts(&self) -> Result<Vec<Contract>, Error> {
        self.contract_tree()?
            .iter()
            .values()
            .map(|x| deserialize_contract(&x.unwrap()))
            .collect::<Result<Vec<Contract>, Error>>()
    }

    fn create_contract(&mut self, contract: &OfferedContract) -> Result<(), Error> {
        let serialized = serialize_contract(&Contract::Offered(contract.clone()))?;
        self.contract_tree()?
            .insert(&contract.id, serialized)
            .map_err(to_storage_error)?;
        Ok(())
    }

    fn delete_contract(&mut self, contract_id: &ContractId) -> Result<(), Error> {
        self.contract_tree()?
            .remove(&contract_id)
            .map_err(to_storage_error)?;
        Ok(())
    }

    fn update_contract(&mut self, contract: &Contract) -> Result<(), Error> {
        let serialized = serialize_contract(contract)?;
        self.contract_tree()?
            .transaction::<_, _, UnabortableTransactionError>(|db| {
                match contract {
                    a @ Contract::Accepted(_) | a @ Contract::Signed(_) => {
                        db.remove(&a.get_temporary_id())?;
                    }
                    _ => {}
                };

                db.insert(&contract.get_id(), serialized.clone())?;
                Ok(())
            })
            .map_err(to_storage_error)?;
        Ok(())
    }

    fn get_signed_contracts(&self) -> Result<Vec<SignedContract>, Error> {
        self.get_data_with_prefix(
            &self.contract_tree()?,
            &[ContractPrefix::Signed.into()],
            None,
        )
    }

    fn get_confirmed_contracts(&self) -> Result<Vec<SignedContract>, Error> {
        self.get_data_with_prefix(
            &self.contract_tree()?,
            &[ContractPrefix::Confirmed.into()],
            None,
        )
    }

    fn get_contract_offers(&self) -> Result<Vec<OfferedContract>, Error> {
        self.get_data_with_prefix(
            &self.contract_tree()?,
            &[ContractPrefix::Offered.into()],
            None,
        )
    }

    fn upsert_channel(
        &mut self,
        channel: Channel,
        contract: Option<Contract>,
    ) -> Result<(), Error> {
        let serialized = serialize_channel(&channel)?;
        let serialized_contract = match contract.as_ref() {
            Some(c) => Some(serialize_contract(c)?),
            None => None,
        };
        self.channel_tree()?
            .transaction::<_, _, UnabortableTransactionError>(|db| {
                match &channel {
                    a @ Channel::Accepted(_) | a @ Channel::Signed(_) => {
                        db.remove(&a.get_temporary_id())?;
                    }
                    _ => {}
                };

                db.insert(&channel.get_id(), serialized.clone())?;

                if let Some(c) = contract.as_ref() {
                    insert_contract(
                        db,
                        serialized_contract
                            .clone()
                            .expect("to have the serialized version"),
                        c,
                    )?;
                }

                Ok(())
            })
            .map_err(to_storage_error)?;
        Ok(())
    }

    fn delete_channel(&mut self, channel_id: &dlc_manager::ChannelId) -> Result<(), Error> {
        self.channel_tree()?
            .remove(channel_id)
            .map_err(to_storage_error)?;
        Ok(())
    }

    fn get_channel(&self, channel_id: &dlc_manager::ChannelId) -> Result<Option<Channel>, Error> {
        match self
            .channel_tree()?
            .get(channel_id)
            .map_err(to_storage_error)?
        {
            Some(res) => Ok(Some(deserialize_channel(&res)?)),
            None => Ok(None),
        }
    }

    fn get_signed_channels(
        &self,
        channel_state: Option<SignedChannelStateType>,
    ) -> Result<Vec<SignedChannel>, Error> {
        let (prefix, consume) = if let Some(state) = &channel_state {
            (
                vec![
                    ChannelPrefix::Signed.into(),
                    SignedChannelPrefix::get_prefix(state),
                ],
                None,
            )
        } else {
            (vec![ChannelPrefix::Signed.into()], Some(1))
        };

        self.get_data_with_prefix(&self.channel_tree()?, &prefix, consume)
    }

    fn get_offered_channels(&self) -> Result<Vec<OfferedChannel>, Error> {
        self.get_data_with_prefix(
            &self.channel_tree()?,
            &[ChannelPrefix::Offered.into()],
            None,
        )
    }

    fn persist_chain_monitor(&mut self, monitor: &ChainMonitor) -> Result<(), Error> {
        self.open_tree(&[CHAIN_MONITOR_TREE])?
            .insert(&[CHAIN_MONITOR_KEY], monitor.serialize()?)
            .map_err(|e| Error::StorageError(format!("Error writing chain monitor: {}", e)))?;
        Ok(())
    }
    fn get_chain_monitor(&self) -> Result<Option<ChainMonitor>, dlc_manager::error::Error> {
        let serialized = self
            .open_tree(&[CHAIN_MONITOR_TREE])?
            .get(&[CHAIN_MONITOR_KEY])
            .map_err(|e| Error::StorageError(format!("Error reading chain monitor: {}", e)))?;
        let deserialized = match serialized {
            Some(s) => Some(
                ChainMonitor::deserialize(&mut ::std::io::Cursor::new(s))
                    .map_err(to_storage_error)?,
            ),
            None => None,
        };
        Ok(deserialized)
    }
}

fn insert_contract(
    db: &sled::transaction::TransactionalTree,
    serialized: Vec<u8>,
    contract: &Contract,
) -> Result<(), UnabortableTransactionError> {
    match contract {
        a @ Contract::Accepted(_) | a @ Contract::Signed(_) => {
            db.remove(&a.get_temporary_id())?;
        }
        _ => {}
    };

    db.insert(&contract.get_id(), serialized)?;
    Ok(())
}

fn serialize_contract(contract: &Contract) -> Result<Vec<u8>, ::std::io::Error> {
    let serialized = match contract {
        Contract::Offered(o) => o.serialize(),
        Contract::Accepted(o) => o.serialize(),
        Contract::Signed(o) | Contract::Confirmed(o) | Contract::Refunded(o) => o.serialize(),
        Contract::FailedAccept(c) => c.serialize(),
        Contract::FailedSign(c) => c.serialize(),
        Contract::Closed(c) => c.serialize(),
    };
    let mut serialized = serialized?;
    let mut res = Vec::with_capacity(serialized.len() + 1);
    res.push(ContractPrefix::get_prefix(contract));
    res.append(&mut serialized);
    Ok(res)
}

fn deserialize_contract(buff: &sled::IVec) -> Result<Contract, Error> {
    let mut cursor = ::std::io::Cursor::new(buff);
    let mut prefix = [0u8; 1];
    cursor.read_exact(&mut prefix)?;
    let contract_prefix: ContractPrefix = prefix[0].try_into()?;
    let contract = match contract_prefix {
        ContractPrefix::Offered => {
            Contract::Offered(OfferedContract::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ContractPrefix::Accepted => Contract::Accepted(
            AcceptedContract::deserialize(&mut cursor).map_err(to_storage_error)?,
        ),
        ContractPrefix::Signed => {
            Contract::Signed(SignedContract::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ContractPrefix::Confirmed => {
            Contract::Confirmed(SignedContract::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ContractPrefix::Closed => {
            Contract::Closed(ClosedContract::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ContractPrefix::FailedAccept => Contract::FailedAccept(
            FailedAcceptContract::deserialize(&mut cursor).map_err(to_storage_error)?,
        ),
        ContractPrefix::FailedSign => Contract::FailedSign(
            FailedSignContract::deserialize(&mut cursor).map_err(to_storage_error)?,
        ),
        ContractPrefix::Refunded => {
            Contract::Refunded(SignedContract::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
    };
    Ok(contract)
}

fn serialize_channel(channel: &Channel) -> Result<Vec<u8>, ::std::io::Error> {
    let serialized = match channel {
        Channel::Offered(o) => o.serialize(),
        Channel::Accepted(a) => a.serialize(),
        Channel::Signed(s) => s.serialize(),
        Channel::FailedAccept(f) => f.serialize(),
        Channel::FailedSign(f) => f.serialize(),
    };
    let mut serialized = serialized?;
    let mut res = Vec::with_capacity(serialized.len() + 1);
    res.push(ChannelPrefix::get_prefix(channel));
    if let Channel::Signed(s) = channel {
        res.push(SignedChannelPrefix::get_prefix(&s.state.get_type()))
    }
    res.append(&mut serialized);
    Ok(res)
}

fn deserialize_channel(buff: &sled::IVec) -> Result<Channel, Error> {
    let mut cursor = ::std::io::Cursor::new(buff);
    let mut prefix = [0u8; 1];
    cursor.read_exact(&mut prefix)?;
    let channel_prefix: ChannelPrefix = prefix[0].try_into()?;
    let channel = match channel_prefix {
        ChannelPrefix::Offered => {
            Channel::Offered(OfferedChannel::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::Accepted => {
            Channel::Accepted(AcceptedChannel::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::Signed => {
            // Skip the channel state prefix.
            cursor.set_position(cursor.position() + 1);
            Channel::Signed(SignedChannel::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::FailedAccept => {
            Channel::FailedAccept(FailedAccept::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::FailedSign => {
            Channel::FailedSign(FailedSign::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
    };
    Ok(channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! sled_test {
        ($name: ident, $body: expr) => {
            #[test]
            fn $name() {
                let path = format!("{}{}", "test_files/sleddb/", std::stringify!($name));
                {
                    let storage = SledStorageProvider::new(&path).expect("Error opening sled DB");
                    $body(storage);
                }
                std::fs::remove_dir_all(path).unwrap();
            }
        };
    }

    fn deserialize_contract<T>(serialized: &[u8]) -> T
    where
        T: Serializable,
    {
        let mut cursor = std::io::Cursor::new(&serialized);
        T::deserialize(&mut cursor).unwrap()
    }

    sled_test!(
        create_contract_can_be_retrieved,
        |mut storage: SledStorageProvider| {
            let serialized = include_bytes!("../test_files/Offered");
            let contract = deserialize_contract(serialized);

            storage
                .create_contract(&contract)
                .expect("Error creating contract");

            let retrieved = storage
                .get_contract(&contract.id)
                .expect("Error retrieving contract.");

            if let Some(Contract::Offered(retrieved_offer)) = retrieved {
                assert_eq!(serialized[..], retrieved_offer.serialize().unwrap()[..]);
            } else {
                unreachable!();
            }
        }
    );

    sled_test!(
        update_contract_is_updated,
        |mut storage: SledStorageProvider| {
            let serialized = include_bytes!("../test_files/Offered");
            let offered_contract = deserialize_contract(serialized);
            let serialized = include_bytes!("../test_files/Accepted");
            let accepted_contract = deserialize_contract(serialized);
            let accepted_contract = Contract::Accepted(accepted_contract);

            storage
                .create_contract(&offered_contract)
                .expect("Error creating contract");

            storage
                .update_contract(&accepted_contract)
                .expect("Error updating contract.");
            let retrieved = storage
                .get_contract(&accepted_contract.get_id())
                .expect("Error retrieving contract.");

            if let Some(Contract::Accepted(_)) = retrieved {
            } else {
                unreachable!();
            }
        }
    );

    sled_test!(
        delete_contract_is_deleted,
        |mut storage: SledStorageProvider| {
            let serialized = include_bytes!("../test_files/Offered");
            let contract = deserialize_contract(serialized);
            storage
                .create_contract(&contract)
                .expect("Error creating contract");

            storage
                .delete_contract(&contract.id)
                .expect("Error deleting contract");

            assert!(storage
                .get_contract(&contract.id)
                .expect("Error querying contract")
                .is_none());
        }
    );

    fn insert_offered_signed_and_confirmed(storage: &mut SledStorageProvider) {
        let serialized = include_bytes!("../test_files/Offered");
        let offered_contract = deserialize_contract(serialized);
        storage
            .create_contract(&offered_contract)
            .expect("Error creating contract");

        let serialized = include_bytes!("../test_files/Signed");
        let signed_contract = Contract::Signed(deserialize_contract(serialized));
        storage
            .update_contract(&signed_contract)
            .expect("Error creating contract");
        let serialized = include_bytes!("../test_files/Signed1");
        let signed_contract = Contract::Signed(deserialize_contract(serialized));
        storage
            .update_contract(&signed_contract)
            .expect("Error creating contract");

        let serialized = include_bytes!("../test_files/Confirmed");
        let confirmed_contract = Contract::Confirmed(deserialize_contract(serialized));
        storage
            .update_contract(&confirmed_contract)
            .expect("Error creating contract");
        let serialized = include_bytes!("../test_files/Confirmed1");
        let confirmed_contract = Contract::Confirmed(deserialize_contract(serialized));
        storage
            .update_contract(&confirmed_contract)
            .expect("Error creating contract");
    }

    sled_test!(
        get_signed_contracts_only_signed,
        |mut storage: SledStorageProvider| {
            insert_offered_signed_and_confirmed(&mut storage);

            let signed_contracts = storage
                .get_signed_contracts()
                .expect("Error retrieving signed contracts");

            assert_eq!(2, signed_contracts.len());
        }
    );

    sled_test!(
        get_confirmed_contracts_only_confirmed,
        |mut storage: SledStorageProvider| {
            println!("1");
            insert_offered_signed_and_confirmed(&mut storage);
            println!("1");

            let confirmed_contracts = storage
                .get_confirmed_contracts()
                .expect("Error retrieving signed contracts");

            assert_eq!(2, confirmed_contracts.len());
        }
    );

    sled_test!(
        get_offered_contracts_only_offered,
        |mut storage: SledStorageProvider| {
            insert_offered_signed_and_confirmed(&mut storage);

            let offered_contracts = storage
                .get_contract_offers()
                .expect("Error retrieving signed contracts");

            assert_eq!(1, offered_contracts.len());
        }
    );
}
